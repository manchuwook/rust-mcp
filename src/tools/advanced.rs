use crate::analyzer::RustAnalyzerClient;
use crate::analyzer::edit::range_to_offsets;
use crate::analyzer::protocol::{normalize_path, path_key};
use crate::tools::cargo::{cargo_dir, cargo_metadata, cargo_output, stderr_tail};
use crate::tools::support::{
    append_item, check_identifier, find_word, format_locations, read_source, str_arg, u32_arg,
    write_source,
};
use crate::tools::types::ToolResult;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The text of an LSP hover result, whichever of its shapes it arrives in.
fn hover_text(hover: &Value) -> String {
    fn text_of(contents: &Value) -> String {
        match contents {
            Value::String(text) => text.clone(),
            Value::Array(parts) => parts.iter().map(text_of).collect::<Vec<_>>().join("\n"),
            Value::Object(_) => contents
                .get("value")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            _ => String::new(),
        }
    }
    hover.get("contents").map(text_of).unwrap_or_default()
}

pub async fn get_type_hierarchy_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;
    let line = u32_arg(&args, "line")?;
    let character = u32_arg(&args, "character")?;

    // rust-analyzer does not implement LSP type hierarchy, so this is built
    // from what it does offer: the declaration, and the impls related to it
    // (a trait's implementors, or a type's impl blocks).
    let hover = analyzer.hover(file_path, line, character).await?;
    let implementations = analyzer
        .find_implementations(file_path, line, character)
        .await?;

    let declaration = hover_text(&hover);
    let implementations = format_locations(&implementations);
    if declaration.trim().is_empty() && implementations.is_empty() {
        bail!("No type or trait found at {file_path}:{line}:{character}");
    }

    let mut out = Vec::new();
    if !declaration.trim().is_empty() {
        out.push(declaration.trim().to_string());
        out.push(String::new());
    }
    if implementations.is_empty() {
        out.push("No implementations found.".to_string());
    } else {
        out.push(format!(
            "{} implementation(s) (path:line:character, 0-based):",
            implementations.len()
        ));
        out.extend(
            implementations
                .iter()
                .map(|location| format!("  {location}")),
        );
    }

    Ok(ToolResult::text(out.join("\n")))
}

/// Parse `cargo search` output lines: `name = "version"    # description`.
fn parse_search_results(stdout: &str) -> Vec<(String, String, String)> {
    stdout
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(" = \"")?;
            let (version, rest) = rest.split_once('"')?;
            let description = rest.trim().trim_start_matches('#').trim();
            Some((
                name.trim().to_string(),
                version.to_string(),
                description.to_string(),
            ))
        })
        .collect()
}

pub async fn suggest_dependencies_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let query = str_arg(&args, "query")?;
    let workspace_path = str_arg(&args, "workspace_path")?;

    let dir = cargo_dir(workspace_path)?;
    let output = cargo_output(&["search", query, "--limit", "10"], &dir).await?;
    if !output.status.success() {
        bail!(
            "`cargo search` failed (it needs network access to crates.io):\n{}",
            stderr_tail(&String::from_utf8_lossy(&output.stderr))
        );
    }
    let results = parse_search_results(&String::from_utf8_lossy(&output.stdout));
    if results.is_empty() {
        return Ok(ToolResult::text(format!(
            "No crates found on crates.io for '{query}'"
        )));
    }

    // Mark the ones the workspace already depends on, when it has a manifest.
    let existing: Vec<String> = match cargo_metadata(&dir.to_string_lossy()).await {
        Ok(metadata) => metadata
            .get("packages")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|package| package.get("dependencies").and_then(Value::as_array))
            .flatten()
            .filter_map(|dependency| dependency.get("name").and_then(Value::as_str))
            .map(str::to_string)
            .collect(),
        Err(_) => Vec::new(),
    };

    let mut out = vec![format!("Crates on crates.io matching '{query}':")];
    for (name, version, description) in &results {
        let marker = if existing.contains(name) {
            " (already a dependency)"
        } else {
            ""
        };
        out.push(format!("  {name} {version}{marker} - {description}"));
    }
    out.push("Add one with `cargo add <name>`.".to_string());

    Ok(ToolResult::text(out.join("\n")))
}

/// Work out where a new module's file goes and which file declares it.
///
/// `module_path` is either the directory to create the module in, or the
/// parent module's file. Returns `(new_file, parent_file)`.
fn plan_module(module_name: &str, module_path: &str) -> Result<(PathBuf, Option<PathBuf>)> {
    let path = normalize_path(Path::new(module_path));

    let (dir, parent_file) = if path.is_file() || path.extension().is_some_and(|ext| ext == "rs") {
        if !path.is_file() {
            bail!("parent module {} does not exist", path.display());
        }
        let folder = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy();
        // `foo.rs` keeps its children in `foo/`; mod.rs, lib.rs and main.rs
        // keep theirs alongside.
        let dir = match stem.as_ref() {
            "mod" | "lib" | "main" => folder,
            other => folder.join(other),
        };
        (dir, Some(path))
    } else {
        let mut candidates = vec![
            path.join("mod.rs"),
            path.join("lib.rs"),
            path.join("main.rs"),
        ];
        candidates.push(path.with_extension("rs"));
        let parent_file = candidates.into_iter().find(|candidate| candidate.is_file());
        (path, parent_file)
    };

    let new_file = dir.join(format!("{module_name}.rs"));
    let as_directory = dir.join(module_name).join("mod.rs");
    for existing in [&new_file, &as_directory] {
        if existing.exists() {
            bail!("module already exists at {}", existing.display());
        }
    }
    Ok((new_file, parent_file))
}

/// Whether an unindented line is a `mod name;` declaration.
fn is_mod_declaration(line: &str) -> bool {
    let line = line.trim_end();
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
    rest.starts_with("mod ") && line.ends_with(';')
}

/// Add a `mod` declaration to a module's source: after its existing `mod`
/// declarations, or else at the top below any inner doc comments/attributes.
pub fn insert_mod_declaration(source: &str, declaration: &str) -> String {
    let crlf = source.contains("\r\n");
    let text = source.replace("\r\n", "\n");
    let mut lines: Vec<&str> = text.lines().collect();

    match lines.iter().rposition(|line| is_mod_declaration(line)) {
        Some(last) => lines.insert(last + 1, declaration),
        None => {
            let header = lines
                .iter()
                .take_while(|line| line.starts_with("//!") || line.starts_with("#!["))
                .count();
            let mut index = header;
            if header > 0 {
                lines.insert(index, "");
                index += 1;
            }
            lines.insert(index, declaration);
            if lines.get(index + 1).is_some_and(|next| !next.is_empty()) {
                lines.insert(index + 1, "");
            }
        }
    }

    let updated = lines.join("\n") + "\n";
    if crlf {
        updated.replace('\n', "\r\n")
    } else {
        updated
    }
}

pub async fn create_module_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let module_name = str_arg(&args, "module_name")?;
    let module_path = str_arg(&args, "module_path")?;
    let is_public = args
        .get("is_public")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    check_identifier(module_name, "module name")?;

    let (new_file, parent_file) = plan_module(module_name, module_path)?;
    let visibility = if is_public { "pub " } else { "" };
    let declaration = format!("{visibility}mod {module_name};");

    // Check the parent before creating anything, so a failure leaves no trace.
    let parent = match &parent_file {
        Some(parent_file) => {
            let source = read_source(parent_file)?;
            if find_word(&source, &format!("mod {module_name}"), 0).is_some() {
                bail!(
                    "{} already declares `mod {module_name}`",
                    parent_file.display()
                );
            }
            Some((parent_file, insert_mod_declaration(&source, &declaration)))
        }
        None => None,
    };

    write_source(&new_file, "")?;
    let mut out = vec![format!(
        "Created {visibility}module '{module_name}' at {}",
        new_file.display()
    )];
    match parent {
        Some((parent_file, updated)) => {
            write_source(parent_file, &updated)?;
            out.push(format!(
                "Declared `{declaration}` in {}",
                parent_file.display()
            ));
        }
        None => out.push(format!(
            "No parent module file (mod.rs, lib.rs or main.rs) was found for it, so nothing \
             declares it yet; add `{declaration}` to the parent module."
        )),
    }

    Ok(ToolResult::text(out.join("\n")))
}

/// Remove byte ranges (sorted, non-overlapping) from `text`, along with the
/// blank line each one would otherwise leave behind.
pub fn remove_ranges(text: &str, ranges: &[(usize, usize)]) -> String {
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;

    fn push_kept(result: &mut String, mut kept: &str, after_removal: bool) {
        if after_removal {
            // Finish the removed item's last line, then drop the blank lines
            // that separated it from what follows.
            kept = kept.strip_prefix('\n').unwrap_or(kept);
            while (result.is_empty() || result.ends_with("\n\n")) && kept.starts_with('\n') {
                kept = &kept[1..];
            }
        }
        result.push_str(kept);
    }

    for (index, (start, end)) in ranges.iter().enumerate() {
        push_kept(&mut result, &text[cursor..*start], index > 0);
        cursor = *end;
    }
    push_kept(&mut result, &text[cursor..], !ranges.is_empty());

    while result.ends_with("\n\n") {
        result.pop();
    }
    result
}

pub async fn move_items_impl(args: Value, analyzer: &mut RustAnalyzerClient) -> Result<ToolResult> {
    let source_file = str_arg(&args, "source_file")?;
    let target_file = str_arg(&args, "target_file")?;
    let item_names_value = args
        .get("item_names")
        .ok_or_else(|| anyhow!("Missing item_names parameter"))?;

    let item_names: Vec<&str> = item_names_value
        .as_array()
        .ok_or_else(|| anyhow!("item_names must be an array"))?
        .iter()
        .map(|v| v.as_str().unwrap_or("").trim())
        .collect();
    if item_names.is_empty() {
        bail!("item_names is empty");
    }

    let source = normalize_path(Path::new(source_file));
    let target = normalize_path(Path::new(target_file));
    if path_key(&source) == path_key(&target) {
        bail!("source_file and target_file are the same file");
    }

    let symbols = analyzer.document_symbols(source_file).await?;
    let symbols = symbols.as_array().map_or(&[][..], Vec::as_slice);
    let original = read_source(&source)?;
    // Positions are line-based, so they are unaffected by line-ending style.
    let text = original.replace("\r\n", "\n");

    // Resolve every name before changing anything.
    fn symbol_name(symbol: &Value) -> &str {
        symbol.get("name").and_then(Value::as_str).unwrap_or("")
    }
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut missing = Vec::new();
    for name in &item_names {
        let mut found = false;
        for symbol in symbols.iter().filter(|symbol| symbol_name(symbol) == *name) {
            let range = symbol
                .get("range")
                .ok_or_else(|| anyhow!("rust-analyzer returned a symbol without a range"))?;
            let offsets = range_to_offsets(&text, range)?;
            if !ranges.contains(&offsets) {
                ranges.push(offsets);
            }
            found = true;
        }
        if !found {
            missing.push(*name);
        }
    }
    if !missing.is_empty() {
        let available: Vec<&str> = symbols.iter().map(symbol_name).collect();
        bail!(
            "No top-level item named {} in {}. Top-level items: {}",
            missing.join(", "),
            source.display(),
            available.join(", ")
        );
    }
    ranges.sort();

    let moved: Vec<&str> = ranges
        .iter()
        .map(|(start, end)| &text[*start..*end])
        .collect();
    let target_existed = target.exists();
    append_item(&target, &moved.join("\n\n"))?;

    let mut remaining = remove_ranges(&text, &ranges);
    if original.contains("\r\n") {
        remaining = remaining.replace('\n', "\r\n");
    }
    write_source(&source, &remaining)?;

    let mut out = vec![format!(
        "Moved {} item(s) from {} to {}: {}",
        ranges.len(),
        source.display(),
        target.display(),
        item_names.join(", ")
    )];
    if !target_existed {
        out.push(format!(
            "{} was created; it still needs a `mod` declaration (see create_module).",
            target.display()
        ));
    }
    out.push(
        "Imports, visibility and paths that refer to the moved items were NOT adjusted. Use \
         run_cargo_check to find what needs updating."
            .to_string(),
    );

    Ok(ToolResult::text(out.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_results_are_parsed() {
        let stdout = "serde = \"1.0.219\"    # A generic serialization framework\n\
                      serde_json = \"1.0.140\"\n\
                      ... and 1200 crates more (use --limit N to see more)\n";

        let results = parse_search_results(stdout);

        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0],
            (
                "serde".to_string(),
                "1.0.219".to_string(),
                "A generic serialization framework".to_string()
            )
        );
        assert_eq!(results[1].2, "");
    }

    #[test]
    fn mod_declaration_follows_existing_ones() {
        let source = "pub mod a;\nmod b;\n\npub use a::*;\n";
        assert_eq!(
            insert_mod_declaration(source, "pub mod c;"),
            "pub mod a;\nmod b;\npub mod c;\n\npub use a::*;\n"
        );
    }

    #[test]
    fn mod_declaration_goes_below_inner_docs_when_first() {
        let source = "//! Crate docs.\n#![allow(dead_code)]\nfn main() {}\n";
        assert_eq!(
            insert_mod_declaration(source, "mod c;"),
            "//! Crate docs.\n#![allow(dead_code)]\n\nmod c;\n\nfn main() {}\n"
        );
        assert_eq!(insert_mod_declaration("", "mod c;"), "mod c;\n");
        assert_eq!(
            insert_mod_declaration("mod a;\r\n", "mod c;"),
            "mod a;\r\nmod c;\r\n"
        );
    }

    #[test]
    fn module_plan_follows_the_parent_file_layout() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("lib.rs"), "").unwrap();
        std::fs::write(src.join("shapes.rs"), "").unwrap();

        // A directory: the parent is its lib.rs.
        let (file, parent) = plan_module("util", &src.to_string_lossy()).unwrap();
        assert_eq!(path_key(&file), path_key(&src.join("util.rs")));
        assert_eq!(path_key(&parent.unwrap()), path_key(&src.join("lib.rs")));

        // A non-root module file: children live in a directory named after it.
        let (file, parent) =
            plan_module("circle", &src.join("shapes.rs").to_string_lossy()).unwrap();
        assert_eq!(
            path_key(&file),
            path_key(&src.join("shapes").join("circle.rs"))
        );
        assert_eq!(path_key(&parent.unwrap()), path_key(&src.join("shapes.rs")));

        // Existing modules are not overwritten.
        assert!(plan_module("shapes", &src.to_string_lossy()).is_err());
    }

    #[test]
    fn removing_items_leaves_single_blank_lines() {
        let text = "use a::b;\n\nfn first() {}\n\n/// Doc.\nfn second() {}\n\nfn third() {}\n";
        let range = |needle: &str, end: &str| {
            let start = text.find(needle).unwrap();
            (start, text[start..].find(end).unwrap() + start + end.len())
        };

        let middle = remove_ranges(text, &[range("/// Doc.", "second() {}")]);
        assert_eq!(middle, "use a::b;\n\nfn first() {}\n\nfn third() {}\n");

        let ends = remove_ranges(
            text,
            &[
                range("fn first", "first() {}"),
                range("fn third", "third() {}"),
            ],
        );
        assert_eq!(ends, "use a::b;\n\n/// Doc.\nfn second() {}\n");

        let all = remove_ranges("fn only() {}\n", &[(0, 12)]);
        assert_eq!(all, "");
    }
}
