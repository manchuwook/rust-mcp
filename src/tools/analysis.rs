use crate::analyzer::RustAnalyzerClient;
use crate::tools::cargo::{check_file, count_levels, message_rendered};
use crate::tools::support::{format_locations, str_arg, u32_arg};
use crate::tools::types::ToolResult;
use anyhow::Result;
use serde_json::Value;

pub async fn find_definition_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;
    let line = u32_arg(&args, "line")?;
    let character = u32_arg(&args, "character")?;

    let result = analyzer.find_definition(file_path, line, character).await?;

    let locations = format_locations(&result);
    if locations.is_empty() {
        return Ok(ToolResult::text("No definition found"));
    }
    Ok(ToolResult::text(format!(
        "Definition (path:line:character, 0-based):\n{}",
        locations.join("\n")
    )))
}

pub async fn find_references_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;
    let line = u32_arg(&args, "line")?;
    let character = u32_arg(&args, "character")?;

    let result = analyzer.find_references(file_path, line, character).await?;

    let locations = format_locations(&result);
    if locations.is_empty() {
        return Ok(ToolResult::text("No references found"));
    }
    Ok(ToolResult::text(format!(
        "{} reference(s) (path:line:character, 0-based):\n{}",
        locations.len(),
        locations.join("\n")
    )))
}

fn severity_name(diagnostic: &Value) -> &'static str {
    match diagnostic.get("severity").and_then(Value::as_u64) {
        Some(1) => "error",
        Some(2) => "warning",
        Some(3) => "info",
        Some(4) => "hint",
        _ => "diagnostic",
    }
}

/// One line for an LSP diagnostic: `severity line:character [code] message`.
fn format_lsp_diagnostic(diagnostic: &Value) -> String {
    let start = diagnostic.get("range").and_then(|range| range.get("start"));
    let position = |name: &str| {
        start
            .and_then(|start| start.get(name))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let code = match diagnostic.get("code") {
        Some(Value::String(code)) => format!(" [{code}]"),
        Some(Value::Number(code)) => format!(" [{code}]"),
        _ => String::new(),
    };
    let message = diagnostic
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or("");
    format!(
        "  {} {}:{}{code} {message}",
        severity_name(diagnostic),
        position("line"),
        position("character")
    )
}

pub async fn get_diagnostics_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;

    let mut sections = Vec::new();

    // rust-analyzer sees syntax errors and unresolved names immediately, but
    // not type or borrow errors; those need the compiler.
    let mut native: Vec<String> = Vec::new();
    for diagnostic in analyzer.get_diagnostics(file_path).await? {
        let line = format_lsp_diagnostic(&diagnostic);
        if !native.contains(&line) {
            native.push(line);
        }
    }
    sections.push(if native.is_empty() {
        "rust-analyzer: no diagnostics".to_string()
    } else {
        format!(
            "rust-analyzer: {} diagnostic(s) (line:character, 0-based)\n{}",
            native.len(),
            native.join("\n")
        )
    });

    match check_file(file_path).await {
        Ok(messages) if messages.is_empty() => {
            sections.push("cargo check: no diagnostics in this file".to_string());
        }
        Ok(messages) => {
            let (errors, warnings) = count_levels(&messages);
            sections.push(format!(
                "cargo check: {errors} error(s), {warnings} warning(s) in this file"
            ));
            sections.extend(messages.iter().map(|m| message_rendered(m).to_string()));
        }
        Err(error) => sections.push(format!("cargo check could not run: {error:#}")),
    }

    Ok(ToolResult::text(sections.join("\n\n")))
}
