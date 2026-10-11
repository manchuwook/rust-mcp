use anyhow::{Context, Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::analyzer::edit::{self, EditSummary};
use crate::analyzer::protocol::*;

/// How long to wait for the response to a single request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// How long to wait for rust-analyzer to finish loading the workspace.
const READY_TIMEOUT: Duration = Duration::from_secs(180);
/// LSP error code for "the document changed while the request was running".
const CONTENT_MODIFIED: i64 = -32801;
const CONTENT_MODIFIED_RETRIES: u32 = 5;

fn get_rust_analyzer_path() -> String {
    if let Ok(path) = std::env::var("RUST_ANALYZER_PATH") {
        return path;
    }

    let exe = format!("rust-analyzer{}", std::env::consts::EXE_SUFFIX);

    // Default to ~/.cargo/bin/rust-analyzer (USERPROFILE covers Windows, where
    // HOME is usually unset).
    let cargo_homes = std::env::var("CARGO_HOME").ok().into_iter().chain(
        ["HOME", "USERPROFILE"]
            .into_iter()
            .filter_map(|var| std::env::var(var).ok())
            .map(|home| format!("{home}/.cargo")),
    );
    for cargo_home in cargo_homes {
        let candidate = Path::new(&cargo_home).join("bin").join(&exe);
        if candidate.is_file() {
            return candidate.to_string_lossy().into_owned();
        }
    }

    // Fall back to whatever is on PATH.
    exe
}

/// rust-analyzer settings, sent both as `initializationOptions` and in reply to
/// `workspace/configuration`.
fn analyzer_config() -> Value {
    json!({
        // Compiler diagnostics come from running cargo ourselves (see
        // `tools::cargo`). Letting rust-analyzer also run `cargo check` in the
        // background would only contend for the build directory lock.
        "checkOnSave": false
    })
}

/// An error response from rust-analyzer.
#[derive(Debug)]
struct LspError {
    method: String,
    code: i64,
    message: String,
}

impl fmt::Display for LspError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "rust-analyzer rejected {}: {} (code {})",
            self.method, self.message, self.code
        )
    }
}

impl std::error::Error for LspError {}

struct OpenDocument {
    path: PathBuf,
    version: i64,
    text: String,
}

/// Read one `Content-Length`-framed message. Returns `None` at end of stream.
async fn read_message(reader: &mut BufReader<ChildStdout>) -> Result<Option<Value>> {
    let mut content_length: Option<usize> = None;
    loop {
        let mut line = String::new();
        let bytes_read = reader.read_line(&mut line).await?;

        // A zero-byte read means EOF: rust-analyzer closed its stdout (e.g. it
        // crashed or exited).
        if bytes_read == 0 {
            return Ok(None);
        }

        // Blank line (CRLF, or bare LF) terminates the headers.
        if line.trim_end_matches(['\r', '\n']).is_empty() {
            break;
        }

        if let Some(stripped) = line.strip_prefix("Content-Length:") {
            content_length = Some(stripped.trim().parse()?);
        }
    }

    let length = content_length.ok_or_else(|| anyhow!("message without Content-Length header"))?;
    let mut content = vec![0u8; length];
    reader.read_exact(&mut content).await?;
    Ok(Some(serde_json::from_slice(&content)?))
}

pub struct RustAnalyzerClient {
    process: Option<Child>,
    stdin: Option<ChildStdin>,
    // Complete messages from rust-analyzer, read by a background task so that
    // waiting on one with a timeout can never tear a message in half.
    incoming: Option<mpsc::UnboundedReceiver<Value>>,
    request_id: u64,
    initialized: bool,
    // False while rust-analyzer is still loading the workspace.
    quiescent: bool,
    // Documents we have sent `didOpen` for, keyed by `path_key`.
    open_documents: HashMap<String, OpenDocument>,
    // Diagnostics pushed by rust-analyzer, keyed by `path_key`.
    published_diagnostics: HashMap<String, Vec<Value>>,
}

impl Default for RustAnalyzerClient {
    fn default() -> Self {
        Self::new()
    }
}

impl RustAnalyzerClient {
    pub fn new() -> Self {
        Self {
            process: None,
            stdin: None,
            incoming: None,
            request_id: 0,
            initialized: false,
            quiescent: false,
            open_documents: HashMap::new(),
            published_diagnostics: HashMap::new(),
        }
    }

    /// Start rust-analyzer with the current working directory as the workspace root.
    pub async fn start(&mut self) -> Result<()> {
        let current_dir = std::env::current_dir()?;
        self.start_in(&current_dir).await
    }

    /// Start rust-analyzer with `root` as the workspace root.
    pub async fn start_in(&mut self, root: &Path) -> Result<()> {
        let root = normalize_path(root);
        let rust_analyzer_path = get_rust_analyzer_path();
        let mut child = tokio::process::Command::new(&rust_analyzer_path)
            .current_dir(&root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Nothing reads stderr; a pipe would fill up and stall the server.
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to start rust-analyzer ({rust_analyzer_path})"))?;

        self.stdin = child.stdin.take();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("rust-analyzer stdout is not available"))?;

        let (sender, receiver) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stdout);
            // Stops at EOF, on a framing error, or once the client is dropped;
            // dropping the sender tells the client the connection is gone.
            while let Ok(Some(message)) = read_message(&mut reader).await {
                if sender.send(message).is_err() {
                    break;
                }
            }
        });

        self.incoming = Some(receiver);
        self.process = Some(child);
        self.initialize(&root).await?;
        Ok(())
    }

    async fn initialize(&mut self, root: &Path) -> Result<()> {
        let root_uri = path_to_file_uri(&root.to_string_lossy());

        // Send initialize request
        let init_params = json!({
            "processId": std::process::id(),
            "clientInfo": {
                "name": "rust-mcp-server",
                "version": env!("CARGO_PKG_VERSION")
            },
            "rootUri": root_uri,
            "workspaceFolders": [{ "uri": root_uri, "name": "root" }],
            "initializationOptions": analyzer_config(),
            "capabilities": {
                "general": {
                    "positionEncodings": ["utf-16"]
                },
                "textDocument": {
                    "synchronization": {
                        "dynamicRegistration": false,
                        "didSave": true
                    },
                    "definition": {
                        "dynamicRegistration": false,
                        "linkSupport": false
                    },
                    "implementation": {
                        "dynamicRegistration": false,
                        "linkSupport": false
                    },
                    "references": {
                        "dynamicRegistration": false
                    },
                    "hover": {
                        "contentFormat": ["plaintext"]
                    },
                    "rename": {
                        "dynamicRegistration": false,
                        "prepareSupport": false
                    },
                    "documentSymbol": {
                        "hierarchicalDocumentSymbolSupport": true
                    },
                    "codeAction": {
                        "codeActionLiteralSupport": {
                            "codeActionKind": {
                                "valueSet": [
                                    "",
                                    "quickfix",
                                    "refactor",
                                    "refactor.extract",
                                    "refactor.inline",
                                    "refactor.rewrite",
                                    "source"
                                ]
                            }
                        },
                        "dataSupport": true,
                        "resolveSupport": {
                            "properties": ["edit"]
                        }
                    },
                    // Pull diagnostics: we ask for a file's diagnostics when we
                    // want them instead of guessing when a push has arrived.
                    "diagnostic": {
                        "dynamicRegistration": false,
                        "relatedDocumentSupport": false
                    },
                    "publishDiagnostics": {
                        "relatedInformation": true
                    }
                },
                "workspace": {
                    "configuration": true,
                    "symbol": {
                        "dynamicRegistration": false
                    },
                    // Note: `snippetTextEdit` is deliberately not advertised, so
                    // every edit rust-analyzer sends is plain text.
                    "workspaceEdit": {
                        "documentChanges": true,
                        "resourceOperations": ["create", "rename", "delete"]
                    }
                },
                "experimental": {
                    "serverStatusNotification": true
                }
            }
        });

        let _response = self.request_once("initialize", init_params).await?;

        // Send initialized notification
        self.send_notification("initialized", json!({})).await?;

        self.initialized = true;
        Ok(())
    }

    async fn send_notification(&mut self, method: &str, params: Value) -> Result<()> {
        let notification = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params
        });

        self.send_message(&notification).await
    }

    async fn send_message(&mut self, message: &Value) -> Result<()> {
        let content = message.to_string();
        let header = format!("Content-Length: {}\r\n\r\n", content.len());

        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow!("rust-analyzer is not running"))?;
        stdin.write_all(header.as_bytes()).await?;
        stdin.write_all(content.as_bytes()).await?;
        stdin.flush().await?;

        Ok(())
    }

    /// Wait for the next message from rust-analyzer. `Ok(None)` means the
    /// deadline passed first.
    async fn next_message(&mut self, deadline: Instant) -> Result<Option<Value>> {
        let incoming = self
            .incoming
            .as_mut()
            .ok_or_else(|| anyhow!("rust-analyzer is not running"))?;

        match tokio::time::timeout_at(deadline, incoming.recv()).await {
            Ok(Some(message)) => Ok(Some(message)),
            Ok(None) => Err(anyhow!("rust-analyzer closed the connection")),
            Err(_) => Ok(None),
        }
    }

    /// Handle anything from rust-analyzer that is not the response we are
    /// waiting for: its own requests to us, and notifications.
    async fn handle_incoming(&mut self, message: &Value) -> Result<()> {
        let Some(method) = message.get("method").and_then(Value::as_str) else {
            // A response to a request we gave up on; nothing to do.
            return Ok(());
        };
        let params = message.get("params").unwrap_or(&Value::Null);

        if let Some(id) = message.get("id") {
            // rust-analyzer blocks on some of these, so every request gets a reply.
            let result = match method {
                "workspace/configuration" => {
                    let items = params.get("items").and_then(Value::as_array);
                    let answers = items.into_iter().flatten().map(|item| {
                        match item.get("section").and_then(Value::as_str) {
                            Some("rust-analyzer") => analyzer_config(),
                            _ => Value::Null,
                        }
                    });
                    Value::Array(answers.collect())
                }
                "workspace/applyEdit" => json!({ "applied": false }),
                _ => Value::Null,
            };
            let response = json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": result
            });
            return self.send_message(&response).await;
        }

        match method {
            "textDocument/publishDiagnostics" => {
                if let Some(uri) = params.get("uri").and_then(Value::as_str) {
                    let diagnostics = params
                        .get("diagnostics")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default();
                    self.published_diagnostics
                        .insert(path_key(&file_uri_to_path(uri)), diagnostics);
                }
            }
            "experimental/serverStatus" => {
                if let Some(quiescent) = params.get("quiescent").and_then(Value::as_bool) {
                    self.quiescent = quiescent;
                }
            }
            _ => {}
        }
        Ok(())
    }

    async fn request_once(&mut self, method: &str, params: Value) -> Result<Value> {
        self.request_id += 1;
        let id = self.request_id;
        let request = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params
        });
        self.send_message(&request).await?;

        let deadline = Instant::now() + REQUEST_TIMEOUT;
        loop {
            let Some(message) = self.next_message(deadline).await? else {
                return Err(anyhow!(
                    "rust-analyzer did not answer {method} within {}s",
                    REQUEST_TIMEOUT.as_secs()
                ));
            };

            let is_our_response = message.get("method").is_none()
                && message.get("id").and_then(Value::as_u64) == Some(id);
            if !is_our_response {
                self.handle_incoming(&message).await?;
                continue;
            }

            if let Some(error) = message.get("error") {
                return Err(LspError {
                    method: method.to_string(),
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error")
                        .to_string(),
                }
                .into());
            }
            return Ok(message.get("result").cloned().unwrap_or(Value::Null));
        }
    }

    /// Send a request and return its `result`.
    async fn request(&mut self, method: &str, params: Value) -> Result<Value> {
        if !self.initialized {
            return Err(anyhow!("Client not initialized"));
        }

        let mut attempt = 0;
        loop {
            match self.request_once(method, params.clone()).await {
                Err(error)
                    if attempt < CONTENT_MODIFIED_RETRIES
                        && error
                            .downcast_ref::<LspError>()
                            .is_some_and(|e| e.code == CONTENT_MODIFIED) =>
                {
                    // rust-analyzer cancelled the request because its view of
                    // the workspace changed underneath it; ask again.
                    attempt += 1;
                    tokio::time::sleep(Duration::from_millis(200 * u64::from(attempt))).await;
                }
                other => return other,
            }
        }
    }

    /// Block until rust-analyzer reports that the workspace is loaded.
    async fn wait_until_ready(&mut self) -> Result<()> {
        // Catch up on anything that arrived since the last request.
        while let Some(message) = self.next_message(Instant::now()).await? {
            self.handle_incoming(&message).await?;
        }

        let deadline = Instant::now() + READY_TIMEOUT;
        while !self.quiescent {
            match self.next_message(deadline).await? {
                Some(message) => self.handle_incoming(&message).await?,
                None => {
                    // Don't stall every later call on a status that never comes.
                    eprintln!(
                        "rust-analyzer did not report ready within {}s; continuing anyway",
                        READY_TIMEOUT.as_secs()
                    );
                    self.quiescent = true;
                }
            }
        }
        Ok(())
    }

    /// Make rust-analyzer's view of `file_path` match what is on disk.
    pub async fn sync_document(&mut self, file_path: &str) -> Result<()> {
        if !self.initialized {
            return Err(anyhow!("Client not initialized"));
        }

        let path = normalize_path(Path::new(file_path));
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?;
        let path_text = path.to_string_lossy().into_owned();
        let key = path_key(&path);

        let (method, params) = match self.open_documents.get_mut(&key) {
            Some(document) if document.text == text => return Ok(()),
            Some(document) => {
                document.version += 1;
                document.text = text;
                let params = create_did_change_params(&path_text, document.version, &document.text);
                ("textDocument/didChange", params)
            }
            None => {
                let params = create_did_open_params(&path_text, 1, &text);
                self.open_documents.insert(
                    key,
                    OpenDocument {
                        path,
                        version: 1,
                        text,
                    },
                );
                ("textDocument/didOpen", params)
            }
        };
        self.send_notification(method, params).await
    }

    /// Re-read every open document, since rust-analyzer ignores the disk for
    /// files it has been told are open and they may have been edited elsewhere.
    async fn resync_open_documents(&mut self) -> Result<()> {
        let open: Vec<(String, PathBuf)> = self
            .open_documents
            .iter()
            .map(|(key, document)| (key.clone(), document.path.clone()))
            .collect();

        for (key, path) in open {
            let path_text = path.to_string_lossy().into_owned();
            if path.is_file() {
                self.sync_document(&path_text).await?;
            } else {
                self.open_documents.remove(&key);
                self.send_notification(
                    "textDocument/didClose",
                    create_text_document_params(&path_text),
                )
                .await?;
            }
        }
        Ok(())
    }

    /// Bring rust-analyzer up to date before a request, optionally opening the
    /// file the request is about.
    async fn prepare(&mut self, file_path: Option<&str>) -> Result<()> {
        if !self.initialized {
            return Err(anyhow!("Client not initialized"));
        }
        self.resync_open_documents().await?;
        if let Some(file_path) = file_path {
            self.sync_document(file_path).await?;
        }
        self.wait_until_ready().await
    }

    /// Apply a `WorkspaceEdit` to disk and tell rust-analyzer about the result.
    pub async fn apply_workspace_edit(&mut self, workspace_edit: &Value) -> Result<EditSummary> {
        let summary = edit::apply_workspace_edit(workspace_edit)?;
        self.resync_open_documents().await?;
        Ok(summary)
    }

    // Requests. Each returns the raw LSP `result` for the tools layer to present.

    pub async fn find_definition(
        &mut self,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value> {
        self.prepare(Some(file_path)).await?;
        let params = create_text_document_position_params(file_path, line, character);
        self.request("textDocument/definition", params).await
    }

    pub async fn find_references(
        &mut self,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value> {
        self.prepare(Some(file_path)).await?;
        let params = create_references_params(file_path, line, character);
        self.request("textDocument/references", params).await
    }

    pub async fn find_implementations(
        &mut self,
        file_path: &str,
        line: u32,
        character: u32,
    ) -> Result<Value> {
        self.prepare(Some(file_path)).await?;
        let params = create_text_document_position_params(file_path, line, character);
        self.request("textDocument/implementation", params).await
    }

    pub async fn hover(&mut self, file_path: &str, line: u32, character: u32) -> Result<Value> {
        self.prepare(Some(file_path)).await?;
        let params = create_text_document_position_params(file_path, line, character);
        self.request("textDocument/hover", params).await
    }

    pub async fn workspace_symbols(&mut self, query: &str) -> Result<Value> {
        self.prepare(None).await?;
        let params = create_workspace_symbol_params(query);
        self.request("workspace/symbol", params).await
    }

    pub async fn document_symbols(&mut self, file_path: &str) -> Result<Value> {
        self.prepare(Some(file_path)).await?;
        let params = create_text_document_params(file_path);
        self.request("textDocument/documentSymbol", params).await
    }

    /// rust-analyzer's own diagnostics for a file (syntax errors, unresolved
    /// names and the like). Type and borrow errors only come from cargo.
    pub async fn get_diagnostics(&mut self, file_path: &str) -> Result<Vec<Value>> {
        self.prepare(Some(file_path)).await?;
        let params = create_text_document_params(file_path);
        let report = self.request("textDocument/diagnostic", params).await?;

        let mut diagnostics = report
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        if let Some(published) = self
            .published_diagnostics
            .get(&path_key(Path::new(file_path)))
        {
            diagnostics.extend(published.iter().cloned());
        }
        Ok(diagnostics)
    }

    /// Rename the symbol at a position across the workspace, on disk.
    pub async fn rename_symbol(
        &mut self,
        file_path: &str,
        line: u32,
        character: u32,
        new_name: &str,
    ) -> Result<EditSummary> {
        self.prepare(Some(file_path)).await?;
        let params = create_rename_params(file_path, line, character, new_name);
        let workspace_edit = self.request("textDocument/rename", params).await?;
        if workspace_edit.is_null() {
            return Err(anyhow!(
                "Nothing to rename at {file_path}:{line}:{character}"
            ));
        }
        self.apply_workspace_edit(&workspace_edit).await
    }

    /// Format a file with rustfmt, on disk. Returns the number of edits made.
    pub async fn format_code(&mut self, file_path: &str) -> Result<usize> {
        self.prepare(Some(file_path)).await?;
        let params = create_formatting_params(file_path);
        let edits = self.request("textDocument/formatting", params).await?;

        let edits = edits.as_array().cloned().unwrap_or_default();
        let count = edit::apply_text_edits_to_file(&normalize_path(Path::new(file_path)), &edits)?;
        self.resync_open_documents().await?;
        Ok(count)
    }

    /// The code actions rust-analyzer offers for a range. `start` and `end` are
    /// `(line, character)` pairs.
    pub async fn code_actions(
        &mut self,
        file_path: &str,
        start: (u32, u32),
        end: (u32, u32),
    ) -> Result<Vec<Value>> {
        self.prepare(Some(file_path)).await?;
        let params = create_code_action_params(file_path, start, end);
        let actions = self.request("textDocument/codeAction", params).await?;
        Ok(actions.as_array().cloned().unwrap_or_default())
    }

    /// Resolve a code action returned by [`Self::code_actions`] and apply its edit.
    pub async fn apply_code_action(&mut self, action: &Value) -> Result<EditSummary> {
        let title = action
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("code action");

        let resolved = if action.get("edit").is_some() {
            action.clone()
        } else {
            self.request("codeAction/resolve", action.clone()).await?
        };
        let workspace_edit = resolved
            .get("edit")
            .ok_or_else(|| anyhow!("rust-analyzer returned no edit for \"{title}\""))?;

        self.apply_workspace_edit(workspace_edit).await
    }
}
