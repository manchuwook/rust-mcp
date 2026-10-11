// Applying LSP edits (TextEdit / WorkspaceEdit) to files on disk.

use anyhow::{Context, Result, anyhow, bail};
use serde_json::Value;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::analyzer::protocol::file_uri_to_path;

/// What a `WorkspaceEdit` changed on disk.
#[derive(Debug, Default)]
pub struct EditSummary {
    /// Edited files with the number of text edits applied to each.
    pub edited: Vec<(PathBuf, usize)>,
    pub created: Vec<PathBuf>,
    pub renamed: Vec<(PathBuf, PathBuf)>,
    pub deleted: Vec<PathBuf>,
}

impl EditSummary {
    pub fn is_empty(&self) -> bool {
        self.edited.is_empty()
            && self.created.is_empty()
            && self.renamed.is_empty()
            && self.deleted.is_empty()
    }

    pub fn edit_count(&self) -> usize {
        self.edited.iter().map(|(_, count)| count).sum()
    }
}

impl fmt::Display for EditSummary {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return write!(f, "no changes");
        }
        let mut lines = Vec::new();
        for (path, count) in &self.edited {
            lines.push(format!("  edited {} ({count} edit(s))", path.display()));
        }
        for path in &self.created {
            lines.push(format!("  created {}", path.display()));
        }
        for (from, to) in &self.renamed {
            lines.push(format!("  renamed {} -> {}", from.display(), to.display()));
        }
        for path in &self.deleted {
            lines.push(format!("  deleted {}", path.display()));
        }
        write!(f, "{}", lines.join("\n"))
    }
}

/// Convert an LSP position (0-based line, UTF-16 code unit offset) to a byte
/// offset into `text`. Positions past the end of a line or file are clamped.
pub fn position_to_offset(text: &str, line: u64, character: u64) -> usize {
    let mut offset = 0;
    let mut rest = text;
    for _ in 0..line {
        match rest.find('\n') {
            Some(i) => {
                offset += i + 1;
                rest = &rest[i + 1..];
            }
            None => return text.len(),
        }
    }

    let line_text = rest.split('\n').next().unwrap_or("");
    let line_text = line_text.strip_suffix('\r').unwrap_or(line_text);

    let mut units = 0u64;
    for (i, c) in line_text.char_indices() {
        if units >= character {
            return offset + i;
        }
        units += c.len_utf16() as u64;
    }
    offset + line_text.len()
}

/// Convert a byte offset (on a char boundary) to an LSP `(line, character)` position.
pub fn offset_to_position(text: &str, offset: usize) -> (u32, u32) {
    let before = &text[..offset];
    let line = before.matches('\n').count() as u32;
    let line_start = before.rfind('\n').map_or(0, |i| i + 1);
    let character = before[line_start..].encode_utf16().count() as u32;
    (line, character)
}

/// Convert an LSP range object to a `(start, end)` byte offset pair.
pub fn range_to_offsets(text: &str, range: &Value) -> Result<(usize, usize)> {
    let point = |name: &str| -> Result<usize> {
        let position = range
            .get(name)
            .ok_or_else(|| anyhow!("range is missing `{name}`"))?;
        let line = position.get("line").and_then(Value::as_u64);
        let character = position.get("character").and_then(Value::as_u64);
        match (line, character) {
            (Some(line), Some(character)) => Ok(position_to_offset(text, line, character)),
            _ => Err(anyhow!("malformed position in range: {position}")),
        }
    };
    Ok((point("start")?, point("end")?))
}

/// Apply a list of LSP `TextEdit`s to `text` and return the result.
///
/// All edits refer to positions in the original text. Inserts at the same
/// position are applied in the order given, as the LSP specification requires.
pub fn apply_text_edits(text: &str, edits: &[Value]) -> Result<String> {
    let mut resolved = Vec::with_capacity(edits.len());
    for edit in edits {
        let range = edit
            .get("range")
            .ok_or_else(|| anyhow!("text edit is missing `range`"))?;
        let new_text = edit
            .get("newText")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow!("text edit is missing `newText`"))?;
        let (start, end) = range_to_offsets(text, range)?;
        if end < start {
            bail!("text edit has an inverted range: {range}");
        }
        resolved.push((start, end, new_text));
    }
    // Stable sort keeps same-position inserts in their original order.
    resolved.sort_by_key(|(start, _, _)| *start);

    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, new_text) in resolved {
        if start < cursor {
            bail!("text edits overlap at byte offset {start}");
        }
        result.push_str(&text[cursor..start]);
        result.push_str(new_text);
        cursor = end;
    }
    result.push_str(&text[cursor..]);
    Ok(result)
}

/// Apply `TextEdit`s to a file on disk. Returns the number of edits applied.
pub fn apply_text_edits_to_file(path: &Path, edits: &[Value]) -> Result<usize> {
    if edits.is_empty() {
        return Ok(0);
    }
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let updated = apply_text_edits(&text, edits)?;
    std::fs::write(path, updated).with_context(|| format!("failed to write {}", path.display()))?;
    Ok(edits.len())
}

fn uri_field(value: &Value, name: &str) -> Result<PathBuf> {
    value
        .get(name)
        .and_then(Value::as_str)
        .map(file_uri_to_path)
        .ok_or_else(|| anyhow!("workspace edit entry is missing `{name}`"))
}

fn option_flag(operation: &Value, name: &str) -> bool {
    operation
        .get("options")
        .and_then(|options| options.get(name))
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

fn record_edit(summary: &mut EditSummary, path: PathBuf, count: usize) {
    if count == 0 {
        return;
    }
    match summary.edited.iter_mut().find(|(p, _)| *p == path) {
        Some((_, total)) => *total += count,
        None => summary.edited.push((path, count)),
    }
}

fn apply_resource_operation(
    operation: &Value,
    kind: &str,
    summary: &mut EditSummary,
) -> Result<()> {
    match kind {
        "create" => {
            let path = uri_field(operation, "uri")?;
            if path.exists() && !option_flag(operation, "overwrite") {
                if option_flag(operation, "ignoreIfExists") {
                    return Ok(());
                }
                bail!("cannot create {}: file already exists", path.display());
            }
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, "")
                .with_context(|| format!("failed to create {}", path.display()))?;
            summary.created.push(path);
        }
        "rename" => {
            let from = uri_field(operation, "oldUri")?;
            let to = uri_field(operation, "newUri")?;
            if to.exists() && !option_flag(operation, "overwrite") {
                if option_flag(operation, "ignoreIfExists") {
                    return Ok(());
                }
                bail!("cannot rename to {}: file already exists", to.display());
            }
            if let Some(parent) = to.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::rename(&from, &to).with_context(|| {
                format!("failed to rename {} to {}", from.display(), to.display())
            })?;
            summary.renamed.push((from, to));
        }
        "delete" => {
            let path = uri_field(operation, "uri")?;
            if !path.exists() {
                if option_flag(operation, "ignoreIfNotExists") {
                    return Ok(());
                }
                bail!("cannot delete {}: file does not exist", path.display());
            }
            if path.is_dir() {
                std::fs::remove_dir_all(&path)?;
            } else {
                std::fs::remove_file(&path)?;
            }
            summary.deleted.push(path);
        }
        other => bail!("unsupported resource operation `{other}` in workspace edit"),
    }
    Ok(())
}

/// Apply an LSP `WorkspaceEdit` to disk.
///
/// Supports both the `documentChanges` form (text edits plus create / rename /
/// delete file operations, applied in order) and the older `changes` map.
pub fn apply_workspace_edit(edit: &Value) -> Result<EditSummary> {
    let mut summary = EditSummary::default();

    if let Some(document_changes) = edit.get("documentChanges").and_then(Value::as_array) {
        for change in document_changes {
            if let Some(kind) = change.get("kind").and_then(Value::as_str) {
                apply_resource_operation(change, kind, &mut summary)?;
                continue;
            }
            let path = change
                .get("textDocument")
                .ok_or_else(|| anyhow!("document change is missing `textDocument`"))
                .and_then(|document| uri_field(document, "uri"))?;
            let edits = change
                .get("edits")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow!("document change is missing `edits`"))?;
            let count = apply_text_edits_to_file(&path, edits)?;
            record_edit(&mut summary, path, count);
        }
    } else if let Some(changes) = edit.get("changes").and_then(Value::as_object) {
        for (uri, edits) in changes {
            let path = file_uri_to_path(uri);
            let edits = edits
                .as_array()
                .ok_or_else(|| anyhow!("`changes` entry for {uri} is not an array"))?;
            let count = apply_text_edits_to_file(&path, edits)?;
            record_edit(&mut summary, path, count);
        }
    }

    Ok(summary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::protocol::path_to_file_uri;
    use serde_json::json;

    fn edit(start: (u64, u64), end: (u64, u64), new_text: &str) -> Value {
        json!({
            "range": {
                "start": { "line": start.0, "character": start.1 },
                "end": { "line": end.0, "character": end.1 }
            },
            "newText": new_text
        })
    }

    #[test]
    fn positions_count_utf16_units() {
        // '😀' is 4 bytes in UTF-8 and 2 code units in UTF-16.
        let text = "let s = \"😀\"; // é\nnext";
        let after_emoji = text.find("\";").unwrap();
        assert_eq!(position_to_offset(text, 0, 11), after_emoji);
        assert_eq!(offset_to_position(text, after_emoji), (0, 11));
        assert_eq!(position_to_offset(text, 1, 2), text.find("xt").unwrap());
    }

    #[test]
    fn positions_clamp_and_ignore_carriage_returns() {
        let text = "ab\r\ncd";
        assert_eq!(position_to_offset(text, 0, 99), 2);
        assert_eq!(position_to_offset(text, 1, 1), 5);
        assert_eq!(position_to_offset(text, 9, 0), text.len());
    }

    #[test]
    fn edits_apply_against_original_positions() {
        let text = "fn add(a: i32) {}\nfn main() { add(1); }\n";
        let edits = [edit((1, 12), (1, 15), "sum"), edit((0, 3), (0, 6), "sum")];
        assert_eq!(
            apply_text_edits(text, &edits).unwrap(),
            "fn sum(a: i32) {}\nfn main() { sum(1); }\n"
        );
    }

    #[test]
    fn same_position_inserts_keep_their_order() {
        let edits = [edit((0, 1), (0, 1), "1"), edit((0, 1), (0, 1), "2")];
        assert_eq!(apply_text_edits("ab", &edits).unwrap(), "a12b");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let edits = [edit((0, 0), (0, 3), "x"), edit((0, 2), (0, 4), "y")];
        assert!(apply_text_edits("abcdef", &edits).is_err());
    }

    #[test]
    fn workspace_edit_creates_edits_and_renames_files() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("a.rs");
        let created = dir.path().join("nested").join("b.rs");
        let renamed = dir.path().join("c.rs");
        std::fs::write(&existing, "old\n").unwrap();

        let uri = |path: &Path| path_to_file_uri(&path.to_string_lossy());
        let workspace_edit = json!({
            "documentChanges": [
                {
                    "textDocument": { "uri": uri(&existing), "version": 1 },
                    "edits": [edit((0, 0), (0, 3), "new")]
                },
                { "kind": "create", "uri": uri(&created) },
                {
                    "textDocument": { "uri": uri(&created), "version": null },
                    "edits": [edit((0, 0), (0, 0), "fn b() {}\n")]
                },
                { "kind": "rename", "oldUri": uri(&existing), "newUri": uri(&renamed) }
            ]
        });

        let summary = apply_workspace_edit(&workspace_edit).unwrap();
        assert_eq!(summary.edit_count(), 2);
        assert_eq!(summary.created.len(), 1);
        assert_eq!(summary.renamed.len(), 1);
        assert!(!existing.exists());
        assert_eq!(std::fs::read_to_string(&renamed).unwrap(), "new\n");
        assert_eq!(std::fs::read_to_string(&created).unwrap(), "fn b() {}\n");
    }

    #[test]
    fn workspace_edit_changes_map_is_supported() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.rs");
        std::fs::write(&file, "abc").unwrap();

        let mut changes = serde_json::Map::new();
        changes.insert(
            path_to_file_uri(&file.to_string_lossy()),
            json!([edit((0, 1), (0, 2), "X")]),
        );
        let summary = apply_workspace_edit(&json!({ "changes": changes })).unwrap();

        assert_eq!(summary.edit_count(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "aXc");
    }
}
