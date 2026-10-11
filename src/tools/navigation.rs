use crate::analyzer::RustAnalyzerClient;
use crate::tools::support::{format_locations, str_arg, symbol_kind_name};
use crate::tools::types::ToolResult;
use anyhow::Result;
use serde_json::Value;

pub async fn workspace_symbols_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let query = str_arg(&args, "query")?;

    let result = analyzer.workspace_symbols(query).await?;

    let mut lines = Vec::new();
    for symbol in result.as_array().into_iter().flatten() {
        let name = symbol.get("name").and_then(Value::as_str).unwrap_or("?");
        let kind = symbol_kind_name(symbol.get("kind").and_then(Value::as_u64).unwrap_or(0));
        let container = match symbol.get("containerName").and_then(Value::as_str) {
            Some(container) if !container.is_empty() => format!(" in {container}"),
            _ => String::new(),
        };
        let location = symbol
            .get("location")
            .map(format_locations)
            .and_then(|locations| locations.into_iter().next())
            .unwrap_or_default();
        lines.push(format!("{kind} {name}{container}\n  {location}"));
    }

    if lines.is_empty() {
        return Ok(ToolResult::text(format!("No symbols found for '{query}'")));
    }
    Ok(ToolResult::text(format!(
        "{} symbol(s) matching '{query}' (path:line:character, 0-based):\n{}",
        lines.len(),
        lines.join("\n")
    )))
}
