use crate::analyzer::RustAnalyzerClient;
use crate::analyzer::protocol::{normalize_path, path_key};
use crate::tools::cargo::{
    find_manifest, message_code, message_in_file, message_rendered, message_text, run_cargo_json,
    span_in_file, workspace_root,
};
use crate::tools::support::{read_source, str_arg, write_source};
use crate::tools::types::ToolResult;
use anyhow::{Result, bail};
use serde_json::Value;
use std::path::Path;

const CLIPPY_ARGS: [&str; 3] = ["clippy", "--all-targets", "--message-format=json"];

/// Error codes for lifetime and borrow-checker problems.
const LIFETIME_CODES: [&str; 22] = [
    "E0106", "E0107", "E0195", "E0261", "E0262", "E0263", "E0309", "E0310", "E0373", "E0491",
    "E0495", "E0499", "E0502", "E0503", "E0505", "E0506", "E0515", "E0521", "E0597", "E0621",
    "E0700", "E0716",
];

/// One machine-applicable suggestion: every replacement has to be made for the
/// fix to be correct. Ranges are byte offsets into the file.
#[derive(Debug, Clone, PartialEq)]
pub struct Fix {
    pub lint: String,
    pub replacements: Vec<(usize, usize, String)>,
}

/// Collect the suggestions for one file that the compiler marks as safe to
/// apply without review (`MachineApplicable`).
pub fn collect_fixes(messages: &[Value], workspace_root: &Path, file_key: &str) -> Vec<Fix> {
    let mut fixes: Vec<Fix> = Vec::new();

    for message in messages {
        let lint = message_code(message).unwrap_or("unknown").to_string();

        // The suggestion is normally a child ("help: remove this") of the
        // diagnostic. A diagnostic can offer alternatives; take the first usable one.
        let candidates = std::iter::once(message).chain(
            message
                .get("children")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        );
        for candidate in candidates {
            let suggestions: Vec<&Value> = candidate
                .get("spans")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|span| {
                    span.get("suggested_replacement")
                        .is_some_and(|r| !r.is_null())
                })
                .collect();
            if suggestions.is_empty() {
                continue;
            }

            let replacements: Option<Vec<(usize, usize, String)>> = suggestions
                .iter()
                .map(|span| {
                    let applicable = span.get("suggestion_applicability").and_then(Value::as_str)
                        == Some("MachineApplicable");
                    if !applicable || !span_in_file(span, workspace_root, file_key) {
                        return None;
                    }
                    Some((
                        span.get("byte_start")?.as_u64()? as usize,
                        span.get("byte_end")?.as_u64()? as usize,
                        span.get("suggested_replacement")?.as_str()?.to_string(),
                    ))
                })
                .collect();

            if let Some(replacements) = replacements {
                let fix = Fix {
                    lint: lint.clone(),
                    replacements,
                };
                if !fixes.contains(&fix) {
                    fixes.push(fix);
                }
                break;
            }
        }
    }

    fixes
}

fn ranges_conflict(a: (usize, usize), b: (usize, usize)) -> bool {
    // Two inserts at the same point conflict too: their order is ambiguous.
    a.0 < b.1 && b.0 < a.1 || a.0 == b.0
}

/// Apply as many fixes as possible to `text`. A fix that touches text another
/// fix already changed is left out; rerunning picks it up against the new text.
///
/// Returns the new text and the fixes that were applied.
pub fn apply_fixes<'a>(text: &str, fixes: &'a [Fix]) -> (String, Vec<&'a Fix>) {
    let mut applied: Vec<&Fix> = Vec::new();
    let mut taken: Vec<(usize, usize, &str)> = Vec::new();

    for fix in fixes {
        let valid = fix.replacements.iter().all(|(start, end, _)| {
            start <= end
                && *end <= text.len()
                && text.is_char_boundary(*start)
                && text.is_char_boundary(*end)
        });
        let conflicts = fix
            .replacements
            .iter()
            .enumerate()
            .any(|(i, (start, end, _))| {
                let range = (*start, *end);
                taken
                    .iter()
                    .any(|(s, e, _)| ranges_conflict(range, (*s, *e)))
                    || fix.replacements[..i]
                        .iter()
                        .any(|(s, e, _)| ranges_conflict(range, (*s, *e)))
            });
        if !valid || conflicts {
            continue;
        }

        taken.extend(
            fix.replacements
                .iter()
                .map(|(start, end, replacement)| (*start, *end, replacement.as_str())),
        );
        applied.push(fix);
    }

    taken.sort_by_key(|(start, _, _)| *start);
    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, replacement) in taken {
        result.push_str(&text[cursor..start]);
        result.push_str(replacement);
        cursor = end;
    }
    result.push_str(&text[cursor..]);

    (result, applied)
}

/// A one-line `code line N: message` description of a compiler message.
fn brief(message: &Value) -> String {
    let line = message
        .get("spans")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|span| span.get("is_primary").and_then(Value::as_bool) == Some(true))
        .and_then(|span| span.get("line_start"))
        .and_then(Value::as_u64)
        .unwrap_or(0);
    format!(
        "{} (line {line}): {}",
        message_code(message).unwrap_or("unknown"),
        message_text(message)
    )
}

pub async fn apply_clippy_suggestions_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;

    let file = normalize_path(Path::new(file_path));
    if !file.is_file() {
        bail!("{} does not exist", file.display());
    }
    let manifest = find_manifest(&file)?;
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let root = workspace_root(dir).await?;
    let file_key = path_key(&file);

    let run = run_cargo_json(&CLIPPY_ARGS, dir).await?;
    let in_file: Vec<Value> = run
        .messages
        .into_iter()
        .filter(|message| message_in_file(message, &root, &file_key))
        .collect();
    if in_file.is_empty() {
        return Ok(ToolResult::text(format!(
            "Clippy reported nothing for {}",
            file.display()
        )));
    }

    let fixes = collect_fixes(&in_file, &root, &file_key);
    let text = read_source(&file)?;
    let (updated, applied) = apply_fixes(&text, &fixes);
    if !applied.is_empty() {
        write_source(&file, &updated)?;
    }

    let mut out = vec![format!(
        "Applied {} clippy suggestion(s) to {}",
        applied.len(),
        file.display()
    )];
    for fix in &applied {
        out.push(format!("  fixed {}", fix.lint));
    }

    // Everything clippy reported that was not fixed: either it has no
    // automatic fix, or its fix overlapped one that was applied.
    let applied_lints: Vec<&str> = applied.iter().map(|fix| fix.lint.as_str()).collect();
    let remaining: Vec<String> = in_file
        .iter()
        .filter(|message| !applied_lints.contains(&message_code(message).unwrap_or("unknown")))
        .map(brief)
        .collect();
    if !remaining.is_empty() {
        out.push(format!(
            "{} diagnostic(s) were not fixed automatically:",
            remaining.len()
        ));
        out.extend(remaining.iter().map(|line| format!("  {line}")));
    }
    if applied.len() < fixes.len() {
        out.push(format!(
            "{} suggestion(s) overlapped an applied fix; run again to apply them",
            fixes.len() - applied.len()
        ));
    }

    Ok(ToolResult::text(out.join("\n")))
}

/// Whether a compiler message is about lifetimes or borrowing.
pub fn is_lifetime_message(message: &Value) -> bool {
    let code = message_code(message).unwrap_or("");
    if LIFETIME_CODES.contains(&code) || code.contains("lifetime") {
        return true;
    }
    let text = message_text(message).to_lowercase();
    ["lifetime", "does not live long enough", "borrowed value"]
        .iter()
        .any(|needle| text.contains(needle))
}

pub async fn validate_lifetimes_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let file_path = str_arg(&args, "file_path")?;

    let file = normalize_path(Path::new(file_path));
    if !file.is_file() {
        bail!("{} does not exist", file.display());
    }
    let manifest = find_manifest(&file)?;
    let dir = manifest.parent().unwrap_or(Path::new("."));
    let root = workspace_root(dir).await?;
    let file_key = path_key(&file);

    // Clippy runs the compiler's own checks and adds lints such as
    // `needless_lifetimes` on top.
    let run = run_cargo_json(&CLIPPY_ARGS, dir).await?;
    let issues: Vec<&Value> = run
        .messages
        .iter()
        .filter(|message| message_in_file(message, &root, &file_key))
        .filter(|message| is_lifetime_message(message))
        .collect();

    if issues.is_empty() {
        return Ok(ToolResult::text(format!(
            "No lifetime or borrow issues found in {}",
            file.display()
        )));
    }

    let mut sections = vec![format!(
        "{} lifetime or borrow issue(s) in {}:",
        issues.len(),
        file.display()
    )];
    sections.extend(
        issues
            .iter()
            .map(|message| message_rendered(message).to_string()),
    );
    Ok(ToolResult::text(sections.join("\n\n")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn suggestion(start: usize, end: usize, replacement: &str, applicability: &str) -> Value {
        json!({
            "file_name": "src/lib.rs",
            "byte_start": start,
            "byte_end": end,
            "is_primary": true,
            "suggested_replacement": replacement,
            "suggestion_applicability": applicability
        })
    }

    fn diagnostic(lint: &str, spans: Vec<Value>) -> Value {
        json!({
            "code": { "code": lint },
            "message": "lint fired",
            "spans": [{ "file_name": "src/lib.rs", "is_primary": true, "line_start": 1 }],
            "children": [
                { "message": "a note with no suggestion", "spans": [] },
                { "message": "help", "spans": spans }
            ]
        })
    }

    fn fixture() -> (std::path::PathBuf, String) {
        let root = std::env::temp_dir().join("ws");
        let key = path_key(&root.join("src").join("lib.rs"));
        (root, key)
    }

    #[test]
    fn machine_applicable_suggestions_are_applied() {
        let text = "fn f() -> i32 {\n    return 1;\n}\n";
        let start = text.find("return 1;").unwrap();
        let messages = [diagnostic(
            "clippy::needless_return",
            vec![suggestion(
                start,
                start + "return 1;".len(),
                "1",
                "MachineApplicable",
            )],
        )];
        let (root, key) = fixture();

        let fixes = collect_fixes(&messages, &root, &key);
        let (updated, applied) = apply_fixes(text, &fixes);

        assert_eq!(updated, "fn f() -> i32 {\n    1\n}\n");
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].lint, "clippy::needless_return");
    }

    #[test]
    fn suggestions_needing_review_or_in_other_files_are_skipped() {
        let (root, key) = fixture();

        let maybe = [diagnostic(
            "clippy::x",
            vec![suggestion(0, 1, "y", "MaybeIncorrect")],
        )];
        assert!(collect_fixes(&maybe, &root, &key).is_empty());

        // A multi-part fix is only usable if every part is.
        let mut elsewhere = suggestion(0, 1, "y", "MachineApplicable");
        elsewhere["file_name"] = json!("src/other.rs");
        let mixed = [diagnostic(
            "clippy::x",
            vec![suggestion(4, 5, "z", "MachineApplicable"), elsewhere],
        )];
        assert!(collect_fixes(&mixed, &root, &key).is_empty());
    }

    #[test]
    fn duplicate_and_overlapping_fixes_are_applied_once() {
        let text = "abcdef";
        let first = diagnostic("a", vec![suggestion(0, 3, "X", "MachineApplicable")]);
        let overlapping = diagnostic("b", vec![suggestion(2, 4, "Y", "MachineApplicable")]);
        let separate = diagnostic("c", vec![suggestion(5, 6, "Z", "MachineApplicable")]);
        let messages = [first.clone(), first, overlapping, separate];
        let (root, key) = fixture();

        let fixes = collect_fixes(&messages, &root, &key);
        assert_eq!(fixes.len(), 3);

        let (updated, applied) = apply_fixes(text, &fixes);
        assert_eq!(updated, "XdeZ");
        assert_eq!(applied.len(), 2);
    }

    #[test]
    fn lifetime_messages_are_recognised() {
        let by_code = json!({ "code": { "code": "E0597" }, "message": "`x` dropped" });
        let by_lint = json!({ "code": { "code": "clippy::needless_lifetimes" }, "message": "" });
        let by_text = json!({ "code": null, "message": "missing lifetime specifier" });
        let unrelated = json!({ "code": { "code": "E0308" }, "message": "mismatched types" });

        assert!(is_lifetime_message(&by_code));
        assert!(is_lifetime_message(&by_lint));
        assert!(is_lifetime_message(&by_text));
        assert!(!is_lifetime_message(&unrelated));
    }
}
