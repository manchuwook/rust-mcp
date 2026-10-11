use anyhow::Result;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{ErrorData as McpError, *},
    tool, tool_handler, tool_router,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::analyzer::RustAnalyzerClient;
use crate::server::parameters::*;
use crate::tools::{execute_tool, get_tools};

#[derive(Clone)]
pub struct RustMcpServer {
    analyzer: Arc<Mutex<RustAnalyzerClient>>,
    // Populated by `Self::tool_router()` and consumed by the `#[tool_handler]`
    // macro's generated routing; not read directly, hence the allow.
    #[allow(dead_code)]
    tool_router: ToolRouter<RustMcpServer>,
}

impl Default for RustMcpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[tool_router]
impl RustMcpServer {
    pub fn new() -> Self {
        Self {
            analyzer: Arc::new(Mutex::new(RustAnalyzerClient::new())),
            tool_router: Self::tool_router(),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        let mut analyzer = self.analyzer.lock().await;
        analyzer.start().await
    }

    pub fn list_tools(&self) -> Vec<crate::tools::ToolDefinition> {
        get_tools()
    }

    pub async fn call_tool(&mut self, name: &str, args: Value) -> Result<crate::tools::ToolResult> {
        let mut analyzer = self.analyzer.lock().await;
        execute_tool(name, args, &mut analyzer).await
    }

    /// Run a tool and turn its outcome into an MCP result. Failures are
    /// reported as tool errors so callers can tell them from results.
    async fn run_tool(
        &self,
        name: &str,
        params: impl serde::Serialize,
    ) -> Result<CallToolResult, McpError> {
        let args = serde_json::to_value(params)
            .map_err(|e| McpError::internal_error(e.to_string(), None))?;

        let mut analyzer = self.analyzer.lock().await;
        match execute_tool(name, args, &mut analyzer).await {
            Ok(result) => {
                let text: Vec<&str> = result
                    .content
                    .iter()
                    .filter_map(|content| content.get("text")?.as_str())
                    .collect();
                Ok(CallToolResult::success(vec![ContentBlock::text(
                    text.join("\n"),
                )]))
            }
            Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                "Error: {e:#}"
            ))])),
        }
    }

    #[tool(description = "Find the definition of a symbol at a given position")]
    async fn find_definition(
        &self,
        Parameters(params): Parameters<FindDefinitionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("find_definition", params).await
    }

    #[tool(description = "Find all references to a symbol at a given position")]
    async fn find_references(
        &self,
        Parameters(params): Parameters<FindReferencesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("find_references", params).await
    }

    #[tool(description = "Get compiler diagnostics for a file")]
    async fn get_diagnostics(
        &self,
        Parameters(params): Parameters<GetDiagnosticsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("get_diagnostics", params).await
    }

    #[tool(description = "Search for symbols in the workspace")]
    async fn workspace_symbols(
        &self,
        Parameters(params): Parameters<WorkspaceSymbolsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("workspace_symbols", params).await
    }

    #[tool(description = "Rename a symbol with scope awareness")]
    async fn rename_symbol(
        &self,
        Parameters(params): Parameters<RenameSymbolParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("rename_symbol", params).await
    }

    #[tool(description = "Apply rustfmt formatting to a file")]
    async fn format_code(
        &self,
        Parameters(params): Parameters<FormatCodeParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("format_code", params).await
    }

    #[tool(description = "Parse and analyze Cargo.toml file")]
    async fn analyze_manifest(
        &self,
        Parameters(params): Parameters<AnalyzeManifestParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("analyze_manifest", params).await
    }

    #[tool(description = "Execute cargo check and parse errors")]
    async fn run_cargo_check(
        &self,
        Parameters(params): Parameters<RunCargoCheckParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("run_cargo_check", params).await
    }

    #[tool(description = "Extract selected code into a new function")]
    async fn extract_function(
        &self,
        Parameters(params): Parameters<ExtractFunctionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("extract_function", params).await
    }

    #[tool(description = "Generate a struct with specified fields and derives")]
    async fn generate_struct(
        &self,
        Parameters(params): Parameters<GenerateStructParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("generate_struct", params).await
    }

    #[tool(description = "Generate an enum with specified variants and derives")]
    async fn generate_enum(
        &self,
        Parameters(params): Parameters<GenerateEnumParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("generate_enum", params).await
    }

    #[tool(description = "Generate a trait implementation for a struct")]
    async fn generate_trait_impl(
        &self,
        Parameters(params): Parameters<GenerateTraitImplParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("generate_trait_impl", params).await
    }

    #[tool(description = "Generate unit tests for a function")]
    async fn generate_tests(
        &self,
        Parameters(params): Parameters<GenerateTestsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("generate_tests", params).await
    }

    #[tool(description = "Inline a function call at specified position")]
    async fn inline_function(
        &self,
        Parameters(params): Parameters<InlineFunctionParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("inline_function", params).await
    }

    #[tool(description = "Change the signature of a function")]
    async fn change_signature(
        &self,
        Parameters(params): Parameters<ChangeSignatureParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("change_signature", params).await
    }

    #[tool(description = "Organize and sort import statements in a file")]
    async fn organize_imports(
        &self,
        Parameters(params): Parameters<OrganizeImportsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("organize_imports", params).await
    }

    #[tool(description = "Apply clippy lint suggestions to improve code quality")]
    async fn apply_clippy_suggestions(
        &self,
        Parameters(params): Parameters<ApplyClippySuggestionsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("apply_clippy_suggestions", params).await
    }

    #[tool(description = "Validate and suggest lifetime annotations")]
    async fn validate_lifetimes(
        &self,
        Parameters(params): Parameters<ValidateLifetimesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("validate_lifetimes", params).await
    }

    #[tool(description = "Get type hierarchy for a symbol at specified position")]
    async fn get_type_hierarchy(
        &self,
        Parameters(params): Parameters<GetTypeHierarchyParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("get_type_hierarchy", params).await
    }

    #[tool(description = "Suggest crate dependencies based on code patterns")]
    async fn suggest_dependencies(
        &self,
        Parameters(params): Parameters<SuggestDependenciesParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("suggest_dependencies", params).await
    }

    #[tool(description = "Create a new Rust module with optional visibility")]
    async fn create_module(
        &self,
        Parameters(params): Parameters<CreateModuleParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("create_module", params).await
    }

    #[tool(description = "Move code items from one file to another")]
    async fn move_items(
        &self,
        Parameters(params): Parameters<MoveItemsParams>,
    ) -> Result<CallToolResult, McpError> {
        self.run_tool("move_items", params).await
    }
}

#[tool_handler]
impl ServerHandler for RustMcpServer {
    fn get_info(&self) -> ServerConfig {
        // `InitializeResult`/`ServerConfig` is `#[non_exhaustive]` in rmcp 3.x, so it
        // must be built via its constructor rather than a struct literal. `new`
        // defaults the protocol version to `ProtocolVersion::LATEST`; we set it
        // explicitly to make the intent (negotiate the newest protocol) clear.
        let mut info = ServerConfig::new(ServerCapabilities::builder().enable_tools().build());
        info.protocol_version = ProtocolVersion::LATEST;
        info.instructions = Some("Rust MCP Server providing rust-analyzer integration for idiomatic Rust development tools. Provides code analysis, refactoring, and project management capabilities.".to_string());
        info
    }
}
