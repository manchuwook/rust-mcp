[![CI](https://github.com/dexwritescode/rust-mcp/actions/workflows/ci.yml/badge.svg)](https://github.com/dexwritescode/rust-mcp/actions/workflows/ci.yml)

# Rust MCP Server

A comprehensive Model Context Protocol (MCP) server that provides rust-analyzer integration for LLM-assisted Rust development. This server enables AI tools like Claude to work with Rust code idiomatically through rust-analyzer's Language Server Protocol capabilities, avoiding string manipulation and providing intelligent code analysis and refactoring.

## Quick Start

1. **Build**: `cargo build --release`
2. **Configure** your MCP client to use `target/release/rustmcp`
3. **Use** through AI assistants with natural language prompts like "Generate a User struct with Debug and Clone derives"

## Features - Complete Tool Suite (22 Tools)

Every tool either does its work on disk or returns an error; none report success for work that did not happen. Positions (`line`, `character`) are 0-based, in both arguments and results.

### Code Analysis (4 tools)
- `find_definition` - Location and source line of the definition of the symbol at a position
- `find_references` - Every use of the symbol at a position
- `get_diagnostics` - rust-analyzer's diagnostics for a file plus `cargo check`'s errors and warnings for it
- `workspace_symbols` - Search project symbols by name

### Code Generation (4 tools)
- `generate_struct` - Append a struct to a file. Fields are `{name, type, public?, doc?}` objects
- `generate_enum` - Append an enum to a file. Variants are names or `{name, fields?, doc?}` objects, where `fields` is a list of types (tuple variant) or of `{name, type}` objects (struct variant)
- `generate_trait_impl` - Append `impl Trait for Type` and have rust-analyzer stub out the trait's required items (the trait must be in scope in the file)
- `generate_tests` - Add `#[test]` functions to the file's `mod tests`, creating it if needed. Cases are `{name?, args?, expected?}`, where `args` and `expected` are Rust expressions; with no cases a failing `todo!()` test is added

### Refactoring (6 tools)
- `rename_symbol` - Rename across the workspace
- `extract_function` - Extract a selection into a new function with the given name
- `inline_function` - Inline the call at a position (or, on a definition, all of its callers)
- `change_signature` - Replace a function's declaration with `new_signature` and list the references that still use the old one. Call sites and trait implementations are not rewritten
- `organize_imports` - Remove unused imports, then sort them by formatting the file with rustfmt
- `format_code` - Format a file with rustfmt

### Quality Assurance (2 tools)
- `apply_clippy_suggestions` - Apply clippy's machine-applicable fixes to one file and list what was left
- `validate_lifetimes` - Report lifetime and borrow-checker errors and lints for one file (read-only)

### Project Management (2 tools)
- `analyze_manifest` - Summarize a `Cargo.toml`: packages, targets, features and dependencies
- `run_cargo_check` - Run `cargo check --all-targets` and return the compiler's diagnostics

### Advanced Features (4 tools)
- `get_type_hierarchy` - The declaration at a position and its implementations (a trait's implementors, or a type's impl blocks). rust-analyzer has no full supertype/subtype hierarchy
- `suggest_dependencies` - Search crates.io for a query (needs network access) and mark crates the workspace already depends on
- `create_module` - Create `<name>.rs` in a directory (or under a given parent module file) and add the `mod` declaration to the parent module
- `move_items` - Move top-level items, with their doc comments and attributes, from one file to another. Imports and paths that refer to them are not adjusted

## Prerequisites

- Rust toolchain (1.88+, the crate uses the 2024 edition and let chains), with `cargo` on `PATH`
- rust-analyzer installed (`rustup component add rust-analyzer`)
- rustfmt and clippy for the formatting and clippy tools (`rustup component add rustfmt clippy`)
- An MCP-compatible client (Claude, Roo, etc.)

## Installation

1. Clone this repository:
```bash
git clone <repository-url>
cd rust-mcp
```

2. Build the server:
```bash
cargo build --release
```

3. The server binary will be available at `target/release/rustmcp`

## Configuration

### Environment Variables

The server supports the following environment variables:

- `RUST_ANALYZER_PATH` - Path to rust-analyzer binary (default: `~/.cargo/bin/rust-analyzer`)

You can set this when running the server:
```bash
RUST_ANALYZER_PATH=/usr/local/bin/rust-analyzer ./target/release/rustmcp
```

Or set it in your MCP client configuration (see examples below).

### Claude Desktop

Add the following to your Claude Desktop MCP configuration file:

**macOS**: `~/Library/Application Support/Claude/claude_desktop_config.json`
**Windows**: `%APPDATA%\Claude\claude_desktop_config.json`

```json
{
  "mcpServers": {
    "rust-analyzer": {
      "command": "/path/to/rust-mcp/target/release/rustmcp",
      "args": [],
      "env": {
        "RUST_ANALYZER_PATH": "/custom/path/to/rust-analyzer"
      }
    }
  }
}
```

If rust-analyzer is in the default location (`~/.cargo/bin/rust-analyzer`), you can omit the `env` section:

```json
{
  "mcpServers": {
    "rust-analyzer": {
      "command": "/path/to/rust-mcp/target/release/rustmcp",
      "args": []
    }
  }
}
```

### Roo Configuration

Add to your Roo configuration file (typically `~/.roo/config.json`):

```json
{
  "mcp_servers": [
    {
      "name": "rust-analyzer",
      "command": "/path/to/rust-mcp/target/release/rustmcp",
      "args": [],
      "env": {
        "RUST_ANALYZER_PATH": "/custom/path/to/rust-analyzer"
      }
    }
  ]
}
```

For default rust-analyzer location, you can use an empty env object:

```json
{
  "mcp_servers": [
    {
      "name": "rust-analyzer",
      "command": "/path/to/rust-mcp/target/release/rustmcp",
      "args": [],
      "env": {}
    }
  ]
}
```

### Other MCP Clients

For any MCP-compatible client, configure it to run:
```bash
/path/to/rust-mcp/target/release/rustmcp
```

The server uses stdio transport and will be ready to accept MCP protocol messages.

## Usage Examples

Once configured, you can use the tools through your AI assistant. Here are some example prompts:

### Code Analysis
```
"Find all references to the `Config` struct in this Rust project"
"Show me the definition of the `parse_args` function"
"Check for compiler errors in src/main.rs"
"Search for all symbols matching 'user' in the workspace"
```

### Code Generation
```
"Generate a struct called `User` with fields: name (String), age (u32), email (String), with Debug and Clone derives"
"Create an enum called `HttpStatus` with variants: Ok, NotFound, ServerError"
"Generate unit tests for the `calculate_total` function"
"Generate a Display trait implementation for the User struct"
```

### Refactoring
```
"Rename the variable `data` to `user_input` throughout the codebase"
"Extract this code block into a separate function called `validate_input`"
"Inline the `helper_function` call on line 42"
"Organize all import statements in src/lib.rs"
"Format all the code in src/lib.rs"
```

### Quality Assurance
```
"Run clippy and apply all automatic fixes to improve code quality"
"Check for any lifetime or borrow checker issues in src/auth.rs"
```

### Project Management
```
"Analyze the Cargo.toml file and show dependency information"
"Run cargo check and report any compilation errors"
```

### Advanced Features
```
"Show me the type hierarchy for the symbol at line 15, character 8 in src/main.rs"
"Suggest crate dependencies for HTTP client functionality in this workspace"
"Create a new public module called 'auth' in src/auth.rs"
"Move the User struct and validate_user function from src/main.rs to src/user.rs"
"Change the signature of the process_data function to accept a reference instead of ownership"
```

## Architecture

The server is built with a modular architecture:

- **`src/main.rs`** - Entry point and server initialization
- **`src/lib.rs`** - Module declarations
- **`src/server/`** - MCP server implementation
  - `handler.rs` - Tool handlers and MCP server logic using rmcp crate
  - `parameters.rs` - Parameter type definitions for all tools
- **`src/analyzer/`** - rust-analyzer LSP client integration
  - `client.rs` - LSP client: transport, document sync and requests
  - `protocol.rs` - Request parameter builders and path/URI conversion
  - `edit.rs` - Applies LSP text and workspace edits to files on disk
- **`src/tools/`** - Modular tool implementations
  - `types.rs` - Tool dispatcher and definitions
  - `support.rs` - Shared helpers (argument parsing, result formatting, source scanning)
  - `analysis.rs` - Code analysis tools (find_definition, find_references, etc.)
  - `generation.rs` - Code generation tools (generate_struct, generate_enum, etc.)
  - `refactoring.rs` - Refactoring tools (rename_symbol, extract_function, etc.)
  - `formatting.rs` - Code formatting tools
  - `quality.rs` - Quality assurance tools (clippy, lifetimes)
  - `cargo.rs` - Project management tools
  - `navigation.rs` - Navigation tools (workspace_symbols)
  - `advanced.rs` - Advanced features (type hierarchy, dependencies, modules)

## Development

### Running in Development
```bash
cargo run
```

### Running the Tests
```bash
cargo test
```

Unit tests cover the edit, rendering and parsing logic. The tests in `tests/tools.rs` copy `tests/fixtures/sample_crate` to a temporary directory and run each tool against it with a real rust-analyzer and cargo; they are skipped if rust-analyzer is not installed.

### Testing Individual Tools
The server exposes all tools through the MCP protocol. For debugging, you can:

1. Run the server: `cargo run`
2. Send MCP messages via stdin (JSON-RPC format)
3. Check server logs and responses

### Adding New Tools

1. Create the tool implementation function in the appropriate `src/tools/*.rs` file
2. Add parameter struct to `src/server/parameters.rs`
3. Add the tool to the `execute_tool` match statement in `src/tools/types.rs`
4. Add tool definition to `get_tools()` function in `src/tools/types.rs`
5. Add the corresponding `#[tool]` method to `RustMcpServer` in `src/server/handler.rs`
6. Add an analyzer client method to `src/analyzer/client.rs` if the tool needs an LSP request that is not there yet
7. Add a test for it in `tests/tools.rs`

## Troubleshooting

### rust-analyzer Not Found
Ensure rust-analyzer is installed and accessible. The server will look for rust-analyzer in the following order:

1. The path specified by the `RUST_ANALYZER_PATH` environment variable
2. `$CARGO_HOME/bin`, then `~/.cargo/bin` (`%USERPROFILE%\.cargo\bin` on Windows)
3. `rust-analyzer` on `PATH`

To use a custom path, set the environment variable:
```bash
export RUST_ANALYZER_PATH=/custom/path/to/rust-analyzer
```

Or configure it in your MCP client configuration (see Configuration section above).

### MCP Connection Issues
- Verify the server binary path in your MCP client configuration
- Check that the binary has execute permissions: `chmod +x target/release/rustmcp`
- Ensure no other processes are using the same MCP server name

### LSP Communication Errors
- Verify rust-analyzer works independently: `rust-analyzer --version`
- Check that your Rust project has a valid `Cargo.toml`
- Ensure the workspace path is correct when calling tools
- rust-analyzer analyzes the workspace in the directory the server was started in; start the server from your project root

## Contributing

1. Fork the repository
2. Create a feature branch
3. Implement your changes with tests
4. Submit a pull request

