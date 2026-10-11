use crate::analyzer::RustAnalyzerClient;
use crate::tools::support::str_arg;
use crate::tools::types::ToolResult;
use anyhow::Result;
use serde_json::Value;

pub async fn format_code_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;

    let edits = analyzer.format_code(file_path).await?;

    Ok(ToolResult::text(if edits == 0 {
        format!("{file_path} is already formatted")
    } else {
        format!("Formatted {file_path} ({edits} edit(s))")
    }))
}
