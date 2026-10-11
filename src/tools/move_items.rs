// Moving top-level items between files, and repairing what the move breaks:
// paths and imports that referred to the items, the imports the items need
// in their new home, and visibility.

use crate::analyzer::RustAnalyzerClient;
use crate::analyzer::edit::{offset_to_position, range_to_offsets};
use crate::analyzer::protocol::{file_uri_to_path, normalize_path, path_key};
use crate::tools::advanced::{insert_mod_declaration, plan_module};
use crate::tools::cargo::{
    cargo_metadata, count_levels, find_manifest, message_in_file, message_level, message_rendered,
    run_cargo_json, workspace_root,
};
use crate::tools::refactoring::import_block;
use crate::tools::support::{code_chars, code_identifiers, read_source, str_arg, write_source};
use crate::tools::types::ToolResult;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Replace bytes `.0..(.1)` of a text with `.2`.
type Replacement = (usize, usize, String);

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

/// Apply replacements, all given against the original `text`, in one pass.
fn apply_replacements(text: &str, mut edits: Vec<Replacement>) -> Result<String> {
    edits.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    edits.dedup();

    let mut result = String::with_capacity(text.len());
    let mut cursor = 0;
    for (start, end, replacement) in edits {
        if start < cursor || end < start || end > text.len() {
            bail!("the edits needed for this move overlap; nothing was changed");
        }
        result.push_str(&text[cursor..start]);
        result.push_str(&replacement);
        cursor = end;
    }
    result.push_str(&text[cursor..]);
    Ok(result)
}

/// Widen an item's range so that removing it also removes the rest of its
/// last line and the blank line that separated it from what follows.
fn removal_span(text: &str, start: usize, end: usize) -> (usize, usize) {
    let mut end = end;
    if text[end..].starts_with('\n') {
        end += 1;
    }
    while (start == 0 || text[..start].ends_with("\n\n")) && text[end..].starts_with('\n') {
        end += 1;
    }
    (start, end)
}

/// Remove byte ranges (sorted, non-overlapping) from `text`, along with the
/// blank line each one would otherwise leave behind.
pub fn remove_ranges(text: &str, ranges: &[(usize, usize)]) -> String {
    let edits = ranges
        .iter()
        .map(|(start, end)| {
            let (start, end) = removal_span(text, *start, *end);
            (start, end, String::new())
        })
        .collect();
    // Item ranges never overlap, and widening only consumes newlines.
    let mut result = apply_replacements(text, edits).unwrap_or_else(|_| text.to_string());
    while result.ends_with("\n\n") {
        result.pop();
    }
    result
}

/// The span of whole lines covering `start..end`, if nothing else is on them;
/// used to delete a statement without leaving an empty line.
fn line_span(text: &str, start: usize, end: usize) -> (usize, usize) {
    let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
    let start = if text[line_start..start].trim().is_empty() {
        line_start
    } else {
        start
    };
    let rest = &text[end..];
    let trailing = rest.len() - rest.trim_start_matches([' ', '\t']).len();
    let end = if rest[trailing..].starts_with('\n') {
        end + trailing + 1
    } else {
        end
    };
    (start, end)
}

/// The module path (`["crate", "tools", "advanced"]`) of a source file, given
/// the root file of its crate target (`src/lib.rs`, `src/main.rs`, ...).
///
/// This follows the standard file layout only: modules relocated with
/// `#[path]` are not understood.
pub fn module_path(root_file: &Path, file: &Path) -> Option<Vec<String>> {
    let root = normalize_path(root_file);
    let file = normalize_path(file);
    let mut path = vec!["crate".to_string()];
    if path_key(&root) == path_key(&file) {
        return Some(path);
    }

    let root_dir = path_key(root.parent()?);
    let file_key = path_key(&file);
    let relative_key = file_key.strip_prefix(&format!("{root_dir}/"))?;
    // Take the same tail from the un-lowercased path to keep the names' case.
    let full = file.to_string_lossy().replace('\\', "/");
    let relative = full.get(full.len().checked_sub(relative_key.len())?..)?;

    let mut segments: Vec<&str> = relative.strip_suffix(".rs")?.split('/').collect();
    if segments.last() == Some(&"mod") {
        segments.pop();
    }
    if segments.is_empty() || segments.iter().any(|segment| segment.is_empty()) {
        return None;
    }
    path.extend(segments.into_iter().map(str::to_string));
    Some(path)
}

/// The root files of a package's targets, library first.
async fn target_roots(manifest: &Path) -> Result<Vec<PathBuf>> {
    let metadata = cargo_metadata(&manifest.to_string_lossy()).await?;
    let manifest_key = path_key(manifest);

    let mut roots: Vec<(u8, PathBuf)> = Vec::new();
    for package in metadata
        .get("packages")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let package_manifest = package.get("manifest_path").and_then(Value::as_str);
        if package_manifest.map(|path| path_key(Path::new(path))) != Some(manifest_key.clone()) {
            continue;
        }
        for target in package
            .get("targets")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let Some(src_path) = target.get("src_path").and_then(Value::as_str) else {
                continue;
            };
            let kinds: Vec<&str> = target
                .get("kind")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .collect();
            let rank = if kinds
                .iter()
                .any(|kind| kind.contains("lib") || *kind == "proc-macro")
            {
                0
            } else if kinds.contains(&"bin") {
                1
            } else {
                2
            };
            roots.push((rank, PathBuf::from(src_path)));
        }
    }
    roots.sort();
    Ok(roots.into_iter().map(|(_, root)| root).collect())
}

/// A `use` declaration in a source text.
#[derive(Debug, Clone, PartialEq)]
pub struct UseStatement {
    /// Start of the statement, including any visibility.
    pub start: usize,
    /// Offset of the `use` keyword.
    pub keyword: usize,
    /// Offset just past the terminating `;`.
    pub end: usize,
    /// Whether it sits at the start of a line, i.e. at the file's top level.
    pub top_level: bool,
}

/// One name brought into scope by a `use` declaration.
#[derive(Debug, Clone, PartialEq)]
pub struct UseBinding {
    /// The full path of what is imported (the module, for a glob).
    pub path: Vec<String>,
    /// The name it is bound to: the alias if there is one.
    pub name: String,
    pub glob: bool,
    /// Offset of the path's last segment, which identifies the entry.
    pub offset: usize,
}

/// If `before` (the text preceding a `use`) ends with a visibility, its offset.
fn visibility_start(before: &str) -> Option<usize> {
    let before = before.trim_end();
    let head = if before.ends_with(')') {
        before[..before.rfind('(')?].trim_end()
    } else {
        before
    };
    let start = head.len().checked_sub(3)?;
    let is_keyword = head.ends_with("pub") && !head[..start].ends_with(is_ident_char);
    is_keyword.then_some(start)
}

/// Every `use` declaration in `text`, in order.
pub fn use_statements(text: &str) -> Vec<UseStatement> {
    let mut statements: Vec<UseStatement> = Vec::new();
    for (offset, word) in code_identifiers(text) {
        if word != "use" || statements.last().is_some_and(|last| offset < last.end) {
            continue;
        }
        // `impl Trait + use<'a>` is a capture list, not an import.
        if text[offset + 3..].trim_start().starts_with('<') {
            continue;
        }
        let Some(end) = code_chars(text, offset)
            .find(|(_, c)| *c == ';')
            .map(|(i, _)| i + 1)
        else {
            continue;
        };
        let start = visibility_start(&text[..offset]).unwrap_or(offset);
        let line_start = text[..start].rfind('\n').map_or(0, |i| i + 1);
        statements.push(UseStatement {
            start,
            keyword: offset,
            end,
            top_level: line_start == start,
        });
    }
    statements
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Token<'a> {
    Ident(usize, &'a str),
    PathSeparator,
    Open,
    Close,
    Comma,
    Star,
}

fn use_tokens<'a>(text: &'a str, statement: &UseStatement) -> Vec<Token<'a>> {
    let body_start = statement.keyword + 3;
    let body_end = statement.end.saturating_sub(1);
    let mut tokens = Vec::new();
    let mut previous_colon = false;

    for (offset, word) in code_identifiers(&text[body_start..body_end]) {
        tokens.push((body_start + offset, Token::Ident(body_start + offset, word)));
    }
    for (offset, c) in code_chars(text, body_start).take_while(|(offset, _)| *offset < body_end) {
        let token = match c {
            ':' if previous_colon => Some(Token::PathSeparator),
            '{' => Some(Token::Open),
            '}' => Some(Token::Close),
            ',' => Some(Token::Comma),
            '*' => Some(Token::Star),
            _ => None,
        };
        previous_colon = c == ':' && !previous_colon;
        if let Some(token) = token {
            tokens.push((offset, token));
        }
    }

    tokens.sort_by_key(|(offset, _)| *offset);
    tokens.into_iter().map(|(_, token)| token).collect()
}

fn parse_use_tree(
    tokens: &[Token],
    index: &mut usize,
    prefix: &mut Vec<String>,
    bindings: &mut Vec<UseBinding>,
) {
    let base = prefix.len();
    let mut last_offset = None;

    loop {
        match tokens.get(*index) {
            Some(Token::Ident(offset, word)) if *word != "as" => {
                prefix.push(word.to_string());
                last_offset = Some(*offset);
                *index += 1;
                if tokens.get(*index) == Some(&Token::PathSeparator) {
                    *index += 1;
                    continue;
                }
                break;
            }
            Some(Token::Open) => {
                *index += 1;
                while let Some(token) = tokens.get(*index) {
                    if *token == Token::Close {
                        *index += 1;
                        break;
                    }
                    let before = *index;
                    parse_use_tree(tokens, index, prefix, bindings);
                    if tokens.get(*index) == Some(&Token::Comma) {
                        *index += 1;
                    }
                    if *index == before {
                        // Not a token a use tree can start with; skip it.
                        *index += 1;
                    }
                }
                prefix.truncate(base);
                return;
            }
            Some(Token::Star) => {
                bindings.push(UseBinding {
                    path: prefix.clone(),
                    name: "*".to_string(),
                    glob: true,
                    offset: 0,
                });
                *index += 1;
                prefix.truncate(base);
                return;
            }
            _ => break,
        }
    }

    if let Some(offset) = last_offset {
        let mut path = prefix.clone();
        // `a::b::{self}` imports `a::b`.
        if path.last().is_some_and(|last| last == "self") && path.len() > 1 {
            path.pop();
        }
        let mut name = path.last().cloned().unwrap_or_default();
        if let (Some(Token::Ident(_, "as")), Some(Token::Ident(_, alias))) =
            (tokens.get(*index), tokens.get(*index + 1))
        {
            name = alias.to_string();
            *index += 2;
        }
        bindings.push(UseBinding {
            path,
            name,
            glob: false,
            offset,
        });
    }
    prefix.truncate(base);
}

/// The names a `use` declaration brings into scope.
pub fn use_bindings(text: &str, statement: &UseStatement) -> Vec<UseBinding> {
    let tokens = use_tokens(text, statement);
    let mut bindings = Vec::new();
    let mut index = 0;
    parse_use_tree(&tokens, &mut index, &mut Vec::new(), &mut bindings);
    bindings
}

/// Remove from a use statement (given as its own string, starting at `use`)
/// the entry whose last path segment is at `position`.
fn remove_use_entry(statement: &mut String, position: usize) {
    let after_keyword = "use".len();
    let entry_start = statement[..position]
        .rfind(['{', ','])
        .map_or(after_keyword, |i| i + 1);
    let entry_start = entry_start
        + (statement[entry_start..].len() - statement[entry_start..].trim_start().len());
    let entry_len = statement[position..]
        .find([',', '}', ';'])
        .unwrap_or(statement.len() - position);
    let entry_end = position + statement[position..position + entry_len].trim_end().len();

    let following = &statement[entry_end..];
    if following.trim_start().starts_with(',') {
        // Take the comma that follows, and the space after it.
        let comma = entry_end + following.find(',').unwrap_or(0);
        let rest = &statement[comma + 1..];
        let spaces = rest.len() - rest.trim_start_matches([' ', '\t']).len();
        statement.replace_range(entry_start..comma + 1 + spaces, "");
    } else if let Some(comma) = statement[..entry_start]
        .trim_end()
        .strip_suffix(',')
        .map(str::len)
    {
        // Last entry of its group: take the comma before it instead.
        statement.replace_range(comma..entry_end, "");
    } else {
        statement.replace_range(entry_start..entry_end, "");
    }
}

/// Rewrite a use statement with some entries removed and, optionally, new
/// statements placed after it. `removals` are offsets (into `text`) of the
/// entries' last path segments. The statement is dropped if nothing is left.
fn rewrite_use(
    text: &str,
    statement: &UseStatement,
    removals: &[usize],
    additions: &[String],
) -> Replacement {
    let mut rewritten = text[statement.keyword..statement.end].to_string();
    let mut positions: Vec<usize> = removals
        .iter()
        .map(|position| position - statement.keyword)
        .collect();
    positions.sort_unstable();
    positions.dedup();
    // Back to front, so earlier offsets stay valid.
    for position in positions.into_iter().rev() {
        remove_use_entry(&mut rewritten, position);
    }
    // A multi-line group can be left with an empty line where an entry was.
    let rewritten: Vec<&str> = rewritten
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    let rewritten = rewritten.join("\n");

    let whole = UseStatement {
        start: 0,
        keyword: 0,
        end: rewritten.len(),
        top_level: true,
    };
    let mut lines = Vec::new();
    if !use_bindings(&rewritten, &whole).is_empty() {
        lines.push(format!(
            "{}{rewritten}",
            &text[statement.start..statement.keyword]
        ));
    }
    lines.extend(additions.iter().cloned());

    if lines.is_empty() {
        let (start, end) = line_span(text, statement.start, statement.end);
        return (start, end, String::new());
    }
    let line_start = text[..statement.start].rfind('\n').map_or(0, |i| i + 1);
    let indent = &text[line_start..statement.start];
    let indent = if indent.trim().is_empty() { indent } else { "" };
    (
        statement.start,
        statement.end,
        lines.join(&format!("\n{indent}")),
    )
}

/// What precedes a name in a path expression.
#[derive(Debug, PartialEq)]
enum Qualifier {
    /// Nothing: the name is used unqualified.
    None,
    /// A module path starting at this offset, e.g. `crate::shapes::`.
    Path(usize),
    /// Something that is not a plain module path, e.g. `Vec::<T>::`.
    Unsupported,
}

fn qualifier_of(text: &str, name_start: usize) -> Qualifier {
    let mut start = name_start;
    let mut found = Qualifier::None;
    loop {
        let Some(before) = text[..start].trim_end().strip_suffix("::") else {
            return found;
        };
        let before = before.trim_end();
        let segment: usize = before
            .chars()
            .rev()
            .take_while(|c| is_ident_char(*c))
            .map(char::len_utf8)
            .sum();
        if segment == 0 {
            return match found {
                Qualifier::None => Qualifier::Unsupported,
                found => found,
            };
        }
        start = before.len() - segment;
        found = Qualifier::Path(start);
    }
}

/// How one reference to a moved item has to change.
#[derive(Debug, PartialEq)]
enum Site {
    /// Replace part of the path in place.
    Edit(Replacement),
    /// The name is an entry of the use statement with this index: remove it
    /// there and import it from its new home (under `alias`, if it had one).
    UseEntry {
        statement: usize,
        alias: Option<String>,
    },
    /// The name is used unqualified, relying on being in scope.
    Bare,
    /// Not something that can be rewritten safely.
    Unhandled,
}

/// Classify the reference at `start..end` of `text`. `target_path` is the
/// moved item's new module path; `in_target` says the reference is in the
/// file the item is moving to, where it becomes local.
fn classify_site(
    text: &str,
    uses: &[UseStatement],
    start: usize,
    end: usize,
    target_path: &str,
    in_target: bool,
) -> Site {
    let Some(index) = uses
        .iter()
        .position(|statement| statement.keyword < start && end <= statement.end)
    else {
        return match qualifier_of(text, start) {
            Qualifier::None => Site::Bare,
            Qualifier::Unsupported => Site::Unhandled,
            Qualifier::Path(qualifier) => {
                let replacement = if in_target {
                    String::new()
                } else {
                    format!("{target_path}::")
                };
                Site::Edit((qualifier, start, replacement))
            }
        };
    };

    let statement = &uses[index];
    let before = &text[statement.keyword..start];
    let depth = before.matches('{').count() as i64 - before.matches('}').count() as i64;
    let after = text[end..statement.end].trim_start();
    let alias = after
        .strip_prefix("as")
        .filter(|rest| rest.starts_with(char::is_whitespace))
        .map(|rest| {
            rest.trim_start()
                .chars()
                .take_while(|c| is_ident_char(*c))
                .collect::<String>()
        });
    let is_last_segment = alias.is_some() || after.starts_with([',', '}', ';']);

    if depth > 0 {
        return if is_last_segment {
            Site::UseEntry {
                statement: index,
                alias,
            }
        } else {
            Site::Unhandled
        };
    }
    if in_target && is_last_segment {
        // `use old::name;` in the item's new home: the import goes away.
        return Site::UseEntry {
            statement: index,
            alias,
        };
    }
    // `use old::path::name...`: swap the path in front of the name.
    let body = &text[statement.keyword + 3..start];
    let path_start = statement.keyword + 3 + (body.len() - body.trim_start().len());
    let replacement = if in_target {
        "self::".to_string()
    } else {
        format!("{target_path}::")
    };
    Site::Edit((path_start, start, replacement))
}

/// Where `pub(crate) ` goes to make the item in `start..end` visible to the
/// rest of the crate. `None` if it already has a visibility or cannot take one.
fn visibility_insertion(text: &str, start: usize, end: usize) -> Option<usize> {
    let mut attribute_depth = 0usize;
    let mut in_attribute = false;
    for (offset, c) in code_chars(text, start).take_while(|(offset, _)| *offset < end) {
        if in_attribute {
            match c {
                '[' => attribute_depth += 1,
                ']' => {
                    attribute_depth = attribute_depth.saturating_sub(1);
                    in_attribute = attribute_depth > 0;
                }
                _ => {}
            }
            continue;
        }
        if c.is_whitespace() {
            continue;
        }
        if c == '#' {
            in_attribute = true;
            continue;
        }
        let word: String = text[offset..end]
            .chars()
            .take_while(|c| is_ident_char(*c))
            .collect();
        return matches!(
            word.as_str(),
            "fn" | "struct"
                | "enum"
                | "trait"
                | "const"
                | "static"
                | "type"
                | "mod"
                | "union"
                | "async"
                | "unsafe"
                | "extern"
        )
        .then_some(offset);
    }
    None
}

/// The edit that adds import statements to a file: after its existing
/// top-level imports, or else at the top below any inner docs/attributes.
fn imports_edit(text: &str, imports: &[String]) -> Replacement {
    let lines: String = imports.iter().map(|import| format!("{import}\n")).collect();

    if let Some((_, end)) = import_block(text) {
        let rest = &text[end..];
        return match rest.find('\n') {
            Some(newline) if rest[..newline].trim().is_empty() => {
                (end + newline + 1, end + newline + 1, lines)
            }
            _ => (end, end, format!("\n{}", lines.trim_end())),
        };
    }

    let header: usize = text
        .split_inclusive('\n')
        .take_while(|line| line.starts_with("//!") || line.starts_with("#!["))
        .map(str::len)
        .sum();
    let rest = &text[header..];
    let before = if header > 0 { "\n" } else { "" };
    let after = if rest.trim().is_empty() || rest.starts_with('\n') {
        ""
    } else {
        "\n"
    };
    (header, header, format!("{before}{lines}{after}"))
}

/// Make a path from a `use` in the module `module` absolute, so that it means
/// the same thing from any other module. `local_names` are the module's own
/// items, which a `use` may name without a `self::` prefix.
fn absolute_path(path: &[String], module: &[String], local_names: &[&str]) -> Option<Vec<String>> {
    let first = path.first()?.as_str();
    match first {
        "crate" => Some(path.to_vec()),
        "self" => Some([module, &path[1..]].concat()),
        "super" => {
            let mut base = module.to_vec();
            let mut rest = path;
            while rest.first().is_some_and(|segment| segment == "super") {
                // `crate` itself has no parent.
                if base.len() <= 1 {
                    return None;
                }
                base.pop();
                rest = &rest[1..];
            }
            Some([base.as_slice(), rest].concat())
        }
        _ if local_names.contains(&first) => Some([module, path].concat()),
        // An external crate (or std).
        _ => Some(path.to_vec()),
    }
}

fn import_statement(path: &[String], name: &str) -> String {
    let joined = path.join("::");
    if path.last().is_some_and(|last| last == name) {
        format!("use {joined};")
    } else {
        format!("use {joined} as {name};")
    }
}

/// A top-level item of the source file.
struct Item {
    name: String,
    range: (usize, usize),
    /// Where its name is; `None` for items without one, such as `impl` blocks.
    name_offset: Option<usize>,
    moved: bool,
}

/// A file taking part in the move, with the changes planned for it.
struct FileState {
    path: PathBuf,
    /// Contents with `\n` line endings; offsets refer to this.
    text: String,
    crlf: bool,
    existed: bool,
    edits: Vec<Replacement>,
    imports: Vec<String>,
}

impl FileState {
    fn load(path: &Path) -> Result<Self> {
        let existed = path.is_file();
        let original = if existed {
            read_source(path)?
        } else {
            String::new()
        };
        Ok(Self {
            path: path.to_path_buf(),
            text: original.replace("\r\n", "\n"),
            crlf: original.contains("\r\n"),
            existed,
            edits: Vec::new(),
            imports: Vec::new(),
        })
    }

    fn add_import(&mut self, import: String) {
        if !self.imports.contains(&import) && !self.text.contains(&import) {
            self.imports.push(import);
        }
    }

    /// The file's new contents (with `\n` line endings).
    fn updated(&self) -> Result<String> {
        let mut edits = self.edits.clone();
        if !self.imports.is_empty() {
            edits.push(imports_edit(&self.text, &self.imports));
        }
        apply_replacements(&self.text, edits)
    }

    fn write(&self, contents: &str) -> Result<()> {
        if self.crlf {
            write_source(&self.path, &contents.replace('\n', "\r\n"))
        } else {
            write_source(&self.path, contents)
        }
    }
}

const SOURCE: usize = 0;
const TARGET: usize = 1;

/// The files touched by the move; the source and target come first.
struct Files(Vec<FileState>);

impl Files {
    fn index_of(&mut self, path: &Path) -> Result<usize> {
        let key = path_key(path);
        if let Some(index) = self.0.iter().position(|file| path_key(&file.path) == key) {
            return Ok(index);
        }
        self.0.push(FileState::load(&normalize_path(path))?);
        Ok(self.0.len() - 1)
    }
}

/// Ask rust-analyzer for every reference to the symbol at `offset` of the
/// source file, as `(file index, start, end)` byte ranges.
async fn reference_sites(
    analyzer: &mut RustAnalyzerClient,
    source_file: &str,
    offset: usize,
    files: &mut Files,
) -> Result<Vec<(usize, usize, usize)>> {
    let (line, character) = offset_to_position(&files.0[SOURCE].text, offset);
    let references = analyzer
        .find_references(source_file, line, character)
        .await?;

    let mut sites = Vec::new();
    for location in references.as_array().into_iter().flatten() {
        let (Some(uri), Some(range)) = (
            location.get("uri").and_then(Value::as_str),
            location.get("range"),
        ) else {
            continue;
        };
        let file = files.index_of(&file_uri_to_path(uri))?;
        let (start, end) = range_to_offsets(&files.0[file].text, range)?;
        if !sites.contains(&(file, start, end)) {
            sites.push((file, start, end));
        }
    }
    Ok(sites)
}

fn within(ranges: &[(usize, usize)], offset: usize) -> bool {
    ranges
        .iter()
        .any(|(start, end)| *start <= offset && offset < *end)
}

/// The top-level items of the source file, from rust-analyzer's symbols.
fn source_items(symbols: &Value, text: &str, wanted: &[&str]) -> Result<Vec<Item>> {
    let mut items = Vec::new();
    for symbol in symbols.as_array().into_iter().flatten() {
        let name = symbol.get("name").and_then(Value::as_str).unwrap_or("");
        let range = symbol
            .get("range")
            .ok_or_else(|| anyhow!("rust-analyzer returned a symbol without a range"))?;
        let range = range_to_offsets(text, range)?;
        let name_offset = symbol
            .get("selectionRange")
            .and_then(|selection| range_to_offsets(text, selection).ok())
            .map(|(start, _)| start)
            // An `impl` block's "name" is its header, not an identifier.
            .filter(|start| text[*start..].starts_with(name) && !name.is_empty());
        items.push(Item {
            name: name.to_string(),
            range,
            name_offset,
            moved: wanted.contains(&name),
        });
    }
    Ok(items)
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

    // Everything up to the "write" step below only reads, so any failure
    // leaves the files untouched.

    // Where the two files sit in the module tree.
    let manifest = find_manifest(&source)?;
    let roots = target_roots(&manifest).await?;
    let (root, source_module) = roots
        .iter()
        .find_map(|root| module_path(root, &source).map(|module| (root, module)))
        .ok_or_else(|| anyhow!("{} is not part of a crate target", source.display()))?;
    let target_module = module_path(root, &target).ok_or_else(|| {
        anyhow!(
            "{} is not in the same crate target as {}",
            target.display(),
            source.display()
        )
    })?;
    let target_path = target_module.join("::");
    let source_path = source_module.join("::");
    // A module can see the private items of its ancestors.
    let target_inside_source = target_module.starts_with(&source_module);

    let mut files = Files(vec![FileState::load(&source)?, FileState::load(&target)?]);

    // Resolve every name before going any further.
    let symbols = analyzer.document_symbols(source_file).await?;
    let items = source_items(&symbols, &files.0[SOURCE].text, &item_names)?;
    let missing: Vec<&str> = item_names
        .iter()
        .filter(|name| !items.iter().any(|item| item.name == **name))
        .copied()
        .collect();
    if !missing.is_empty() {
        let available: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();
        bail!(
            "No top-level item named {} in {}. Top-level items: {}",
            missing.join(", "),
            source.display(),
            available.join(", ")
        );
    }
    let mut moved_ranges: Vec<(usize, usize)> = items
        .iter()
        .filter(|item| item.moved)
        .map(|item| item.range)
        .collect();
    moved_ranges.sort();
    moved_ranges.dedup();

    // A new target file has to be declared as a module to be part of the crate.
    let new_module = if files.0[TARGET].existed {
        None
    } else {
        let name = target_module.last().cloned().unwrap_or_default();
        let directory = target.parent().unwrap_or(Path::new("."));
        let is_plain_file = target.file_stem().is_some_and(|stem| stem != "mod");
        let parent = plan_module(&name, &directory.to_string_lossy())
            .ok()
            .filter(|(file, _)| is_plain_file && path_key(file) == path_key(&target))
            .and_then(|(_, parent)| parent);
        let Some(parent) = parent else {
            bail!(
                "{} does not exist and no parent module file was found to declare it in; \
                 create it with create_module first",
                target.display()
            );
        };
        Some((name, parent))
    };

    // What the target already has in scope.
    let mut target_names: Vec<String> = items
        .iter()
        .filter(|item| item.moved)
        .map(|item| item.name.clone())
        .collect();
    if files.0[TARGET].existed {
        let target_symbols = analyzer.document_symbols(target_file).await?;
        for symbol in target_symbols.as_array().into_iter().flatten() {
            if let Some(name) = symbol.get("name").and_then(Value::as_str) {
                target_names.push(name.to_string());
            }
        }
        let text = &files.0[TARGET].text;
        for statement in use_statements(text).iter().filter(|s| s.top_level) {
            target_names.extend(use_bindings(text, statement).into_iter().map(|b| b.name));
        }
    }

    // References to the moved items, from rust-analyzer.
    let mut external_sites: Vec<(usize, usize, usize, usize)> = Vec::new();
    let mut internal_sites: Vec<(usize, usize)> = Vec::new();
    for (index, item) in items.iter().enumerate().filter(|(_, item)| item.moved) {
        let Some(name_offset) = item.name_offset else {
            continue;
        };
        for (file, start, end) in
            reference_sites(analyzer, source_file, name_offset, &mut files).await?
        {
            if file == SOURCE && within(&moved_ranges, start) {
                if start != name_offset {
                    internal_sites.push((start, end));
                }
            } else {
                external_sites.push((index, file, start, end));
            }
        }
    }

    // What the moved code itself refers to: imports and other items of the
    // source file. Names narrow down the candidates; rust-analyzer confirms.
    let source_text = files.0[SOURCE].text.clone();
    let source_uses = use_statements(&source_text);
    let moved_words: Vec<&str> = code_identifiers(&source_text)
        .into_iter()
        .filter(|(offset, _)| within(&moved_ranges, *offset))
        .map(|(_, word)| word)
        .collect();
    let local_names: Vec<&str> = items.iter().map(|item| item.name.as_str()).collect();

    let mut target_imports: Vec<String> = Vec::new();
    let mut unused_source_imports: BTreeMap<usize, Vec<usize>> = BTreeMap::new();
    let mut glob_imports: Vec<String> = Vec::new();
    for (statement_index, statement) in source_uses.iter().enumerate() {
        if !statement.top_level {
            continue;
        }
        let reexport = statement.start != statement.keyword;
        for binding in use_bindings(&source_text, statement) {
            if binding.glob {
                glob_imports.push(format!("{}::*", binding.path.join("::")));
                continue;
            }
            if !moved_words.contains(&binding.name.as_str()) || target_names.contains(&binding.name)
            {
                continue;
            }
            // If rust-analyzer cannot say, assume the import is needed in both places.
            let (needed, still_used) =
                match reference_sites(analyzer, source_file, binding.offset, &mut files).await {
                    Ok(sites) => {
                        let in_source = |site: &&(usize, usize, usize)| site.0 == SOURCE;
                        let needed = sites
                            .iter()
                            .filter(in_source)
                            .any(|site| within(&moved_ranges, site.1));
                        let still_used = sites.iter().filter(in_source).any(|site| {
                            !within(&moved_ranges, site.1)
                                && !source_uses
                                    .iter()
                                    .any(|u| u.keyword <= site.1 && site.1 < u.end)
                        });
                        (needed, still_used)
                    }
                    Err(_) => (true, true),
                };
            if !needed {
                continue;
            }
            let Some(path) = absolute_path(&binding.path, &source_module, &local_names) else {
                continue;
            };
            // Something that lives in the target module needs no import there.
            if path.len() != target_module.len() + 1 || !path.starts_with(&target_module) {
                target_imports.push(import_statement(&path, &binding.name));
            }
            if !still_used && !reexport {
                unused_source_imports
                    .entry(statement_index)
                    .or_default()
                    .push(binding.offset);
            }
        }
    }

    let mut widened: Vec<String> = Vec::new();
    for item in items.iter().filter(|item| !item.moved) {
        let Some(name_offset) = item.name_offset else {
            continue;
        };
        if !moved_words.contains(&item.name.as_str()) || target_names.contains(&item.name) {
            continue;
        }
        let used_by_moved = reference_sites(analyzer, source_file, name_offset, &mut files)
            .await
            .map(|sites| {
                sites
                    .iter()
                    .any(|site| site.0 == SOURCE && within(&moved_ranges, site.1))
            })
            .unwrap_or(true);
        if !used_by_moved {
            continue;
        }
        target_imports.push(format!("use {source_path}::{};", item.name));
        if !target_inside_source
            && let Some(offset) = visibility_insertion(&source_text, item.range.0, item.range.1)
        {
            files.0[SOURCE]
                .edits
                .push((offset, offset, "pub(crate) ".to_string()));
            widened.push(item.name.clone());
        }
    }

    // Plan the edit for each reference to a moved item.
    let uses: Vec<Vec<UseStatement>> = files
        .0
        .iter()
        .map(|file| use_statements(&file.text))
        .collect();
    let mut use_entries: BTreeMap<(usize, usize), (Vec<usize>, Vec<String>)> = BTreeMap::new();
    let mut bare: Vec<(usize, usize)> = Vec::new();
    let mut imported_at_top: Vec<(usize, usize)> = Vec::new();
    let mut unhandled: Vec<String> = Vec::new();
    let mut rewritten = 0usize;
    let mut needs_visibility: Vec<usize> = Vec::new();

    for (item_index, file, start, end) in &external_sites {
        let (item_index, file, start, end) = (*item_index, *file, *start, *end);
        let name = &items[item_index].name;
        let text = &files.0[file].text;
        let in_target = file == TARGET;

        // Code outside the target module (and its children) needs the item
        // to be visible to it.
        let site_module = module_path(root, &files.0[file].path);
        if !site_module.is_some_and(|module| module.starts_with(&target_module)) {
            needs_visibility.push(item_index);
        }

        let describe = |text: &str| {
            let (line, character) = offset_to_position(text, start);
            format!("{}:{line}:{character}", files.0[file].path.display())
        };
        if text.get(start..end) != Some(name.as_str()) {
            // A macro expansion or similar: the name is not literally there.
            unhandled.push(describe(text));
            continue;
        }

        match classify_site(text, &uses[file], start, end, &target_path, in_target) {
            Site::Edit(edit) => {
                // A top-level `use` of the item keeps unqualified uses working.
                if uses[file]
                    .iter()
                    .any(|u| u.top_level && u.keyword < start && end <= u.end)
                {
                    imported_at_top.push((file, item_index));
                }
                files.0[file].edits.push(edit);
                rewritten += 1;
            }
            Site::UseEntry { statement, alias } => {
                let entry = use_entries.entry((file, statement)).or_default();
                entry.0.push(start);
                let visibility = &text[uses[file][statement].start..uses[file][statement].keyword];
                match (in_target, alias) {
                    (true, None) => {}
                    (true, Some(alias)) => entry
                        .1
                        .push(format!("{visibility}use self::{name} as {alias};")),
                    (false, None) => entry
                        .1
                        .push(format!("{visibility}use {target_path}::{name};")),
                    (false, Some(alias)) => entry
                        .1
                        .push(format!("{visibility}use {target_path}::{name} as {alias};")),
                }
                if uses[file][statement].top_level && !in_target {
                    imported_at_top.push((file, item_index));
                }
                rewritten += 1;
            }
            Site::Bare => {
                if !in_target && !bare.contains(&(file, item_index)) {
                    bare.push((file, item_index));
                }
            }
            Site::Unhandled => unhandled.push(describe(text)),
        }
    }
    for ((file, statement), (removals, additions)) in &use_entries {
        let edit = rewrite_use(
            &files.0[*file].text,
            &uses[*file][*statement],
            removals,
            additions,
        );
        files.0[*file].edits.push(edit);
    }
    for (file, item_index) in bare {
        if !imported_at_top.contains(&(file, item_index)) {
            let import = format!("use {target_path}::{};", items[item_index].name);
            files.0[file].add_import(import);
        }
    }

    // Imports of the source file that only the moved items used.
    for (statement, removals) in &unused_source_imports {
        let edit = rewrite_use(&source_text, &source_uses[*statement], removals, &[]);
        files.0[SOURCE].edits.push(edit);
    }

    // The moved text itself: drop paths that pointed back at the old module,
    // and widen visibility where the rest of the crate still uses the item.
    let mut moved_edits: Vec<Replacement> = Vec::new();
    for (start, end) in internal_sites {
        if source_uses
            .iter()
            .any(|u| u.keyword < start && end <= u.end)
        {
            continue;
        }
        if let Qualifier::Path(qualifier) = qualifier_of(&source_text, start) {
            moved_edits.push((qualifier, start, String::new()));
        }
    }
    for (index, item) in items.iter().enumerate().filter(|(_, item)| item.moved) {
        if needs_visibility.contains(&index)
            && let Some(offset) = visibility_insertion(&source_text, item.range.0, item.range.1)
        {
            moved_edits.push((offset, offset, "pub(crate) ".to_string()));
            widened.push(item.name.clone());
        }
    }
    let mut moved_pieces = Vec::new();
    for (start, end) in &moved_ranges {
        let edits = moved_edits
            .iter()
            .filter(|(edit_start, _, _)| start <= edit_start && edit_start < end)
            .map(|(edit_start, edit_end, text)| {
                (edit_start - start, edit_end - start, text.clone())
            })
            .collect();
        moved_pieces.push(apply_replacements(&source_text[*start..*end], edits)?);
    }
    let any_public = moved_pieces.iter().any(|piece| {
        visibility_insertion(piece, 0, piece.len()).is_none() && !piece.contains("pub(")
    });

    for (start, end) in &moved_ranges {
        let (start, end) = removal_span(&source_text, *start, *end);
        files.0[SOURCE].edits.push((start, end, String::new()));
    }
    for import in target_imports {
        files.0[TARGET].add_import(import);
    }

    // Work out every file's new contents before writing any of them.
    let mut contents: Vec<String> = Vec::new();
    for (index, file) in files.0.iter().enumerate() {
        let mut updated = file.updated()?;
        if index == SOURCE {
            while updated.ends_with("\n\n") {
                updated.pop();
            }
        }
        if index == TARGET {
            let mut combined = updated.trim_end().to_string();
            if !combined.is_empty() {
                combined.push_str("\n\n");
            }
            combined.push_str(&moved_pieces.join("\n\n"));
            combined.push('\n');
            updated = combined;
        }
        contents.push(updated);
    }
    let declaration = match &new_module {
        Some((name, parent)) => {
            let visibility = if any_public { "pub" } else { "pub(crate)" };
            let declaration = format!("{visibility} mod {name};");
            // The parent may itself be one of the files being edited.
            let parent_index = files.index_of(parent)?;
            if parent_index == contents.len() {
                contents.push(files.0[parent_index].text.clone());
            }
            contents[parent_index] = insert_mod_declaration(&contents[parent_index], &declaration);
            Some((declaration, parent.clone()))
        }
        None => None,
    };

    // Write.
    let mut touched: Vec<&Path> = Vec::new();
    for (file, updated) in files.0.iter().zip(&contents) {
        if *updated != file.text || !file.existed {
            file.write(updated)?;
            touched.push(&file.path);
        }
    }

    let mut out = vec![format!(
        "Moved {} item(s) from {} to {}: {}",
        moved_ranges.len(),
        source.display(),
        target.display(),
        item_names.join(", ")
    )];
    if let Some((declaration, parent)) = &declaration {
        out.push(format!(
            "  created the file and declared `{declaration}` in {}",
            parent.display()
        ));
    }
    if rewritten > 0 {
        out.push(format!(
            "  rewrote {rewritten} reference(s) to point at {target_path}"
        ));
    }
    for file in &files.0 {
        if !file.imports.is_empty() {
            out.push(format!(
                "  added to {}: {}",
                file.path.display(),
                file.imports.join(" ")
            ));
        }
    }
    if !unused_source_imports.is_empty() {
        out.push(format!(
            "  removed imports from {} that only the moved code used",
            source.display()
        ));
    }
    if !widened.is_empty() {
        widened.sort();
        widened.dedup();
        out.push(format!("  made pub(crate): {}", widened.join(", ")));
    }
    if !unhandled.is_empty() {
        out.push(format!(
            "  {} reference(s) could not be rewritten (path:line:character, 0-based):",
            unhandled.len()
        ));
        out.extend(unhandled.iter().map(|site| format!("    {site}")));
    }
    if !glob_imports.is_empty() {
        out.push(format!(
            "  note: {} has glob imports ({}) that were not copied",
            source.display(),
            glob_imports.join(", ")
        ));
    }

    // Say whether the result compiles, rather than leaving the caller to guess.
    let directory = manifest.parent().unwrap_or(Path::new("."));
    let check = async {
        let workspace = workspace_root(directory).await?;
        let args = ["check", "--all-targets", "--message-format=json"];
        let run = run_cargo_json(&args, directory).await?;
        anyhow::Ok((workspace, run))
    };
    match check.await {
        Ok((workspace, run)) => {
            let (errors, warnings) = count_levels(&run.messages);
            let keys: Vec<String> = touched.iter().map(|path| path_key(path)).collect();
            let in_touched: Vec<&Value> = run
                .messages
                .iter()
                .filter(|message| {
                    keys.iter()
                        .any(|key| message_in_file(message, &workspace, key))
                })
                .collect();
            out.push(String::new());
            if errors == 0 {
                out.push(format!(
                    "cargo check passes ({warnings} warning(s), {} in the files changed).",
                    in_touched.len()
                ));
            } else {
                out.push(format!(
                    "cargo check reports {errors} error(s) after the move; these need fixing by hand:"
                ));
                out.extend(
                    run.messages
                        .iter()
                        .filter(|message| message_level(message) == "error")
                        .take(10)
                        .map(|message| message_rendered(message).to_string()),
                );
            }
        }
        Err(error) => out.push(format!(
            "\ncargo check could not run to verify the move: {error:#}"
        )),
    }

    Ok(ToolResult::text(out.join("\n")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn statement_of(text: &str) -> UseStatement {
        use_statements(text).remove(0)
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

    #[test]
    fn module_paths_follow_the_file_layout() {
        let src = std::env::temp_dir().join("crate_root").join("src");
        let root = src.join("lib.rs");
        let path = |file: PathBuf| module_path(&root, &file).map(|segments| segments.join("::"));

        assert_eq!(path(root.clone()).unwrap(), "crate");
        assert_eq!(path(src.join("shapes.rs")).unwrap(), "crate::shapes");
        assert_eq!(
            path(src.join("tools").join("mod.rs")).unwrap(),
            "crate::tools"
        );
        assert_eq!(
            path(src.join("tools").join("advanced.rs")).unwrap(),
            "crate::tools::advanced"
        );
        assert!(path(std::env::temp_dir().join("elsewhere.rs")).is_none());
        assert!(path(src.join("notes.txt")).is_none());
    }

    #[test]
    fn use_statements_are_found_with_their_visibility() {
        let text = "// use this\nuse a::b;\npub(crate) use c::{d, e};\nmod m {\n    pub use super::x;\n}\nfn f() -> impl Sized + use<> {}\nconst S: &str = \"use z;\";\n";

        let statements = use_statements(text);

        assert_eq!(statements.len(), 3);
        assert_eq!(&text[statements[0].start..statements[0].end], "use a::b;");
        assert_eq!(
            &text[statements[1].start..statements[1].end],
            "pub(crate) use c::{d, e};"
        );
        assert!(statements[1].top_level);
        assert_eq!(
            &text[statements[2].start..statements[2].end],
            "pub use super::x;"
        );
        assert!(!statements[2].top_level);
    }

    #[test]
    fn use_trees_are_flattened_into_bindings() {
        let text = "use a::{b::{self, C as D}, e::*, f};";

        let bindings = use_bindings(text, &statement_of(text));

        let summary: Vec<(String, &str, bool)> = bindings
            .iter()
            .map(|b| (b.path.join("::"), b.name.as_str(), b.glob))
            .collect();
        assert_eq!(
            summary,
            [
                ("a::b".to_string(), "b", false),
                ("a::b::C".to_string(), "D", false),
                ("a::e".to_string(), "*", true),
                ("a::f".to_string(), "f", false),
            ]
        );
        assert!(text[bindings[1].offset..].starts_with("C as D"));
    }

    #[test]
    fn qualified_references_get_the_new_path() {
        let text = "fn f() { let a = crate::shapes::perimeter(s) + shapes::perimeter(s); }";
        let first = text.find("perimeter").unwrap();
        let second = text.rfind("perimeter").unwrap();

        let site = classify_site(text, &[], first, first + 9, "crate::geometry", false);
        assert_eq!(
            site,
            Site::Edit((
                text.find("crate::shapes").unwrap(),
                first,
                "crate::geometry::".to_string()
            ))
        );

        // In the item's new home the qualifier just goes away.
        let site = classify_site(text, &[], second, second + 9, "crate::geometry", true);
        assert_eq!(
            site,
            Site::Edit((text.rfind("shapes::").unwrap(), second, String::new()))
        );
    }

    #[test]
    fn unqualified_and_unusual_references_are_told_apart() {
        let bare = "fn f() { perimeter(s) }";
        let start = bare.find("perimeter").unwrap();
        assert_eq!(
            classify_site(bare, &[], start, start + 9, "crate::g", false),
            Site::Bare
        );

        let generic = "fn f() { Wrapper::<u8>::perimeter(s) }";
        let start = generic.find("perimeter").unwrap();
        assert_eq!(
            classify_site(generic, &[], start, start + 9, "crate::g", false),
            Site::Unhandled
        );
    }

    #[test]
    fn simple_imports_have_their_path_replaced() {
        let text = "pub use super::shapes::perimeter as p;";
        let uses = use_statements(text);
        let start = text.find("perimeter").unwrap();

        let site = classify_site(text, &uses, start, start + 9, "crate::geometry", false);

        let Site::Edit(edit) = site else {
            panic!("expected an edit, got {site:?}");
        };
        assert_eq!(
            apply_replacements(text, vec![edit]).unwrap(),
            "pub use crate::geometry::perimeter as p;"
        );
    }

    #[test]
    fn grouped_imports_lose_the_entry_and_gain_a_statement() {
        let text = "    use crate::shapes::{self, perimeter as p, Square};\n";
        let uses = use_statements(text);
        let start = text.find("perimeter").unwrap();

        let site = classify_site(text, &uses, start, start + 9, "crate::geometry", false);
        assert_eq!(
            site,
            Site::UseEntry {
                statement: 0,
                alias: Some("p".to_string())
            }
        );

        let additions = ["use crate::geometry::perimeter as p;".to_string()];
        let edit = rewrite_use(text, &uses[0], &[start], &additions);
        assert_eq!(
            apply_replacements(text, vec![edit]).unwrap(),
            "    use crate::shapes::{self, Square};\n    use crate::geometry::perimeter as p;\n"
        );
    }

    #[test]
    fn use_entries_are_removed_cleanly_from_any_position() {
        let rewrite = |text: &str, names: &[&str]| {
            let removals: Vec<usize> = names.iter().map(|n| text.find(n).unwrap()).collect();
            let statement = use_statements(text)
                .into_iter()
                .find(|s| s.keyword < removals[0] && removals[0] < s.end)
                .unwrap();
            apply_replacements(text, vec![rewrite_use(text, &statement, &removals, &[])]).unwrap()
        };

        assert_eq!(rewrite("use a::{B, C, D};", &["B"]), "use a::{C, D};");
        assert_eq!(rewrite("use a::{B, C, D};", &["C"]), "use a::{B, D};");
        assert_eq!(rewrite("use a::{B, C, D};", &["D"]), "use a::{B, C};");
        assert_eq!(rewrite("use a::{B, C, D};", &["B", "D"]), "use a::{C};");
        assert_eq!(
            rewrite("use a::{\n    B,\n    C,\n};\n", &["C"]),
            "use a::{\n    B,\n};\n"
        );
        // Nothing left: the whole line goes.
        assert_eq!(
            rewrite("use x;\nuse a::{B, C};\nfn f() {}\n", &["B", "C"]),
            "use x;\nfn f() {}\n"
        );
        assert_eq!(rewrite("use a::B;\nfn f() {}\n", &["B"]), "fn f() {}\n");
    }

    #[test]
    fn visibility_is_inserted_after_attributes_and_docs() {
        let text =
            "/// Doc with fn in it.\n#[inline]\n#[cfg(all(unix, not(test)))]\nfn helper() {}\n";
        let offset = visibility_insertion(text, 0, text.len()).unwrap();
        assert!(text[offset..].starts_with("fn helper"));

        for visible in [
            "pub fn f() {}",
            "pub(crate) struct S;",
            "impl S {}",
            "macro_rules! m { () => {} }",
        ] {
            assert_eq!(visibility_insertion(visible, 0, visible.len()), None);
        }
    }

    #[test]
    fn imports_go_after_existing_ones_or_at_the_top() {
        let add = |text: &str| {
            let edit = imports_edit(text, &["use x::Y;".to_string()]);
            apply_replacements(text, vec![edit]).unwrap()
        };

        assert_eq!(
            add("use a::B;\n\nfn f() {}\n"),
            "use a::B;\nuse x::Y;\n\nfn f() {}\n"
        );
        assert_eq!(add("fn f() {}\n"), "use x::Y;\n\nfn f() {}\n");
        assert_eq!(
            add("//! Docs.\n\nfn f() {}\n"),
            "//! Docs.\n\nuse x::Y;\n\nfn f() {}\n"
        );
        assert_eq!(add(""), "use x::Y;\n");
    }

    #[test]
    fn relative_import_paths_are_made_absolute() {
        let module: Vec<String> = ["crate", "a", "b"].map(String::from).to_vec();
        let resolve = |path: &str| {
            let path: Vec<String> = path.split("::").map(String::from).collect();
            absolute_path(&path, &module, &["child"]).map(|p| p.join("::"))
        };

        assert_eq!(resolve("crate::x::Y").unwrap(), "crate::x::Y");
        assert_eq!(resolve("self::x::Y").unwrap(), "crate::a::b::x::Y");
        assert_eq!(resolve("super::Y").unwrap(), "crate::a::Y");
        assert_eq!(resolve("super::super::Y").unwrap(), "crate::Y");
        assert_eq!(resolve("child::Y").unwrap(), "crate::a::b::child::Y");
        assert_eq!(resolve("std::fmt::Display").unwrap(), "std::fmt::Display");
        assert!(resolve("super::super::super::Y").is_none());
    }
}
