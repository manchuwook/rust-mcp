use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Resolve a (possibly relative) path to an absolute one without touching the
/// filesystem more than necessary.
///
/// Windows 8.3 short names (`C:\Users\MANCHU~1\...`) are expanded, because
/// rust-analyzer and cargo report the long form and paths have to compare equal.
pub fn normalize_path(path: &Path) -> PathBuf {
    let absolute = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());

    if cfg!(windows) && absolute.to_string_lossy().contains('~') {
        // The file itself may not exist yet, so expand the deepest ancestor that does.
        let mut missing = Vec::new();
        let mut existing = absolute.as_path();
        loop {
            if let Ok(canonical) = std::fs::canonicalize(existing) {
                let text = canonical.to_string_lossy();
                // Only accept plain drive paths; UNC results keep the original form.
                if let Some(stripped) = text.strip_prefix(r"\\?\")
                    && stripped.as_bytes().get(1) == Some(&b':')
                {
                    let mut expanded = PathBuf::from(stripped);
                    expanded.extend(missing.iter().rev());
                    return expanded;
                }
                break;
            }
            match (existing.parent(), existing.file_name()) {
                (Some(parent), Some(name)) => {
                    missing.push(name.to_os_string());
                    existing = parent;
                }
                _ => break,
            }
        }
    }

    absolute
}

/// A string form of a path that compares equal for the same file regardless of
/// separator style or (on Windows) letter case.
pub fn path_key(path: &Path) -> String {
    let key = normalize_path(path).to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        key.to_lowercase()
    } else {
        key
    }
}

/// Convert an OS file path into a valid `file://` URI that rust-analyzer accepts.
///
/// On Windows a path like `X:\dev\foo` must become `file:///X:/dev/foo`
/// (backslashes normalized to `/`, a leading `/` added before the drive letter).
/// POSIX paths are already absolute with a leading `/`, yielding `file:///home/...`.
pub fn path_to_file_uri(file_path: &str) -> String {
    let normalized = normalize_path(Path::new(file_path))
        .to_string_lossy()
        .replace('\\', "/");

    let mut encoded = String::with_capacity(normalized.len());
    for byte in normalized.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' | b':' => {
                encoded.push(byte as char)
            }
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }

    if encoded.starts_with('/') {
        format!("file://{encoded}")
    } else {
        // Windows drive path (e.g. "X:/dev/foo") needs the extra slash.
        format!("file:///{encoded}")
    }
}

/// Inverse of [`path_to_file_uri`]: turn a `file://` URI back into an OS path.
pub fn file_uri_to_path(uri: &str) -> PathBuf {
    let rest = uri.strip_prefix("file://").unwrap_or(uri);

    let mut bytes = Vec::with_capacity(rest.len());
    let raw = rest.as_bytes();
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'%'
            && let Some(decoded) = rest
                .get(i + 1..i + 3)
                .and_then(|hex| u8::from_str_radix(hex, 16).ok())
        {
            bytes.push(decoded);
            i += 3;
            continue;
        }
        bytes.push(raw[i]);
        i += 1;
    }
    let decoded = String::from_utf8_lossy(&bytes).into_owned();

    // "/c:/dev/foo" -> "C:/dev/foo" (rust-analyzer lowercases drive letters)
    let decoded = match decoded.as_bytes() {
        [b'/', drive, b':', ..] if drive.is_ascii_alphabetic() => {
            format!("{}{}", drive.to_ascii_uppercase() as char, &decoded[2..])
        }
        _ => decoded,
    };

    if cfg!(windows) {
        PathBuf::from(decoded.replace('/', "\\"))
    } else {
        PathBuf::from(decoded)
    }
}

fn text_document(file_path: &str) -> Value {
    json!({ "uri": path_to_file_uri(file_path) })
}

pub fn create_text_document_position_params(file_path: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": text_document(file_path),
        "position": {
            "line": line,
            "character": character
        }
    })
}

pub fn create_references_params(file_path: &str, line: u32, character: u32) -> Value {
    json!({
        "textDocument": text_document(file_path),
        "position": {
            "line": line,
            "character": character
        },
        "context": {
            "includeDeclaration": true
        }
    })
}

pub fn create_workspace_symbol_params(query: &str) -> Value {
    json!({
        "query": query
    })
}

pub fn create_rename_params(file_path: &str, line: u32, character: u32, new_name: &str) -> Value {
    json!({
        "textDocument": text_document(file_path),
        "position": {
            "line": line,
            "character": character
        },
        "newName": new_name
    })
}

pub fn create_formatting_params(file_path: &str) -> Value {
    json!({
        "textDocument": text_document(file_path),
        "options": {
            "tabSize": 4,
            "insertSpaces": true
        }
    })
}

pub fn create_text_document_params(file_path: &str) -> Value {
    json!({
        "textDocument": text_document(file_path)
    })
}

pub fn create_did_open_params(file_path: &str, version: i64, text: &str) -> Value {
    json!({
        "textDocument": {
            "uri": path_to_file_uri(file_path),
            "languageId": "rust",
            "version": version,
            "text": text
        }
    })
}

pub fn create_did_change_params(file_path: &str, version: i64, text: &str) -> Value {
    json!({
        "textDocument": {
            "uri": path_to_file_uri(file_path),
            "version": version
        },
        // A change without a range replaces the whole document.
        "contentChanges": [{ "text": text }]
    })
}

/// `start` and `end` are `(line, character)` pairs.
pub fn create_code_action_params(file_path: &str, start: (u32, u32), end: (u32, u32)) -> Value {
    json!({
        "textDocument": text_document(file_path),
        "range": {
            "start": { "line": start.0, "character": start.1 },
            "end": { "line": end.0, "character": end.1 }
        },
        "context": {
            "diagnostics": []
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_round_trips_through_path() {
        let dir = std::env::temp_dir().join("rust mcp#dir");
        let path = dir.join("lib.rs");
        let uri = path_to_file_uri(&path.to_string_lossy());

        assert!(uri.starts_with("file:///"));
        assert!(uri.contains("rust%20mcp%23dir"));
        assert_eq!(path_key(&file_uri_to_path(&uri)), path_key(&path));
    }

    #[test]
    fn uri_with_encoded_drive_colon_decodes() {
        let path = file_uri_to_path("file:///c%3A/dev/foo.rs");
        assert_eq!(path.to_string_lossy().replace('\\', "/"), "C:/dev/foo.rs");
    }
}
