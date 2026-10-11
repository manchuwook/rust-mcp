use crate::analyzer::RustAnalyzerClient;
use crate::analyzer::protocol::{normalize_path, path_key};
use crate::tools::support::str_arg;
use crate::tools::types::ToolResult;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Stdio;

/// The result of a cargo command run with `--message-format=json`.
pub struct CargoRun {
    /// Compiler diagnostics, deduplicated (the same message is reported once
    /// per target that compiles the file).
    pub messages: Vec<Value>,
    pub success: bool,
    pub stderr: String,
}

pub async fn cargo_output(args: &[&str], dir: &Path) -> Result<std::process::Output> {
    tokio::process::Command::new("cargo")
        .args(args)
        .current_dir(dir)
        .stdin(Stdio::null())
        .output()
        .await
        .with_context(|| format!("failed to run `cargo {}`", args.join(" ")))
}

/// The last few lines of cargo's stderr, which is where it explains a failure.
pub fn stderr_tail(stderr: &str) -> String {
    let lines: Vec<&str> = stderr.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(15)..].join("\n")
}

/// Pull the compiler diagnostics out of cargo's JSON message stream.
pub fn parse_compiler_messages(stdout: &str) -> Vec<Value> {
    let mut messages: Vec<Value> = Vec::new();
    for line in stdout.lines() {
        let Ok(entry) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        if entry.get("reason").and_then(Value::as_str) != Some("compiler-message") {
            continue;
        }
        let Some(message) = entry.get("message") else {
            continue;
        };
        // Skip rustc's closing summaries ("aborting due to 2 previous errors",
        // "3 warnings emitted"); they are not diagnostics in their own right.
        let has_spans = message
            .get("spans")
            .and_then(Value::as_array)
            .is_some_and(|spans| !spans.is_empty());
        let text = message_text(message);
        if !has_spans && (text.starts_with("aborting due to") || text.ends_with("emitted")) {
            continue;
        }
        if !messages.contains(message) {
            messages.push(message.clone());
        }
    }
    messages
}

/// Run `cargo <args>` in `dir`. `args` must include `--message-format=json`.
///
/// A failed build that produced diagnostics is a normal result; an error is
/// returned only when cargo could not run the build at all.
pub async fn run_cargo_json(args: &[&str], dir: &Path) -> Result<CargoRun> {
    let output = cargo_output(args, dir).await?;
    let messages = parse_compiler_messages(&String::from_utf8_lossy(&output.stdout));
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    let has_errors = messages.iter().any(|m| message_level(m) == "error");
    if !output.status.success() && !has_errors {
        bail!(
            "`cargo {}` failed in {}:\n{}",
            args.join(" "),
            dir.display(),
            stderr_tail(&stderr)
        );
    }

    Ok(CargoRun {
        messages,
        success: output.status.success(),
        stderr,
    })
}

/// The directory to run cargo in for a path that may be a file or a directory.
pub fn cargo_dir(path: &str) -> Result<PathBuf> {
    let path = normalize_path(Path::new(path));
    if path.is_dir() {
        return Ok(path);
    }
    if path.is_file()
        && let Some(parent) = path.parent()
    {
        return Ok(parent.to_path_buf());
    }
    Err(anyhow!("{} does not exist", path.display()))
}

/// The nearest `Cargo.toml` at or above `path`.
pub fn find_manifest(path: &Path) -> Result<PathBuf> {
    let path = normalize_path(path);
    path.ancestors()
        .map(|dir| dir.join("Cargo.toml"))
        .find(|manifest| manifest.is_file())
        .ok_or_else(|| anyhow!("no Cargo.toml found for {}", path.display()))
}

/// The root of the workspace containing `dir`. Compiler messages report file
/// names relative to it.
pub async fn workspace_root(dir: &Path) -> Result<PathBuf> {
    let args = ["locate-project", "--workspace", "--message-format", "plain"];
    let output = cargo_output(&args, dir).await?;
    if !output.status.success() {
        bail!(
            "could not locate the cargo workspace for {}:\n{}",
            dir.display(),
            stderr_tail(&String::from_utf8_lossy(&output.stderr))
        );
    }
    let manifest = String::from_utf8_lossy(&output.stdout).trim().to_string();
    Path::new(&manifest)
        .parent()
        .map(normalize_path)
        .ok_or_else(|| anyhow!("unexpected `cargo locate-project` output: {manifest}"))
}

pub fn message_level(message: &Value) -> &str {
    message.get("level").and_then(Value::as_str).unwrap_or("")
}

pub fn message_text(message: &Value) -> &str {
    message.get("message").and_then(Value::as_str).unwrap_or("")
}

/// The lint or error code of a message, e.g. `E0502` or `clippy::needless_return`.
pub fn message_code(message: &Value) -> Option<&str> {
    message
        .get("code")
        .and_then(|code| code.get("code"))
        .and_then(Value::as_str)
}

/// The message as rustc would print it, falling back to the bare text.
pub fn message_rendered(message: &Value) -> &str {
    message
        .get("rendered")
        .and_then(Value::as_str)
        .unwrap_or_else(|| message_text(message))
        .trim_end()
}

/// Whether a span (from a compiler message) points into the file with this `path_key`.
pub fn span_in_file(span: &Value, workspace_root: &Path, file_key: &str) -> bool {
    span.get("file_name")
        .and_then(Value::as_str)
        // `join` leaves absolute names (dependencies, std) as they are.
        .is_some_and(|name| path_key(&workspace_root.join(name)) == file_key)
}

/// Whether a message's primary location is in the file with this `path_key`.
pub fn message_in_file(message: &Value, workspace_root: &Path, file_key: &str) -> bool {
    message
        .get("spans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .any(|span| {
            span.get("is_primary").and_then(Value::as_bool) == Some(true)
                && span_in_file(span, workspace_root, file_key)
        })
}

/// `(errors, warnings)` in a list of compiler messages.
pub fn count_levels(messages: &[Value]) -> (usize, usize) {
    let count = |level: &str| {
        messages
            .iter()
            .filter(|m| message_level(m) == level)
            .count()
    };
    (count("error"), count("warning"))
}

/// Run `cargo check` for the package containing `file_path` and return the
/// diagnostics whose primary location is in that file.
pub async fn check_file(file_path: &str) -> Result<Vec<Value>> {
    let file = normalize_path(Path::new(file_path));
    let manifest = find_manifest(&file)?;
    let dir = manifest.parent().unwrap_or(Path::new("."));

    let root = workspace_root(dir).await?;
    let run = run_cargo_json(&["check", "--all-targets", "--message-format=json"], dir).await?;

    let file_key = path_key(&file);
    Ok(run
        .messages
        .into_iter()
        .filter(|message| message_in_file(message, &root, &file_key))
        .collect())
}

fn field<'a>(value: &'a Value, name: &str) -> &'a str {
    value.get(name).and_then(Value::as_str).unwrap_or("")
}

fn describe_dependency(dependency: &Value) -> String {
    let mut line = format!("{} {}", field(dependency, "name"), field(dependency, "req"));

    let mut notes = Vec::new();
    if let Some(rename) = dependency.get("rename").and_then(Value::as_str) {
        notes.push(format!("renamed to {rename}"));
    }
    if dependency.get("optional").and_then(Value::as_bool) == Some(true) {
        notes.push("optional".to_string());
    }
    if dependency
        .get("uses_default_features")
        .and_then(Value::as_bool)
        == Some(false)
    {
        notes.push("no default features".to_string());
    }
    let features: Vec<&str> = dependency
        .get("features")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    if !features.is_empty() {
        notes.push(format!("features: {}", features.join(", ")));
    }
    if let Some(target) = dependency.get("target").and_then(Value::as_str) {
        notes.push(format!("target: {target}"));
    }
    if dependency.get("path").is_some_and(|p| !p.is_null()) {
        notes.push("path dependency".to_string());
    }

    if !notes.is_empty() {
        line.push_str(&format!(" [{}]", notes.join("; ")));
    }
    line
}

/// Render `cargo metadata --no-deps` output as a readable summary.
pub fn describe_metadata(metadata: &Value) -> String {
    let mut out = Vec::new();
    out.push(format!(
        "Workspace root: {}",
        field(metadata, "workspace_root")
    ));

    let packages = metadata
        .get("packages")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if packages.len() > 1 {
        out.push(format!("Workspace members: {}", packages.len()));
    }

    for package in &packages {
        out.push(String::new());
        out.push(format!(
            "Package {} {} (edition {})",
            field(package, "name"),
            field(package, "version"),
            field(package, "edition")
        ));
        out.push(format!("  Manifest: {}", field(package, "manifest_path")));
        if let Some(rust_version) = package.get("rust_version").and_then(Value::as_str) {
            out.push(format!("  Minimum Rust version: {rust_version}"));
        }

        let targets: Vec<String> = package
            .get("targets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|target| {
                let kinds: Vec<&str> = target
                    .get("kind")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                    .collect();
                format!("{} ({})", field(target, "name"), kinds.join("/"))
            })
            .collect();
        out.push(format!("  Targets: {}", targets.join(", ")));

        match package.get("features").and_then(Value::as_object) {
            Some(features) if !features.is_empty() => {
                out.push("  Features:".to_string());
                for (name, enables) in features {
                    let enables: Vec<&str> = enables
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .collect();
                    out.push(format!("    {name} = [{}]", enables.join(", ")));
                }
            }
            _ => out.push("  Features: none".to_string()),
        }

        let dependencies = package
            .get("dependencies")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for (kind, title) in [
            (None, "Dependencies"),
            (Some("dev"), "Dev-dependencies"),
            (Some("build"), "Build-dependencies"),
        ] {
            let of_kind: Vec<&Value> = dependencies
                .iter()
                .filter(|d| d.get("kind").and_then(Value::as_str) == kind)
                .collect();
            if of_kind.is_empty() {
                continue;
            }
            out.push(format!("  {title} ({}):", of_kind.len()));
            for dependency in of_kind {
                out.push(format!("    {}", describe_dependency(dependency)));
            }
        }
    }

    out.join("\n")
}

/// Run `cargo metadata --no-deps` for a manifest (or a directory containing one).
pub async fn cargo_metadata(manifest_path: &str) -> Result<Value> {
    let mut manifest = normalize_path(Path::new(manifest_path));
    if manifest.is_dir() {
        manifest = manifest.join("Cargo.toml");
    }
    if !manifest.is_file() {
        bail!("{} does not exist", manifest.display());
    }
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let manifest_text = manifest.to_string_lossy();

    let args = [
        "metadata",
        "--format-version",
        "1",
        "--no-deps",
        "--manifest-path",
        manifest_text.as_ref(),
    ];
    let output = cargo_output(&args, dir).await?;
    if !output.status.success() {
        bail!(
            "could not read {}:\n{}",
            manifest.display(),
            stderr_tail(&String::from_utf8_lossy(&output.stderr))
        );
    }
    serde_json::from_slice(&output.stdout).context("unexpected `cargo metadata` output")
}

pub async fn analyze_manifest_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let manifest_path = str_arg(&args, "manifest_path")?;

    let metadata = cargo_metadata(manifest_path).await?;

    Ok(ToolResult::text(describe_metadata(&metadata)))
}

pub async fn run_cargo_check_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let workspace_path = str_arg(&args, "workspace_path")?;

    let dir = cargo_dir(workspace_path)?;
    let run = run_cargo_json(&["check", "--all-targets", "--message-format=json"], &dir).await?;

    let (errors, warnings) = count_levels(&run.messages);
    let status = if run.success { "passed" } else { "failed" };
    let mut sections: Vec<String> = run
        .messages
        .iter()
        .map(|message| message_rendered(message).to_string())
        .collect();
    sections.push(format!(
        "cargo check {status} in {}: {errors} error(s), {warnings} warning(s)",
        dir.display()
    ));

    Ok(ToolResult::text(sections.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn compiler_messages_are_extracted_and_deduplicated() {
        let diagnostic = json!({
            "reason": "compiler-message",
            "message": {
                "level": "warning",
                "message": "unused variable: `x`",
                "code": { "code": "unused_variables" },
                "spans": [{ "file_name": "src/lib.rs", "is_primary": true }],
                "rendered": "warning: unused variable: `x`\n"
            }
        });
        let summary = json!({
            "reason": "compiler-message",
            "message": { "level": "warning", "message": "1 warning emitted", "spans": [] }
        });
        let stdout = format!(
            "{diagnostic}\n{}\nnot json\n{diagnostic}\n{summary}\n",
            json!({ "reason": "compiler-artifact" })
        );

        let messages = parse_compiler_messages(&stdout);

        assert_eq!(messages.len(), 1);
        assert_eq!(message_code(&messages[0]), Some("unused_variables"));
        assert_eq!(
            message_rendered(&messages[0]),
            "warning: unused variable: `x`"
        );
        assert_eq!(count_levels(&messages), (0, 1));
    }

    #[test]
    fn messages_are_matched_to_files_by_primary_span() {
        let root = std::env::temp_dir().join("ws");
        let file_key = path_key(&root.join("src").join("lib.rs"));
        let message = json!({
            "spans": [
                { "file_name": "src/other.rs", "is_primary": true },
                { "file_name": "src/lib.rs", "is_primary": false }
            ]
        });
        assert!(!message_in_file(&message, &root, &file_key));

        let message = json!({ "spans": [{ "file_name": "src/lib.rs", "is_primary": true }] });
        assert!(message_in_file(&message, &root, &file_key));
    }

    #[test]
    fn metadata_summary_lists_dependencies_by_kind() {
        let metadata = json!({
            "workspace_root": "/ws",
            "packages": [{
                "name": "demo",
                "version": "0.1.0",
                "edition": "2021",
                "manifest_path": "/ws/Cargo.toml",
                "targets": [{ "name": "demo", "kind": ["lib"] }],
                "features": { "default": ["std"], "std": [] },
                "dependencies": [
                    { "name": "serde", "req": "^1.0", "kind": null, "optional": true,
                      "uses_default_features": false, "features": ["derive"] },
                    { "name": "tempfile", "req": "^3", "kind": "dev", "optional": false,
                      "uses_default_features": true, "features": [] }
                ]
            }]
        });

        let summary = describe_metadata(&metadata);

        assert!(summary.contains("Package demo 0.1.0 (edition 2021)"));
        assert!(summary.contains("Targets: demo (lib)"));
        assert!(summary.contains("default = [std]"));
        assert!(summary.contains(
            "Dependencies (1):\n    serde ^1.0 [optional; no default features; features: derive]"
        ));
        assert!(summary.contains("Dev-dependencies (1):\n    tempfile ^3"));
    }
}
