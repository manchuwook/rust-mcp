use crate::analyzer::RustAnalyzerClient;
use crate::analyzer::edit::offset_to_position;
use crate::analyzer::protocol::normalize_path;
use crate::tools::refactoring::pick_action;
use crate::tools::support::{
    append_item, array_arg, check_identifier, code_chars, contains_word, find_matching_brace,
    find_word, read_source, str_arg, str_list_arg, write_source,
};
use crate::tools::types::ToolResult;
use anyhow::{Result, anyhow, bail};
use serde_json::Value;
use std::path::Path;

const INDENT: &str = "    ";

fn derive_attribute(derives: &[&str]) -> String {
    if derives.is_empty() {
        String::new()
    } else {
        format!("#[derive({})]\n", derives.join(", "))
    }
}

fn doc_comment(item: &Value, indent: &str) -> String {
    match item.get("doc").and_then(Value::as_str) {
        Some(doc) => doc
            .lines()
            .map(|line| format!("{indent}/// {line}").trim_end().to_string() + "\n")
            .collect(),
        None => String::new(),
    }
}

/// A `{name, type}` object as `name: Type`.
fn named_field(field: &Value, what: &str) -> Result<String> {
    let name = field
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("every {what} needs a `name`: {field}"))?;
    check_identifier(name, "field name")?;
    let field_type = ["type", "field_type", "ty"]
        .iter()
        .find_map(|key| field.get(*key).and_then(Value::as_str))
        .ok_or_else(|| anyhow!("{what} `{name}` needs a `type`"))?;
    Ok(format!("{name}: {field_type}"))
}

/// The visibility prefix for a struct field: `visibility: "pub(crate)"` or
/// `public: true`; private by default.
fn field_visibility(field: &Value) -> String {
    match field.get("visibility").and_then(Value::as_str) {
        Some("" | "private") => String::new(),
        Some(visibility) => format!("{visibility} "),
        None => {
            let public = ["public", "pub"]
                .iter()
                .any(|key| field.get(*key).and_then(Value::as_bool) == Some(true));
            if public {
                "pub ".to_string()
            } else {
                String::new()
            }
        }
    }
}

/// Render a struct definition from `{name, type, public?, doc?}` field objects.
pub fn render_struct(struct_name: &str, fields: &[Value], derives: &[&str]) -> Result<String> {
    check_identifier(struct_name, "struct name")?;

    let mut code = derive_attribute(derives);
    if fields.is_empty() {
        code.push_str(&format!("pub struct {struct_name};\n"));
        return Ok(code);
    }

    code.push_str(&format!("pub struct {struct_name} {{\n"));
    for field in fields {
        code.push_str(&doc_comment(field, INDENT));
        code.push_str(&format!(
            "{INDENT}{}{},\n",
            field_visibility(field),
            named_field(field, "field")?
        ));
    }
    code.push_str("}\n");
    Ok(code)
}

/// Render an enum definition. A variant is a name, or a `{name, fields?, doc?}`
/// object whose `fields` are types (tuple variant) or `{name, type}` objects
/// (struct variant).
pub fn render_enum(enum_name: &str, variants: &[Value], derives: &[&str]) -> Result<String> {
    check_identifier(enum_name, "enum name")?;
    if variants.is_empty() {
        bail!("an enum needs at least one variant");
    }

    let mut code = derive_attribute(derives);
    code.push_str(&format!("pub enum {enum_name} {{\n"));
    for variant in variants {
        let name = variant
            .as_str()
            .or_else(|| variant.get("name").and_then(Value::as_str))
            .ok_or_else(|| anyhow!("every variant needs a `name`: {variant}"))?;
        check_identifier(name, "variant name")?;

        let fields = variant
            .get("fields")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let payload = if fields.is_empty() {
            String::new()
        } else if fields.iter().all(Value::is_string) {
            let types: Vec<&str> = fields.iter().filter_map(Value::as_str).collect();
            format!("({})", types.join(", "))
        } else if fields.iter().all(Value::is_object) {
            let named = fields
                .iter()
                .map(|field| named_field(field, "variant field"))
                .collect::<Result<Vec<_>>>()?;
            format!(" {{ {} }}", named.join(", "))
        } else {
            bail!("the fields of variant `{name}` must be all types or all {{name, type}} objects");
        };

        code.push_str(&doc_comment(variant, INDENT));
        code.push_str(&format!("{INDENT}{name}{payload},\n"));
    }
    code.push_str("}\n");
    Ok(code)
}

/// Refuse to add a second definition of a type that is already in the file.
fn ensure_not_defined(file: &Path, keyword: &str, name: &str) -> Result<()> {
    if !file.exists() {
        return Ok(());
    }
    let text = read_source(file)?;
    let mut from = 0;
    while let Some(found) = find_word(&text, keyword, from) {
        from = found + keyword.len();
        if find_word(text[from..].trim_start(), name, 0) == Some(0) {
            bail!("{} already defines `{keyword} {name}`", file.display());
        }
    }
    Ok(())
}

pub async fn generate_struct_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let struct_name = str_arg(&args, "struct_name")?;
    let fields = array_arg(&args, "fields")?;
    let derives = str_list_arg(&args, "derives");
    let file_path = str_arg(&args, "file_path")?;

    let code = render_struct(struct_name, fields, &derives)?;
    let file = normalize_path(Path::new(file_path));
    ensure_not_defined(&file, "struct", struct_name)?;
    append_item(&file, &code)?;

    Ok(ToolResult::text(format!(
        "Added struct '{struct_name}' to {}:\n\n{code}",
        file.display()
    )))
}

pub async fn generate_enum_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let enum_name = str_arg(&args, "enum_name")?;
    let variants = array_arg(&args, "variants")?;
    let derives = str_list_arg(&args, "derives");
    let file_path = str_arg(&args, "file_path")?;

    let code = render_enum(enum_name, variants, &derives)?;
    let file = normalize_path(Path::new(file_path));
    ensure_not_defined(&file, "enum", enum_name)?;
    append_item(&file, &code)?;

    Ok(ToolResult::text(format!(
        "Added enum '{enum_name}' to {}:\n\n{code}",
        file.display()
    )))
}

pub async fn generate_trait_impl_impl(
    args: Value,
    analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let trait_name = str_arg(&args, "trait_name")?;
    let struct_name = str_arg(&args, "struct_name")?;
    let file_path = str_arg(&args, "file_path")?;

    let file = normalize_path(Path::new(file_path));
    if !file.is_file() {
        bail!("{} does not exist", file.display());
    }
    let header = format!("impl {trait_name} for {struct_name}");
    if read_source(&file)?.contains(&header) {
        bail!("{} already contains `{header}`", file.display());
    }

    append_item(&file, &format!("{header} {{}}"))?;

    // Let rust-analyzer fill in the trait's required items. It can only do so
    // if it can resolve the trait from this file.
    let text = read_source(&file)?;
    let impl_offset = text
        .rfind(&header)
        .ok_or_else(|| anyhow!("could not find the impl that was just written"))?;
    let position = offset_to_position(&text, impl_offset + "impl ".len());
    let actions = analyzer.code_actions(file_path, position, position).await?;
    let filled = match pick_action(
        &actions,
        &["add_impl_missing_members"],
        &["Implement missing members"],
    ) {
        Some(action) => {
            analyzer.apply_code_action(action).await?;
            true
        }
        None => false,
    };

    let text = read_source(&file)?;
    let generated = text
        .rfind(&header)
        .map_or("", |start| text[start..].trim_end());
    let note = if filled {
        "with the trait's required items stubbed out"
    } else {
        "left empty: rust-analyzer found no required items to add (the trait has none, or \
         it is not in scope in this file)"
    };
    Ok(ToolResult::text(format!(
        "Added `{header}` to {}, {note}:\n\n{generated}",
        file.display()
    )))
}

/// A test-case value as Rust source: strings are taken as Rust expressions
/// verbatim, numbers and booleans as literals.
fn rust_expression(value: &Value) -> Result<String> {
    match value {
        Value::String(expression) => Ok(expression.clone()),
        Value::Number(_) | Value::Bool(_) => Ok(value.to_string()),
        other => Err(anyhow!(
            "test case values must be Rust expressions given as strings, numbers or booleans, \
             got: {other}"
        )),
    }
}

fn snake_case(name: &str) -> String {
    let mut snake = String::new();
    for c in name.chars() {
        if c.is_alphanumeric() {
            if c.is_uppercase() && snake.chars().next_back().is_some_and(char::is_lowercase) {
                snake.push('_');
            }
            snake.extend(c.to_lowercase());
        } else if !snake.ends_with('_') && !snake.is_empty() {
            snake.push('_');
        }
    }
    snake.trim_end_matches('_').to_string()
}

/// Render `#[test]` functions (unindented) and return them with their names.
///
/// Each case is `{name?, args?, expected?}`. With no cases, a single test
/// containing `todo!()` is produced so that it fails until it is written.
pub fn render_tests(target_function: &str, test_cases: &[Value]) -> Result<Vec<(String, String)>> {
    let base = format!("test_{}", snake_case(target_function));

    if test_cases.is_empty() {
        let code = format!(
            "#[test]\nfn {base}() {{\n{INDENT}todo!(\"write a test for `{target_function}`\");\n}}\n"
        );
        return Ok(vec![(base, code)]);
    }

    let mut tests = Vec::new();
    for (index, case) in test_cases.iter().enumerate() {
        let suffix = match case.get("name").and_then(Value::as_str).map(snake_case) {
            Some(name) if !name.is_empty() => name,
            _ => format!("case_{}", index + 1),
        };
        let name = format!("{base}_{suffix}");

        let args = match case.get("args").or_else(|| case.get("input")) {
            None | Some(Value::Null) => String::new(),
            Some(Value::Array(args)) => args
                .iter()
                .map(rust_expression)
                .collect::<Result<Vec<_>>>()?
                .join(", "),
            Some(single) => rust_expression(single)?,
        };
        let call = format!("{target_function}({args})");

        let body = match case.get("expected") {
            None | Some(Value::Null) => format!("let _ = {call};"),
            Some(expected) => format!("assert_eq!({call}, {});", rust_expression(expected)?),
        };
        let code = format!("#[test]\nfn {name}() {{\n{INDENT}{body}\n}}\n");

        if tests.iter().any(|(existing, _)| *existing == name) {
            bail!("two test cases produce the same test name `{name}`");
        }
        tests.push((name, code));
    }
    Ok(tests)
}

fn indent(code: &str) -> String {
    code.lines()
        .map(|line| {
            if line.is_empty() {
                "\n".to_string()
            } else {
                format!("{INDENT}{line}\n")
            }
        })
        .collect()
}

/// Add test functions to `source`: into its `mod tests` if it has one,
/// otherwise in a new `#[cfg(test)] mod tests` at the end.
pub fn insert_tests(source: &str, tests: &[(String, String)]) -> Result<String> {
    let crlf = source.contains("\r\n");
    let text = source.replace("\r\n", "\n");

    for (name, _) in tests {
        if contains_word(&text, name) {
            bail!("a test named `{name}` already exists");
        }
    }
    let body = indent(
        &tests
            .iter()
            .map(|(_, code)| code.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
    );

    let existing_module = find_word(&text, "mod tests", 0).and_then(|start| {
        let open = code_chars(&text, start).find(|(_, c)| *c == '{' || *c == ';')?;
        (open.1 == '{').then(|| find_matching_brace(&text, open.0))?
    });

    let updated = match existing_module {
        Some(close) => {
            let head = text[..close].trim_end();
            let separator = if head.ends_with('{') { "\n" } else { "\n\n" };
            format!("{head}{separator}{body}{}", &text[close..])
        }
        None => {
            let mut head = text.trim_end().to_string();
            if !head.is_empty() {
                head.push_str("\n\n");
            }
            format!("{head}#[cfg(test)]\nmod tests {{\n{INDENT}use super::*;\n\n{body}}}\n")
        }
    };

    Ok(if crlf {
        updated.replace('\n', "\r\n")
    } else {
        updated
    })
}

pub async fn generate_tests_impl(
    args: Value,
    _analyzer: &mut RustAnalyzerClient,
) -> Result<ToolResult> {
    let target_function = str_arg(&args, "target_function")?;
    let file_path = str_arg(&args, "file_path")?;
    let empty_cases = vec![];
    let test_cases = args
        .get("test_cases")
        .and_then(|v| v.as_array())
        .unwrap_or(&empty_cases);

    let file = normalize_path(Path::new(file_path));
    let source = read_source(&file)?;

    // `Type::method` is defined as `fn method`.
    let function_name = target_function
        .rsplit("::")
        .next()
        .unwrap_or(target_function);
    if find_word(&source, &format!("fn {function_name}"), 0).is_none() {
        bail!(
            "{} does not define a function named `{function_name}`",
            file.display()
        );
    }

    let tests = render_tests(target_function, test_cases)?;
    let updated = insert_tests(&source, &tests)?;
    write_source(&file, &updated)?;

    let names: Vec<&str> = tests.iter().map(|(name, _)| name.as_str()).collect();
    Ok(ToolResult::text(format!(
        "Added {} test(s) for '{target_function}' to {}:\n  {}",
        tests.len(),
        file.display(),
        names.join("\n  ")
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn struct_is_rendered_with_derives_visibility_and_docs() {
        let fields = [
            json!({ "name": "id", "type": "u64", "public": true, "doc": "Unique id." }),
            json!({ "name": "tags", "type": "Vec<String>" }),
            json!({ "name": "owner", "type": "String", "visibility": "pub(crate)" }),
        ];

        let code = render_struct("User", &fields, &["Debug", "Clone"]).unwrap();

        assert_eq!(
            code,
            "#[derive(Debug, Clone)]\npub struct User {\n    /// Unique id.\n    pub id: u64,\n    tags: Vec<String>,\n    pub(crate) owner: String,\n}\n"
        );
    }

    #[test]
    fn struct_without_fields_is_a_unit_struct() {
        assert_eq!(
            render_struct("Marker", &[], &[]).unwrap(),
            "pub struct Marker;\n"
        );
    }

    #[test]
    fn struct_fields_are_validated() {
        assert!(render_struct("User", &[json!({ "name": "id" })], &[]).is_err());
        assert!(render_struct("User", &[json!({ "type": "u64" })], &[]).is_err());
        assert!(render_struct("not valid", &[], &[]).is_err());
    }

    #[test]
    fn enum_supports_unit_tuple_and_struct_variants() {
        let variants = [
            json!("Idle"),
            json!({ "name": "Moved", "fields": ["i32", "i32"], "doc": "Moved by an offset." }),
            json!({ "name": "Named", "fields": [{ "name": "label", "type": "String" }] }),
        ];

        let code = render_enum("Event", &variants, &["Debug"]).unwrap();

        assert_eq!(
            code,
            "#[derive(Debug)]\npub enum Event {\n    Idle,\n    /// Moved by an offset.\n    Moved(i32, i32),\n    Named { label: String },\n}\n"
        );
        assert!(render_enum("Empty", &[], &[]).is_err());
    }

    #[test]
    fn tests_are_rendered_from_cases() {
        let cases = [
            json!({ "name": "small numbers", "args": [1, "2"], "expected": 3 }),
            json!({ "args": "-1, 1" }),
        ];

        let tests = render_tests("add", &cases).unwrap();

        assert_eq!(tests[0].0, "test_add_small_numbers");
        assert_eq!(
            tests[0].1,
            "#[test]\nfn test_add_small_numbers() {\n    assert_eq!(add(1, 2), 3);\n}\n"
        );
        assert_eq!(tests[1].0, "test_add_case_2");
        assert!(tests[1].1.contains("let _ = add(-1, 1);"));
    }

    #[test]
    fn a_failing_skeleton_is_rendered_without_cases() {
        let tests = render_tests("Parser::new", &[]).unwrap();

        assert_eq!(tests[0].0, "test_parser_new");
        assert!(
            tests[0]
                .1
                .contains("todo!(\"write a test for `Parser::new`\");")
        );
    }

    #[test]
    fn tests_go_into_a_new_module_when_there_is_none() {
        let tests = render_tests(
            "add",
            &[json!({ "name": "one", "args": [1, 1], "expected": 2 })],
        )
        .unwrap();

        let updated = insert_tests("fn add(a: i32, b: i32) -> i32 { a + b }\n", &tests).unwrap();

        assert_eq!(
            updated,
            "fn add(a: i32, b: i32) -> i32 { a + b }\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn test_add_one() {\n        assert_eq!(add(1, 1), 2);\n    }\n}\n"
        );
    }

    #[test]
    fn tests_are_added_to_an_existing_module() {
        let source = "fn add() {}\n\n#[cfg(test)]\nmod tests {\n    use super::*;\n\n    #[test]\n    fn existing() {\n        let s = \"}\";\n    }\n}\n";
        let tests = render_tests("add", &[]).unwrap();

        let updated = insert_tests(source, &tests).unwrap();

        assert!(updated.ends_with(
            "        let s = \"}\";\n    }\n\n    #[test]\n    fn test_add() {\n        todo!(\"write a test for `add`\");\n    }\n}\n"
        ));
        assert_eq!(updated.matches("mod tests").count(), 1);

        // Adding the same test twice is an error rather than a duplicate.
        assert!(insert_tests(&updated, &tests).is_err());
    }
}
