// End-to-end tests: each one copies the fixture crate to a temporary
// directory and drives the tools against it, with a real rust-analyzer and
// real cargo.
//
// Tests that need rust-analyzer are skipped (with a note on stderr) when it is
// not installed.

use rustmcp::analyzer::RustAnalyzerClient;
use rustmcp::analyzer::edit::offset_to_position;
use rustmcp::tools::execute_tool;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use tempfile::TempDir;

struct Fixture {
    // Held so the directory lives as long as the test.
    _dir: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample_crate");
        let root = dir.path().join("sample_crate");
        copy_dir(&source, &root);
        Self { _dir: dir, root }
    }

    fn path(&self, relative: &str) -> String {
        self.root.join(relative).to_string_lossy().into_owned()
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.root.join(relative))
            .unwrap()
            .replace("\r\n", "\n")
    }

    /// The 0-based `(line, character)` of the first occurrence of `needle`.
    fn position(&self, relative: &str, needle: &str) -> (u32, u32) {
        let text = std::fs::read_to_string(self.root.join(relative)).unwrap();
        let offset = text
            .find(needle)
            .unwrap_or_else(|| panic!("`{needle}` not found in {relative}"));
        offset_to_position(&text, offset)
    }

    /// A client with rust-analyzer running on the fixture, or `None` if
    /// rust-analyzer is not available.
    async fn analyzer(&self) -> Option<RustAnalyzerClient> {
        let mut client = RustAnalyzerClient::new();
        match client.start_in(&self.root).await {
            Ok(()) => Some(client),
            Err(error) => {
                eprintln!("skipping: rust-analyzer is not available ({error:#})");
                None
            }
        }
    }
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_dir(&entry.path(), &target);
        } else {
            std::fs::copy(entry.path(), target).unwrap();
        }
    }
}

async fn call(
    analyzer: &mut RustAnalyzerClient,
    name: &str,
    args: Value,
) -> anyhow::Result<String> {
    let result = execute_tool(name, args, analyzer).await?;
    Ok(result
        .content
        .iter()
        .filter_map(|content| content.get("text")?.as_str())
        .collect::<Vec<_>>()
        .join("\n"))
}

/// `cargo check` output for the fixture, via the tool.
async fn cargo_check(fixture: &Fixture) -> String {
    let mut offline = RustAnalyzerClient::new();
    call(
        &mut offline,
        "run_cargo_check",
        json!({ "workspace_path": fixture.path("") }),
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn cargo_tools_report_on_the_crate() {
    let fixture = Fixture::new();
    // These tools only use cargo, so they work without rust-analyzer running.
    let mut analyzer = RustAnalyzerClient::new();

    let manifest = call(
        &mut analyzer,
        "analyze_manifest",
        json!({ "manifest_path": fixture.path("Cargo.toml") }),
    )
    .await
    .unwrap();
    assert!(
        manifest.contains("Package sample_crate 0.1.0 (edition 2021)"),
        "{manifest}"
    );
    assert!(
        manifest.contains("Targets: sample_crate (lib)"),
        "{manifest}"
    );

    let check = cargo_check(&fixture).await;
    assert!(
        check.contains("unused import: `std::collections::HashMap`"),
        "{check}"
    );
    assert!(check.contains("cargo check passed"), "{check}");
    assert!(check.contains("0 error(s), 1 warning(s)"), "{check}");

    // A broken crate is reported as a failed check, not as a tool error.
    std::fs::write(
        fixture.root.join("src/shapes.rs"),
        "pub fn broken() -> i32 { \"no\" }\n",
    )
    .unwrap();
    let check = cargo_check(&fixture).await;
    assert!(check.contains("mismatched types"), "{check}");
    assert!(check.contains("cargo check failed"), "{check}");

    let missing = call(
        &mut analyzer,
        "run_cargo_check",
        json!({ "workspace_path": fixture.path("no/such/dir") }),
    )
    .await;
    assert!(missing.is_err());
}

#[tokio::test]
async fn clippy_suggestions_are_applied_to_the_file() {
    let fixture = Fixture::new();
    let mut analyzer = RustAnalyzerClient::new();

    let output = call(
        &mut analyzer,
        "apply_clippy_suggestions",
        json!({ "file_path": fixture.path("src/lib.rs") }),
    )
    .await
    .unwrap();

    assert!(output.contains("fixed clippy::needless_return"), "{output}");
    let lib = fixture.read("src/lib.rs");
    assert!(
        lib.contains("pub fn triple(x: i32) -> i32 {\n    x * 3\n}"),
        "{lib}"
    );
    assert!(!cargo_check(&fixture).await.contains("cargo check failed"));
}

#[tokio::test]
async fn lifetime_problems_are_reported_for_the_file() {
    let fixture = Fixture::new();
    let mut analyzer = RustAnalyzerClient::new();
    let args = json!({ "file_path": fixture.path("src/shapes.rs") });

    let clean = call(&mut analyzer, "validate_lifetimes", args.clone())
        .await
        .unwrap();
    assert!(clean.contains("No lifetime or borrow issues"), "{clean}");

    let mut shapes = fixture.read("src/shapes.rs");
    shapes.push_str("\npub fn longest(a: &str, b: &str) -> &str {\n    if a.len() > b.len() { a } else { b }\n}\n");
    std::fs::write(fixture.root.join("src/shapes.rs"), shapes).unwrap();

    let broken = call(&mut analyzer, "validate_lifetimes", args)
        .await
        .unwrap();
    assert!(broken.contains("1 lifetime or borrow issue(s)"), "{broken}");
    assert!(broken.contains("E0106"), "{broken}");
}

#[tokio::test]
async fn items_are_generated_into_a_file_that_still_compiles() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let lib = fixture.path("src/lib.rs");

    call(
        &mut analyzer,
        "generate_struct",
        json!({
            "struct_name": "Rectangle",
            "fields": [
                { "name": "width", "type": "f64", "public": true },
                { "name": "height", "type": "f64", "public": true }
            ],
            "derives": ["Debug", "Clone"],
            "file_path": lib
        }),
    )
    .await
    .unwrap();
    call(
        &mut analyzer,
        "generate_enum",
        json!({
            "enum_name": "Unit",
            "variants": ["Metres", { "name": "Custom", "fields": ["String"] }],
            "derives": ["Debug"],
            "file_path": lib
        }),
    )
    .await
    .unwrap();
    let trait_impl = call(
        &mut analyzer,
        "generate_trait_impl",
        json!({ "trait_name": "Shape", "struct_name": "Rectangle", "file_path": lib }),
    )
    .await
    .unwrap();
    call(
        &mut analyzer,
        "generate_tests",
        json!({
            "target_function": "add",
            "file_path": lib,
            "test_cases": [{ "name": "small", "args": [1, 2], "expected": 3 }]
        }),
    )
    .await
    .unwrap();

    let text = fixture.read("src/lib.rs");
    assert!(
        text.contains("#[derive(Debug, Clone)]\npub struct Rectangle {\n    pub width: f64,\n    pub height: f64,\n}"),
        "{text}"
    );
    assert!(
        text.contains("pub enum Unit {\n    Metres,\n    Custom(String),\n}"),
        "{text}"
    );
    assert!(
        trait_impl.contains("required items stubbed out"),
        "{trait_impl}"
    );
    assert!(text.contains("impl Shape for Rectangle {"), "{text}");
    assert!(text.contains("fn area(&self) -> f64 {"), "{text}");
    assert!(text.contains("fn name(&self) -> String {"), "{text}");
    assert!(
        text.contains("mod tests {\n    use super::*;\n\n    #[test]\n    fn test_add_small() {\n        assert_eq!(add(1, 2), 3);"),
        "{text}"
    );

    let check = cargo_check(&fixture).await;
    assert!(check.contains("0 error(s)"), "{check}");

    // Generating the same struct again is refused rather than duplicated.
    let again = call(
        &mut analyzer,
        "generate_struct",
        json!({ "struct_name": "Rectangle", "fields": [], "file_path": lib }),
    )
    .await;
    assert!(again.is_err());
}

#[tokio::test]
async fn navigation_tools_locate_symbols() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let lib = fixture.path("src/lib.rs");

    let (call_line, call_character) = fixture.position("src/lib.rs", "add(x, x)");
    let (definition_line, _) = fixture.position("src/lib.rs", "pub fn add");
    let at_call = json!({ "file_path": lib, "line": call_line, "character": call_character });

    let definition = call(&mut analyzer, "find_definition", at_call.clone())
        .await
        .unwrap();
    assert!(
        definition.contains(&format!(
            "lib.rs:{definition_line}:7  pub fn add(a: i32, b: i32) -> i32 {{"
        )),
        "{definition}"
    );

    let references = call(&mut analyzer, "find_references", at_call)
        .await
        .unwrap();
    assert!(references.contains("2 reference(s)"), "{references}");
    assert!(references.contains("add(x, x)"), "{references}");

    let symbols = call(
        &mut analyzer,
        "workspace_symbols",
        json!({ "query": "Circle" }),
    )
    .await
    .unwrap();
    assert!(symbols.contains("struct Circle"), "{symbols}");
    assert!(symbols.contains("lib.rs:"), "{symbols}");

    let (trait_line, trait_character) = fixture.position("src/lib.rs", "Shape {");
    let hierarchy = call(
        &mut analyzer,
        "get_type_hierarchy",
        json!({ "file_path": lib, "line": trait_line, "character": trait_character }),
    )
    .await
    .unwrap();
    assert!(hierarchy.contains("trait Shape"), "{hierarchy}");
    assert!(hierarchy.contains("1 implementation(s)"), "{hierarchy}");
    assert!(hierarchy.contains("impl Shape for Square {"), "{hierarchy}");
}

#[tokio::test]
async fn diagnostics_combine_rust_analyzer_and_cargo() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let args = json!({ "file_path": fixture.path("src/lib.rs") });

    let clean = call(&mut analyzer, "get_diagnostics", args.clone())
        .await
        .unwrap();
    assert!(clean.contains("rust-analyzer: no diagnostics"), "{clean}");
    assert!(
        clean.contains("cargo check: 0 error(s), 1 warning(s) in this file"),
        "{clean}"
    );
    assert!(clean.contains("unused import"), "{clean}");

    // A syntax error is something rust-analyzer reports itself; the edit is
    // picked up from disk without restarting it.
    let mut lib = fixture.read("src/lib.rs");
    lib.push_str("\npub fn unfinished( {\n");
    std::fs::write(fixture.root.join("src/lib.rs"), lib).unwrap();

    let broken = call(&mut analyzer, "get_diagnostics", args).await.unwrap();
    assert!(
        !broken.contains("rust-analyzer: no diagnostics"),
        "{broken}"
    );
    assert!(broken.contains("  error "), "{broken}");
}

#[tokio::test]
async fn rename_updates_every_use_on_disk() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let (line, character) = fixture.position("src/lib.rs", "add(a: i32");

    let output = call(
        &mut analyzer,
        "rename_symbol",
        json!({
            "file_path": fixture.path("src/lib.rs"),
            "line": line,
            "character": character,
            "new_name": "sum_two"
        }),
    )
    .await
    .unwrap();

    assert!(output.contains("2 edit(s)"), "{output}");
    let lib = fixture.read("src/lib.rs");
    assert!(
        lib.contains("pub fn sum_two(a: i32, b: i32) -> i32"),
        "{lib}"
    );
    assert!(lib.contains("sum_two(x, x)"), "{lib}");
    assert!(cargo_check(&fixture).await.contains("0 error(s)"));
}

#[tokio::test]
async fn extract_function_uses_the_requested_name() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let expression = "total * 2 + 1";
    let (line, start) = fixture.position("src/lib.rs", expression);

    let output = call(
        &mut analyzer,
        "extract_function",
        json!({
            "file_path": fixture.path("src/lib.rs"),
            "start_line": line,
            "start_character": start,
            "end_line": line,
            "end_character": start + expression.len() as u32,
            "function_name": "scale"
        }),
    )
    .await
    .unwrap();

    assert!(output.contains("into function 'scale'"), "{output}");
    let lib = fixture.read("src/lib.rs");
    assert!(lib.contains("let scaled = scale(total);"), "{lib}");
    assert!(
        lib.contains("fn scale(total: i32) -> i32 {\n    total * 2 + 1\n}"),
        "{lib}"
    );
    assert!(!lib.contains("fun_name"), "{lib}");
    assert!(cargo_check(&fixture).await.contains("0 error(s)"));
}

#[tokio::test]
async fn inline_function_replaces_the_call() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let (line, character) = fixture.position("src/lib.rs", "add(x, x)");
    let args = json!({
        "file_path": fixture.path("src/lib.rs"),
        "line": line,
        "character": character
    });

    call(&mut analyzer, "inline_function", args).await.unwrap();

    let lib = fixture.read("src/lib.rs");
    assert!(
        lib.contains("pub fn double(x: i32) -> i32 {\n    x + x\n}"),
        "{lib}"
    );

    // Nothing to inline on a struct definition: an error, not a silent success.
    let (line, character) = fixture.position("src/lib.rs", "Circle");
    let nothing = call(
        &mut analyzer,
        "inline_function",
        json!({ "file_path": fixture.path("src/lib.rs"), "line": line, "character": character }),
    )
    .await;
    assert!(nothing.is_err());
}

#[tokio::test]
async fn organize_imports_removes_unused_and_sorts() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };

    let output = call(
        &mut analyzer,
        "organize_imports",
        json!({ "file_path": fixture.path("src/lib.rs") }),
    )
    .await
    .unwrap();

    assert!(output.contains("Removed unused imports"), "{output}");
    let lib = fixture.read("src/lib.rs");
    assert!(!lib.contains("HashMap"), "{lib}");
    assert!(lib.contains("use std::fmt::Display;"), "{lib}");
    assert!(
        cargo_check(&fixture)
            .await
            .contains("0 error(s), 0 warning(s)")
    );
}

#[tokio::test]
async fn format_code_rewrites_the_file() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let mut shapes = fixture.read("src/shapes.rs");
    shapes.push_str("\npub fn ugly( ) -> i32 {1}\n");
    std::fs::write(fixture.root.join("src/shapes.rs"), shapes).unwrap();
    let args = json!({ "file_path": fixture.path("src/shapes.rs") });

    let output = call(&mut analyzer, "format_code", args.clone())
        .await
        .unwrap();
    assert!(output.starts_with("Formatted"), "{output}");
    let shapes = fixture.read("src/shapes.rs");
    assert!(
        shapes.contains("pub fn ugly() -> i32 {\n    1\n}"),
        "{shapes}"
    );

    let again = call(&mut analyzer, "format_code", args).await.unwrap();
    assert!(again.contains("already formatted"), "{again}");
}

#[tokio::test]
async fn change_signature_rewrites_the_declaration_and_lists_call_sites() {
    let fixture = Fixture::new();
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };
    let (line, character) = fixture.position("src/lib.rs", "add(a: i32");

    let output = call(
        &mut analyzer,
        "change_signature",
        json!({
            "file_path": fixture.path("src/lib.rs"),
            "line": line,
            "character": character,
            "new_signature": "pub fn add(a: i32, b: i32, c: i32) -> i32"
        }),
    )
    .await
    .unwrap();

    let lib = fixture.read("src/lib.rs");
    assert!(
        lib.contains("pub fn add(a: i32, b: i32, c: i32) -> i32 {\n    a + b\n}"),
        "{lib}"
    );
    assert!(
        output.contains("was: pub fn add(a: i32, b: i32) -> i32"),
        "{output}"
    );
    assert!(
        output.contains("1 reference(s) were NOT updated"),
        "{output}"
    );
    assert!(output.contains("add(x, x)"), "{output}");
}

#[tokio::test]
async fn modules_are_created_and_declared() {
    let fixture = Fixture::new();
    let mut analyzer = RustAnalyzerClient::new();

    let created = call(
        &mut analyzer,
        "create_module",
        json!({ "module_name": "geometry", "module_path": fixture.path("src"), "is_public": true }),
    )
    .await
    .unwrap();
    assert!(
        created.contains("Declared `pub mod geometry;`"),
        "{created}"
    );
    assert!(
        fixture
            .read("src/lib.rs")
            .starts_with("pub mod shapes;\npub mod geometry;\n"),
        "{}",
        fixture.read("src/lib.rs")
    );
    assert_eq!(fixture.read("src/geometry.rs"), "");

    // Creating the same module twice is refused.
    let duplicate = call(
        &mut analyzer,
        "create_module",
        json!({ "module_name": "geometry", "module_path": fixture.path("src") }),
    )
    .await;
    assert!(duplicate.is_err());
}

/// Rework the fixture so that `perimeter` and `diagonal` are referred to in
/// every way a move has to cope with: a private helper and an import they
/// depend on, an unqualified use left behind in their own file, and a
/// grouped import plus qualified paths in another module.
fn add_users_of_shapes(fixture: &Fixture) {
    let write = |relative: &str, text: &str| {
        std::fs::write(fixture.root.join(relative), text).unwrap();
    };

    write(
        "src/shapes.rs",
        r#"use crate::Shape;
use std::f64::consts::SQRT_2;

/// A square.
pub struct Square {
    pub side: f64,
}

impl Shape for Square {
    fn area(&self) -> f64 {
        self.side * self.side
    }

    fn name(&self) -> String {
        "square".to_string()
    }
}

fn sides() -> f64 {
    4.0
}

/// The distance around a square.
pub fn perimeter(square: &Square) -> f64 {
    square.side * sides()
}

pub fn diagonal(square: &Square) -> f64 {
    square.side * SQRT_2
}

pub fn double_perimeter(square: &Square) -> f64 {
    perimeter(square) * 2.0
}
"#,
    );
    write(
        "src/report.rs",
        r#"use crate::shapes::{self, perimeter, Square};

pub fn describe(square: &Square) -> String {
    let around = perimeter(square);
    let again = shapes::perimeter(square);
    let full = crate::shapes::perimeter(square);
    format!("{around} {again} {full} {}", shapes::diagonal(square))
}
"#,
    );
    write("src/geometry.rs", "");
    let lib = fixture.read("src/lib.rs").replace(
        "pub mod shapes;\n",
        "pub mod geometry;\npub mod report;\npub mod shapes;\n",
    );
    write("src/lib.rs", &lib);
}

#[tokio::test]
async fn moving_items_keeps_the_crate_compiling() {
    let fixture = Fixture::new();
    add_users_of_shapes(&fixture);
    let Some(mut analyzer) = fixture.analyzer().await else {
        return;
    };

    // Into an existing module.
    let moved = call(
        &mut analyzer,
        "move_items",
        json!({
            "source_file": fixture.path("src/shapes.rs"),
            "target_file": fixture.path("src/geometry.rs"),
            "item_names": ["perimeter"]
        }),
    )
    .await
    .unwrap();
    assert!(moved.contains("Moved 1 item(s)"), "{moved}");
    assert!(moved.contains("made pub(crate): sides"), "{moved}");
    assert!(moved.contains("cargo check passes"), "{moved}");

    // The item arrives with the imports it needs.
    assert_eq!(
        fixture.read("src/geometry.rs"),
        "use crate::shapes::Square;\nuse crate::shapes::sides;\n\n/// The distance around a square.\npub fn perimeter(square: &Square) -> f64 {\n    square.side * sides()\n}\n"
    );
    // Its old file imports it for the code that still uses it, and exposes
    // the helper it depends on.
    let shapes = fixture.read("src/shapes.rs");
    assert!(!shapes.contains("fn perimeter"), "{shapes}");
    assert!(
        shapes.starts_with(
            "use crate::Shape;\nuse std::f64::consts::SQRT_2;\nuse crate::geometry::perimeter;\n"
        ),
        "{shapes}"
    );
    assert!(
        shapes.contains("pub(crate) fn sides() -> f64 {"),
        "{shapes}"
    );
    assert!(
        shapes.contains("    4.0\n}\n\npub fn diagonal(square: &Square) -> f64 {"),
        "{shapes}"
    );
    // Other modules are pointed at its new home.
    let report = fixture.read("src/report.rs");
    assert!(
        report.starts_with("use crate::shapes::{self, Square};\nuse crate::geometry::perimeter;\n"),
        "{report}"
    );
    assert!(
        report.contains("let around = perimeter(square);"),
        "{report}"
    );
    assert!(
        report.contains("let again = crate::geometry::perimeter(square);"),
        "{report}"
    );
    assert!(
        report.contains("let full = crate::geometry::perimeter(square);"),
        "{report}"
    );

    // Into a file that does not exist yet.
    let moved = call(
        &mut analyzer,
        "move_items",
        json!({
            "source_file": fixture.path("src/shapes.rs"),
            "target_file": fixture.path("src/extra.rs"),
            "item_names": ["diagonal"]
        }),
    )
    .await
    .unwrap();
    assert!(moved.contains("declared `pub mod extra;`"), "{moved}");
    assert!(moved.contains("cargo check passes"), "{moved}");
    assert_eq!(
        fixture.read("src/extra.rs"),
        "use std::f64::consts::SQRT_2;\nuse crate::shapes::Square;\n\npub fn diagonal(square: &Square) -> f64 {\n    square.side * SQRT_2\n}\n"
    );
    assert!(
        fixture
            .read("src/lib.rs")
            .contains("pub mod shapes;\npub mod extra;\n"),
        "{}",
        fixture.read("src/lib.rs")
    );
    // The import only `diagonal` used went with it.
    let shapes = fixture.read("src/shapes.rs");
    assert!(!shapes.contains("SQRT_2"), "{shapes}");
    assert!(
        fixture
            .read("src/report.rs")
            .contains("crate::extra::diagonal(square)"),
        "{}",
        fixture.read("src/report.rs")
    );
    assert!(cargo_check(&fixture).await.contains("0 error(s)"));

    // An unknown item is an error and changes nothing.
    let unknown = call(
        &mut analyzer,
        "move_items",
        json!({
            "source_file": fixture.path("src/shapes.rs"),
            "target_file": fixture.path("src/geometry.rs"),
            "item_names": ["double_perimeter", "missing"]
        }),
    )
    .await;
    assert!(unknown.unwrap_err().to_string().contains("missing"));
    assert_eq!(fixture.read("src/shapes.rs"), shapes);
}
