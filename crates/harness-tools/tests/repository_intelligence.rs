use std::fs;
use std::path::Path;

use harness_core::Error;
use harness_policy::{AllowAllPolicy, DenyAllPolicy};
use harness_tools::{ToolContext, ToolRegistry, ToolRequest};
use serde_json::json;
use tempfile::tempdir;

fn execute(
    registry: &ToolRegistry,
    workspace: &Path,
    name: &str,
    arguments: serde_json::Value,
) -> Result<harness_tools::ToolResult, Error> {
    let context = ToolContext {
        policy: &AllowAllPolicy,
        working_directory: workspace,
        cancellation: None,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    registry.execute(&context, ToolRequest::new(name, arguments))
}

#[test]
fn standard_registry_exposes_bounded_repository_queries() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::create_dir_all(root.join("tests")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"index-fixture\"\nversion = \"0.1.0\"\n",
    )
    .unwrap();
    fs::write(
        root.join("src/lib.rs"),
        "use std::path::Path;\npub struct Engine;\npub fn locate() -> Path { todo!() }\n",
    )
    .unwrap();
    fs::write(root.join("tests/engine_test.rs"), "fn engine_test() {}\n").unwrap();
    let registry = ToolRegistry::with_workspace_tools();

    let names = registry.names();
    for name in [
        "search_files",
        "search_text",
        "find_symbol",
        "find_references",
        "goto_definition",
        "get_diagnostics",
        "get_file_outline",
        "get_repo_tree",
    ] {
        assert!(
            names.iter().any(|registered| registered == name),
            "missing {name}"
        );
    }

    let symbols = execute(&registry, root, "find_symbol", json!({"name":"locate"})).unwrap();
    assert!(symbols
        .output
        .contains("locate [function] at src/lib.rs:3 (exported)"));
    let definition = execute(&registry, root, "goto_definition", json!({"name":"Engine"})).unwrap();
    assert!(definition.output.contains("src/lib.rs:2"));
    let references = execute(&registry, root, "find_references", json!({"name":"Path"})).unwrap();
    assert!(
        references.output.contains("src/lib.rs:1"),
        "unexpected references: {}",
        references.output
    );
    let outline = execute(
        &registry,
        root,
        "get_file_outline",
        json!({"path":"src/lib.rs"}),
    )
    .unwrap();
    assert!(outline.output.contains("imports: std::path::Path"));
    assert!(outline.output.contains("symbols:"));
    let tree = execute(
        &registry,
        root,
        "get_repo_tree",
        json!({"path":"src","depth":2}),
    )
    .unwrap();
    assert!(tree.output.contains("src/lib.rs"));
    let files = execute(
        &registry,
        root,
        "search_files",
        json!({"query":"engine_test"}),
    )
    .unwrap();
    assert!(files.output.contains("tests/engine_test.rs"));
    let text = execute(
        &registry,
        root,
        "search_text",
        json!({"query":"todo!", "glob":"*.rs"}),
    )
    .unwrap();
    assert!(text.output.contains("src/lib.rs:3"));
    assert!(text.output.len() < 24 * 1024);
    let diagnostics = execute(&registry, root, "get_diagnostics", json!({})).unwrap();
    assert!(diagnostics.output.contains("needs a source file path"));
}

#[test]
fn successful_edits_update_cached_symbols_before_the_next_query() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn before() {}\n").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    assert!(
        execute(&registry, root, "find_symbol", json!({"name":"before"}))
            .unwrap()
            .output
            .contains("before")
    );
    assert!(registry.repository_map(root).unwrap().contains("before"));

    execute(
        &registry,
        root,
        "write_file",
        json!({"path":"src/lib.rs", "content":"pub fn after() {}\n"}),
    )
    .unwrap();
    let updated = execute(&registry, root, "find_symbol", json!({"name":"after"})).unwrap();
    assert!(updated.output.contains("after [function]"));
    let removed = execute(&registry, root, "find_symbol", json!({"name":"before"})).unwrap();
    assert!(removed.output.is_empty());
    let updated_map = registry.repository_map(root).unwrap();
    assert!(updated_map.contains("after"));
    assert!(!updated_map.contains("before"));
}

#[test]
fn repository_queries_obey_read_search_policy_and_workspace_boundaries() {
    let temporary = tempdir().unwrap();
    let root = temporary.path().join("repo");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("main.py"), "def task(): pass\n").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let context = ToolContext {
        policy: &DenyAllPolicy,
        working_directory: &root,
        cancellation: None,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let denied = registry.execute(
        &context,
        ToolRequest::new("find_symbol", json!({"name":"task"})),
    );
    assert!(matches!(denied, Err(Error::PermissionDenied { .. })));

    let escaped = execute(
        &ToolRegistry::with_workspace_tools(),
        &root,
        "get_file_outline",
        json!({"path":"../outside.py"}),
    );
    assert!(matches!(escaped, Err(Error::Tool { .. })));
}

#[test]
fn repository_summary_stays_compact_for_initial_context() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("package.json"), r#"{"name":"sample-app"}"#).unwrap();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/main.ts"), "export function main() {}\n").unwrap();
    let repo_map = harness_tools::RepositoryIndex::build(root)
        .unwrap()
        .repo_map();
    assert!(repo_map.contains("Top-level: package.json, src"));
    assert!(repo_map.contains("sample-app"));
    assert!(repo_map.contains("main"));
    assert!(repo_map.len() < 4_000);
}
