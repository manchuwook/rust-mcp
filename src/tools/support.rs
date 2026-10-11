// Helpers shared by the tool implementations: argument parsing, presenting
// LSP results, and small source-text utilities.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;

use crate::analyzer::edit::position_to_offset;
use crate::analyzer::protocol::file_uri_to_path;

pub fn str_arg<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args.get(name)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Missing {name} parameter"))
}

pub fn u32_arg(args: &Value, name: &str) -> Result<u32> {
    let value = args
        .get(name)
        .and_then(|v| v.as_u64())
        .ok_or_else(|| anyhow!("Missing {name} parameter"))?;
    u32::try_from(value).map_err(|_| anyhow!("{name} parameter is out of range"))
}

pub fn array_arg<'a>(args: &'a Value, name: &str) -> Result<&'a Vec<Value>> {
    args.get(name)
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("Missing {name} parameter"))
}

/// An optional array of strings; absent or `null` is an empty list.
pub fn str_list_arg<'a>(args: &'a Value, name: &str) -> Vec<&'a str> {
    args.get(name)
        .and_then(|v| v.as_array())
        .map(|v| v.iter().filter_map(|d| d.as_str()).collect())
        .unwrap_or_default()
}

pub fn read_source(path: &Path) -> Result<String> {
    std::fs::read_to_string(path).with_context(|| format!("failed to read {}", path.display()))
}

pub fn write_source(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    std::fs::write(path, text).with_context(|| format!("failed to write {}", path.display()))
}

/// Check that `name` can be used as a Rust identifier.
pub fn check_identifier(name: &str, what: &str) -> Result<()> {
    let name = name.strip_prefix("r#").unwrap_or(name);
    let mut chars = name.chars();
    let valid = match chars.next() {
        Some(first) => {
            (first.is_alphabetic() || first == '_')
                && chars.all(|c| c.is_alphanumeric() || c == '_')
        }
        None => false,
    };
    if !valid {
        bail!("`{name}` is not a valid {what}");
    }
    Ok(())
}

/// Whether `text` contains `word` as a whole identifier (not as part of a longer one).
pub fn contains_word(text: &str, word: &str) -> bool {
    find_word(text, word, 0).is_some()
}

/// Find `word` as a whole identifier at or after byte offset `from`.
pub fn find_word(text: &str, word: &str, from: usize) -> Option<usize> {
    let is_ident = |c: char| c.is_alphanumeric() || c == '_';
    let mut search_from = from;
    while let Some(found) = text[search_from..].find(word) {
        let start = search_from + found;
        let end = start + word.len();
        let before_ok = !text[..start].chars().next_back().is_some_and(is_ident);
        let after_ok = !text[end..].chars().next().is_some_and(is_ident);
        if before_ok && after_ok {
            return Some(start);
        }
        search_from = end;
    }
    None
}

/// Append a top-level item to a source file, separated from what is already
/// there by a blank line. Creates the file (and its directory) if needed and
/// follows the file's existing line-ending style.
pub fn append_item(path: &Path, code: &str) -> Result<()> {
    let existing = if path.exists() {
        read_source(path)?
    } else {
        String::new()
    };

    let mut updated = existing.clone();
    if !updated.is_empty() {
        if !updated.ends_with('\n') {
            updated.push('\n');
        }
        updated.push('\n');
    }
    updated.push_str(code.trim_end());
    updated.push('\n');

    if existing.contains("\r\n") {
        updated = updated.replace("\r\n", "\n").replace('\n', "\r\n");
    }
    write_source(path, &updated)
}

/// Given the byte offset of an opening `{`, find its matching `}`.
///
/// Braces inside comments, string literals and char literals are ignored.
pub fn find_matching_brace(text: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    for (offset, c) in code_chars(text, open) {
        match c {
            '{' => depth += 1,
            '}' => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// The identifiers and keywords in `text` with their byte offsets, ignoring
/// comments, string literals and char literals.
pub fn code_identifiers(text: &str) -> Vec<(usize, &str)> {
    let mut identifiers = Vec::new();
    let mut current: Option<(usize, usize)> = None;

    let mut finish = |current: &mut Option<(usize, usize)>| {
        if let Some((start, end)) = current.take() {
            // A leading digit means a number literal such as `2_f64`.
            if !text[start..].starts_with(|c: char| c.is_ascii_digit()) {
                identifiers.push((start, &text[start..end]));
            }
        }
    };

    for (offset, c) in code_chars(text, 0) {
        let end = offset + c.len_utf8();
        if c.is_alphanumeric() || c == '_' {
            match &mut current {
                Some((_, current_end)) if *current_end == offset => *current_end = end,
                _ => {
                    finish(&mut current);
                    current = Some((offset, end));
                }
            }
        } else {
            finish(&mut current);
        }
    }
    finish(&mut current);

    identifiers
}

/// Iterate over the characters of `text` from byte offset `from` that are
/// actual code, skipping comments, string literals and char literals.
pub fn code_chars(text: &str, from: usize) -> impl Iterator<Item = (usize, char)> + '_ {
    let bytes = text.as_bytes();
    let mut i = from;

    std::iter::from_fn(move || {
        loop {
            let rest = text.get(i..)?;
            let c = rest.chars().next()?;

            if rest.starts_with("//") {
                i += rest.find('\n').unwrap_or(rest.len());
                continue;
            }
            if rest.starts_with("/*") {
                // Block comments nest.
                let mut depth = 0usize;
                let mut j = i;
                while j < bytes.len() {
                    if text[j..].starts_with("/*") {
                        depth += 1;
                        j += 2;
                    } else if text[j..].starts_with("*/") {
                        depth -= 1;
                        j += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        j += text[j..].chars().next().map_or(1, char::len_utf8);
                    }
                }
                i = j;
                continue;
            }
            if c == '"' {
                i = skip_string(text, i + 1);
                continue;
            }
            if c == 'r' {
                // Raw string: r"..." or r#"..."#
                let hashes = rest[1..].bytes().take_while(|b| *b == b'#').count();
                if rest[1 + hashes..].starts_with('"') {
                    let terminator = format!("\"{}", "#".repeat(hashes));
                    let body = i + 1 + hashes + 1;
                    i = match text[body..].find(&terminator) {
                        Some(end) => body + end + terminator.len(),
                        None => text.len(),
                    };
                    continue;
                }
            }
            if c == '\'' {
                // Either a char literal ('a', '\n') or a lifetime ('a).
                let mut after = rest[1..].chars();
                match (after.next(), after.next()) {
                    (Some('\\'), _) => {
                        i = match rest[2..].find('\'') {
                            // `'\''`: the first quote found is the escaped one.
                            Some(0) => i + 4,
                            Some(end) => i + 2 + end + 1,
                            None => text.len(),
                        };
                        continue;
                    }
                    (Some(inner), Some('\'')) => {
                        i += 1 + inner.len_utf8() + 1;
                        continue;
                    }
                    _ => {}
                }
            }

            let offset = i;
            i += c.len_utf8();
            return Some((offset, c));
        }
    })
}

/// `start` is the offset just past an opening `"`; returns the offset just
/// past the closing one.
fn skip_string(text: &str, start: usize) -> usize {
    let mut escaped = false;
    for (offset, c) in text[start..].char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '"' => return start + offset + 1,
            _ => {}
        }
    }
    text.len()
}

/// The source line at a 0-based line number, trimmed.
fn source_line(text: &str, line: u64) -> &str {
    let start = position_to_offset(text, line, 0);
    text[start..].lines().next().unwrap_or("").trim()
}

/// Present LSP `Location` / `LocationLink` results (a single one, an array, or
/// null) as `path:line:character  <source line>` strings. Positions are 0-based,
/// the same convention the tools accept as input.
pub fn format_locations(result: &Value) -> Vec<String> {
    let locations: Vec<&Value> = match result {
        Value::Array(items) => items.iter().collect(),
        Value::Null => Vec::new(),
        single => vec![single],
    };

    let mut formatted = Vec::new();
    for location in locations {
        let uri = location
            .get("uri")
            .or_else(|| location.get("targetUri"))
            .and_then(Value::as_str);
        let start = location
            .get("range")
            .or_else(|| location.get("targetSelectionRange"))
            .and_then(|range| range.get("start"));
        let (Some(uri), Some(start)) = (uri, start) else {
            continue;
        };

        let path = file_uri_to_path(uri);
        let line = start.get("line").and_then(Value::as_u64).unwrap_or(0);
        let character = start.get("character").and_then(Value::as_u64).unwrap_or(0);
        let entry = match std::fs::read_to_string(&path) {
            Ok(text) => format!(
                "{}:{line}:{character}  {}",
                path.display(),
                source_line(&text, line)
            ),
            Err(_) => format!("{}:{line}:{character}", path.display()),
        };
        if !formatted.contains(&entry) {
            formatted.push(entry);
        }
    }
    formatted
}

/// Name for an LSP `SymbolKind` number.
pub fn symbol_kind_name(kind: u64) -> &'static str {
    match kind {
        1 => "file",
        2 => "module",
        3 => "namespace",
        4 => "package",
        5 => "class",
        6 => "method",
        7 => "property",
        8 => "field",
        9 => "constructor",
        10 => "enum",
        11 => "interface",
        12 => "function",
        13 => "variable",
        14 => "constant",
        15 => "string",
        16 => "number",
        17 => "boolean",
        18 => "array",
        19 => "object",
        20 => "key",
        21 => "null",
        22 => "enum member",
        23 => "struct",
        24 => "event",
        25 => "operator",
        26 => "type parameter",
        _ => "symbol",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matching_brace_ignores_strings_comments_and_chars() {
        let text = r##"mod tests { // }
    const A: &str = "}";
    const B: &str = r#"}"#;
    const C: char = '}';
    const D: char = '\'';
    /* } /* } */ } */
    fn f<'a>(x: &'a str) -> &'a str { x }
}
fn after() {}"##;
        let open = text.find('{').unwrap();
        let close = find_matching_brace(text, open).unwrap();
        assert_eq!(&text[close..close + 2], "}\n");
        assert!(text[close..].starts_with("}\nfn after"));
    }

    #[test]
    fn word_search_respects_identifier_boundaries() {
        assert!(contains_word("struct Foo {", "Foo"));
        assert!(!contains_word("struct FooBar {", "Foo"));
        assert_eq!(find_word("fun_name1 fun_name", "fun_name", 0), Some(10));
    }

    #[test]
    fn identifiers_are_validated() {
        assert!(check_identifier("my_mod", "module name").is_ok());
        assert!(check_identifier("r#type", "module name").is_ok());
        assert!(check_identifier("9lives", "module name").is_err());
        assert!(check_identifier("a-b", "module name").is_err());
        assert!(check_identifier("", "module name").is_err());
    }

    #[test]
    fn append_item_separates_with_a_blank_line() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("nested").join("lib.rs");

        append_item(&file, "struct A;\n").unwrap();
        append_item(&file, "struct B;").unwrap();

        assert_eq!(read_source(&file).unwrap(), "struct A;\n\nstruct B;\n");
    }

    #[test]
    fn append_item_keeps_crlf_line_endings() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("lib.rs");
        std::fs::write(&file, "struct A;\r\n").unwrap();

        append_item(&file, "struct B {\n    x: i32,\n}").unwrap();

        assert_eq!(
            read_source(&file).unwrap(),
            "struct A;\r\n\r\nstruct B {\r\n    x: i32,\r\n}\r\n"
        );
    }
}
