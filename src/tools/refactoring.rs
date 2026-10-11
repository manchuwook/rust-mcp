use crate::analyzer::RustAnalyzerClient;
use crate::analyzer::edit::{offset_to_position, range_to_offsets};
use crate::analyzer::protocol::{file_uri_to_path, normalize_path, path_key};
use crate::tools::support::{
    check_identifier, code_chars, contains_word, find_word, format_locations, read_source, str_arg,
    u32_arg, write_source,
};
use crate::tools::types::ToolResult;
use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;

/// Find a rust-analyzer code action by assist id (the stable identifier it
/// carries in `data.id`, e.g. `extract_function:RefactorExtract:0`), falling
/// back to matching the title for actions that arrive already resolved.
pub fn pick_action<'a>(actions: &'a [Value], ids: &[&str], titles: &[&str]) -> Option<&'a Value> {
    let assist_id = |action: &'a Value| {
        action
            .get("data")
            .and_then(|data| data.get("id"))
            .and_then(Value::as_str)
            .and_then(|id| id.split(':').next())
    };
    let title = |action: &'a Value| action.get("title").and_then(Value::as_str).unwrap_or("");

    ids.iter()
        .find_map(|id| actions.iter().find(|action| assist_id(action) == Some(*id)))
        .or_else(|| {
            titles.iter().find_map(|wanted| {
                actions
                    .iter()
                    .find(|action| title(action).starts_with(wanted))
            })
        })
}

/// The titles of the actions rust-analyzer did offer, for error messages.
pub fn describe_actions(actions: &[Value]) -> String {
    let titles: Vec<&str> = actions
        .iter()
        .filter_map(|action| action.get("title").and_then(Value::as_str))
        .collect();
    if titles.is_empty() {
        "rust-analyzer offers no code actions there".to_string()
    } else {
        format!("rust-analyzer offers: {}", titles.join("; "))
    }
}

pub async fn rename_symbol_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;
    let line = u32_arg(&args, "line")?;
    let character = u32_arg(&args, "character")?;
    let new_name = str_arg(&args, "new_name")?;

    let summary = analyzer
        .rename_symbol(file_path, line, character, new_name)
        .await?;

    Ok(ToolResult::text(format!(
        "Renamed to '{new_name}': {} edit(s)\n{summary}",
        summary.edit_count()
    )))
}

/// rust-analyzer names an extracted function `fun_name` (or `fun_name1`, ...
/// if that is taken). Find the one that is in `after` but was not in `before`.
fn find_generated_function(before: &str, after: &str) -> Option<(String, usize)> {
    const PLACEHOLDER: &str = "fun_name";

    let mut from = 0;
    while let Some(found) = after[from..].find(PLACEHOLDER) {
        let start = from + found;
        let suffix_len = after[start + PLACEHOLDER.len()..]
            .bytes()
            .take_while(u8::is_ascii_digit)
            .count();
        let end = start + PLACEHOLDER.len() + suffix_len;
        let name = &after[start..end];

        let is_definition = after[..start].trim_end().ends_with("fn");
        if is_definition
            && find_word(after, name, start) == Some(start)
            && !contains_word(before, name)
        {
            return Some((name.to_string(), start));
        }
        from = end;
    }
    None
}

pub async fn extract_function_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;
    let start_line = u32_arg(&args, "start_line")?;
    let start_character = u32_arg(&args, "start_character")?;
    let end_line = u32_arg(&args, "end_line")?;
    let end_character = u32_arg(&args, "end_character")?;
    let function_name = str_arg(&args, "function_name")?;
    check_identifier(function_name, "function name")?;

    let file = normalize_path(Path::new(file_path));
    let before = read_source(&file)?;

    let actions = analyzer
        .code_actions(
            file_path,
            (start_line, start_character),
            (end_line, end_character),
        )
        .await?;
    let action = pick_action(&actions, &["extract_function"], &["Extract into function"])
        .ok_or_else(|| {
            anyhow!(
                "The selection cannot be extracted into a function; {}",
                describe_actions(&actions)
            )
        })?;
    let mut summary = analyzer.apply_code_action(action).await?;

    // Give the new function the requested name.
    let after = read_source(&file)?;
    match find_generated_function(&before, &after) {
        Some((generated, _)) if generated == function_name => {}
        Some((generated, offset)) => {
            let (line, character) = offset_to_position(&after, offset);
            summary = analyzer
                .rename_symbol(file_path, line, character, function_name)
                .await
                .with_context(|| {
                    format!(
                        "the code was extracted into `{generated}`, but renaming it to \
                         `{function_name}` failed"
                    )
                })?;
        }
        None => bail!(
            "the code was extracted, but the new function could not be located to rename it \
             to `{function_name}`"
        ),
    }

    Ok(ToolResult::text(format!(
        "Extracted {file_path}:{start_line}:{start_character}-{end_line}:{end_character} into \
         function '{function_name}'\n{summary}"
    )))
}

pub async fn inline_function_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;
    let line = u32_arg(&args, "line")?;
    let character = u32_arg(&args, "character")?;

    let position = (line, character);
    let actions = analyzer.code_actions(file_path, position, position).await?;
    // On a call this inlines that call; on a definition it inlines every caller.
    let action = pick_action(
        &actions,
        &["inline_call", "inline_into_callers"],
        &["Inline `", "Inline into all callers"],
    )
    .ok_or_else(|| {
        anyhow!(
            "No function call to inline at {file_path}:{line}:{character}; {}",
            describe_actions(&actions)
        )
    })?;
    let title = action
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or("Inline")
        .to_string();

    let summary = analyzer.apply_code_action(action).await?;

    Ok(ToolResult::text(format!(
        "{title} at {file_path}:{line}:{character}\n{summary}"
    )))
}

/// The innermost function or method symbol whose range contains the position.
fn find_function_at(symbols: &[Value], line: u32, character: u32) -> Option<&Value> {
    const METHOD: u64 = 6;
    const FUNCTION: u64 = 12;

    let position = (u64::from(line), u64::from(character));
    let point = |range: &Value, name: &str| -> Option<(u64, u64)> {
        let point = range.get(name)?;
        Some((
            point.get("line")?.as_u64()?,
            point.get("character")?.as_u64()?,
        ))
    };

    for symbol in symbols {
        let Some(range) = symbol.get("range") else {
            continue;
        };
        let (Some(start), Some(end)) = (point(range, "start"), point(range, "end")) else {
            continue;
        };
        if position < start || position > end {
            continue;
        }

        let children = symbol.get("children").and_then(Value::as_array);
        if let Some(inner) = children.and_then(|c| find_function_at(c, line, character)) {
            return Some(inner);
        }
        let kind = symbol.get("kind").and_then(Value::as_u64);
        if kind == Some(FUNCTION) || kind == Some(METHOD) {
            return Some(symbol);
        }
    }
    None
}

/// The span of a function's declaration header within `text`: from its
/// qualifiers (or the `fn` keyword when `from_fn_keyword`) up to, but not
/// including, the body's `{` or the terminating `;`.
fn signature_span(
    text: &str,
    item: (usize, usize),
    name: (usize, usize),
    from_fn_keyword: bool,
) -> Result<(usize, usize)> {
    let before_name = &text[item.0..name.0];
    let fn_offset = item.0
        + before_name
            .rfind("fn")
            .ok_or_else(|| anyhow!("could not find the `fn` keyword of the function"))?;

    let start = if from_fn_keyword {
        fn_offset
    } else {
        // Qualifiers (`pub`, `async`, `const`, `unsafe`, `extern`) sit between
        // the start of the line and `fn`; attributes are on the lines above.
        let line_start = text[..fn_offset].rfind('\n').map_or(0, |i| i + 1);
        let indent = text[line_start..fn_offset]
            .find(|c: char| !c.is_whitespace())
            .unwrap_or(fn_offset - line_start);
        (line_start + indent).max(item.0)
    };

    let mut depth = 0usize;
    for (offset, c) in code_chars(text, name.1) {
        if offset >= item.1 {
            break;
        }
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            '{' | ';' if depth == 0 => return Ok((start, offset)),
            _ => {}
        }
    }
    Err(anyhow!("could not find the end of the function signature"))
}

pub async fn change_signature_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;
    let line = u32_arg(&args, "line")?;
    let character = u32_arg(&args, "character")?;
    let new_signature = str_arg(&args, "new_signature")?
        .trim()
        .trim_end_matches(['{', ';'])
        .trim_end();

    let fn_in_signature = find_word(new_signature, "fn", 0).ok_or_else(|| {
        anyhow!("new_signature must be a function declaration, e.g. `pub fn name(arg: T) -> R`")
    })?;

    let symbols = analyzer.document_symbols(file_path).await?;
    let function = find_function_at(
        symbols.as_array().map_or(&[][..], Vec::as_slice),
        line,
        character,
    )
    .ok_or_else(|| anyhow!("No function found at {file_path}:{line}:{character}"))?;

    let file = normalize_path(Path::new(file_path));
    let text = read_source(&file)?;
    let range = |name: &str| -> Result<(usize, usize)> {
        let range = function
            .get(name)
            .ok_or_else(|| anyhow!("rust-analyzer returned a symbol without `{name}`"))?;
        range_to_offsets(&text, range)
    };
    let (start, end) = signature_span(
        &text,
        range("range")?,
        range("selectionRange")?,
        fn_in_signature == 0,
    )?;

    let old_signature = text[start..end].trim_end().to_string();
    let body_follows = text[end..].starts_with('{');
    let mut updated = String::with_capacity(text.len() + new_signature.len());
    updated.push_str(&text[..start]);
    updated.push_str(new_signature);
    if body_follows {
        updated.push(' ');
    }
    updated.push_str(&text[end..]);
    write_source(&file, &updated)?;

    // The name sits right after `fn` in the new declaration; everything that
    // refers to it still has the old argument list.
    let after_fn = &new_signature[fn_in_signature + 2..];
    let name_offset = start + fn_in_signature + 2 + (after_fn.len() - after_fn.trim_start().len());
    let (name_line, name_character) = offset_to_position(&updated, name_offset);
    let references = analyzer
        .find_references(file_path, name_line, name_character)
        .await?;
    let file_key = path_key(&file);
    let is_declaration = |location: &Value| {
        let start = location.get("range").and_then(|range| range.get("start"));
        let at = |name: &str| start.and_then(|s| s.get(name)).and_then(Value::as_u64);
        location
            .get("uri")
            .and_then(Value::as_str)
            .is_some_and(|uri| path_key(&file_uri_to_path(uri)) == file_key)
            && at("line") == Some(u64::from(name_line))
            && at("character") == Some(u64::from(name_character))
    };
    let others: Vec<Value> = references
        .as_array()
        .into_iter()
        .flatten()
        .filter(|location| !is_declaration(location))
        .cloned()
        .collect();
    let call_sites = format_locations(&Value::Array(others));

    let mut out = vec![
        format!("Changed signature at {file_path}:{line}:{character}"),
        format!("  was: {old_signature}"),
        format!("  now: {new_signature}"),
    ];
    if call_sites.is_empty() {
        out.push("No other references found.".to_string());
    } else {
        out.push(format!(
            "{} reference(s) were NOT updated and may need to change (path:line:character, 0-based):",
            call_sites.len()
        ));
        out.extend(call_sites.iter().map(|site| format!("  {site}")));
    }
    out.push(
        "Only the declaration was rewritten; trait implementations of this function are not \
         updated either. Use run_cargo_check to find what no longer compiles."
            .to_string(),
    );

    Ok(ToolResult::text(out.join("\n")))
}

/// Whether an unindented line starts a `use` declaration, with or without a
/// visibility (`pub`, `pub(crate)`, ...).
fn is_use_line(line: &str) -> bool {
    let mut rest = line;
    if let Some(after_pub) = rest.strip_prefix("pub") {
        rest = after_pub.trim_start();
        if rest.starts_with('(') {
            match rest.find(')') {
                Some(close) => rest = rest[close + 1..].trim_start(),
                None => return false,
            }
        }
    }
    rest.starts_with("use ")
}

/// The byte range covering a file's top-level `use` declarations, from the
/// start of the first to the end of the last.
pub fn import_block(text: &str) -> Option<(usize, usize)> {
    let mut first = None;
    let mut last_end = None;
    let mut offset = 0;

    for line in text.split_inclusive('\n') {
        if is_use_line(line) {
            first.get_or_insert(offset);
            // A `use` can span several lines; it ends at its semicolon.
            let end = code_chars(text, offset)
                .find(|(_, c)| *c == ';')
                .map(|(i, _)| i + 1)?;
            last_end = Some(end);
        }
        offset += line.len();
    }

    Some((first?, last_end?))
}

pub async fn organize_imports_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;

    let file = normalize_path(Path::new(file_path));
    let text = read_source(&file)?;
    let Some((start, end)) = import_block(&text) else {
        return Ok(ToolResult::text(format!(
            "{file_path} has no top-level imports to organize"
        )));
    };

    let mut out = Vec::new();

    let actions = analyzer
        .code_actions(
            file_path,
            offset_to_position(&text, start),
            offset_to_position(&text, end),
        )
        .await?;
    match pick_action(
        &actions,
        &["remove_unused_imports"],
        &["Remove all unused imports"],
    ) {
        Some(action) => {
            let summary = analyzer.apply_code_action(action).await?;
            out.push(format!(
                "Removed unused imports ({} edit(s))",
                summary.edit_count()
            ));
        }
        None => out.push("No unused imports found".to_string()),
    }

    // rustfmt sorts each group of imports. It formats whole files, so the rest
    // of the file is formatted along with them.
    let edits = analyzer.format_code(file_path).await?;
    out.push(if edits == 0 {
        "Imports were already sorted (file is rustfmt-clean)".to_string()
    } else {
        format!("Sorted imports by formatting the file with rustfmt ({edits} edit(s))")
    });

    Ok(ToolResult::text(format!(
        "Organized imports in {file_path}\n  {}",
        out.join("\n  ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn actions_are_picked_by_assist_id_before_title() {
        let actions = [
            json!({ "title": "Extract into variable", "data": { "id": "extract_variable:RefactorExtract:0" } }),
            json!({ "title": "Extract into function", "data": { "id": "extract_function:RefactorExtract:1" } }),
            json!({ "title": "Inline `add`" }),
        ];

        let by_id = pick_action(&actions, &["extract_function"], &[]).unwrap();
        assert_eq!(by_id["title"], "Extract into function");

        let by_title = pick_action(&actions, &["inline_call"], &["Inline `"]).unwrap();
        assert_eq!(by_title["title"], "Inline `add`");

        assert!(pick_action(&actions, &["merge_imports"], &["Merge imports"]).is_none());
        assert!(describe_actions(&actions).contains("Extract into variable; Extract"));
    }

    #[test]
    fn generated_function_is_found_even_when_the_placeholder_is_taken() {
        let before = "fn fun_name() {}\nfn main() { let x = 1 + 2; }\n";
        let after = "fn fun_name() {}\nfn main() { let x = fun_name1(); }\n\nfn fun_name1() -> i32 {\n    1 + 2\n}\n";

        let (name, offset) = find_generated_function(before, after).unwrap();

        assert_eq!(name, "fun_name1");
        assert!(after[offset..].starts_with("fun_name1() -> i32"));
    }

    #[test]
    fn signature_span_stops_at_the_body() {
        let text = "#[inline]\npub fn area(w: [u8; 2], h: f64) -> f64\nwhere\n    f64: Copy,\n{\n    0.0\n}\n";
        let item = (0, text.len());
        let name_start = text.find("area").unwrap();
        let name = (name_start, name_start + 4);

        let (start, end) = signature_span(text, item, name, false).unwrap();
        assert!(text[start..end].starts_with("pub fn area("));
        assert!(text[start..end].trim_end().ends_with("f64: Copy,"));
        assert!(text[end..].starts_with('{'));

        let (start, _) = signature_span(text, item, name, true).unwrap();
        assert!(text[start..].starts_with("fn area("));
    }

    #[test]
    fn signature_span_handles_trait_method_declarations() {
        let text = "trait Shape {\n    fn area(&self) -> f64;\n}\n";
        let item_start = text.find("fn area").unwrap();
        let item = (item_start, text.find(";\n").unwrap() + 1);
        let name = (item_start + 3, item_start + 7);

        let (start, end) = signature_span(text, item, name, false).unwrap();
        assert_eq!(&text[start..end], "fn area(&self) -> f64");
    }

    #[test]
    fn import_block_covers_all_top_level_uses() {
        let text = "//! Docs\nuse std::fmt;\npub use crate::a::{\n    B,\n    C,\n};\n\npub(crate) use x::y;\n\npub fn user() {}\n";
        let (start, end) = import_block(text).unwrap();
        assert!(text[start..].starts_with("use std::fmt;"));
        assert!(text[..end].ends_with("pub(crate) use x::y;"));

        assert!(import_block("pub fn user() {}\npub struct Used;\n").is_none());
    }
}
