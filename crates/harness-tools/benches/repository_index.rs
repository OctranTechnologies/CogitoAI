use std::fs;
use std::hint::black_box;
use std::time::{Duration, Instant};

use harness_policy::AllowAllPolicy;
use harness_tools::{RepositoryIndex, ToolContext, ToolRegistry, ToolRequest};
use serde_json::json;
use tempfile::tempdir;

fn main() {
    let directory = tempdir().expect("benchmark fixture");
    let root = directory.path();
    fs::create_dir_all(root.join("src")).expect("source directory");
    fs::create_dir_all(root.join("tests")).expect("test directory");
    for index in 0..10_000 {
        let source = format!(
            "pub fn module_{index}() -> usize {{\n    let needle_value = {index};\n    needle_value\n}}\n"
        );
        fs::write(root.join(format!("src/module_{index}.rs")), source).expect("source file");
    }
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"repo-bench\"\n",
    )
    .expect("manifest");
    fs::write(
        root.join("tests/workspace_test.rs"),
        "fn workspace_test() {}\n",
    )
    .expect("test file");

    let (index_build, mut index) = timed(3, || RepositoryIndex::build(root).expect("index build"));
    let (symbol_search, _) = timed(1_000, || {
        black_box(index.find_symbol("module_1024", 20));
    });

    let registry = ToolRegistry::with_workspace_tools();
    let policy = AllowAllPolicy;
    let context = ToolContext {
        policy: &policy,
        working_directory: root,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let (text_search, _) = timed(50, || {
        black_box(
            registry
                .execute(
                    &context,
                    ToolRequest::new(
                        "search_text",
                        json!({"query":"needle_value", "path":"src", "max_results":20}),
                    ),
                )
                .expect("text search"),
        );
    });

    let (incremental_update, _) = timed(200, || {
        let path = root.join("src/module_1024.rs");
        fs::write(&path, "pub fn module_1024() -> usize { 1024 }\n").expect("update fixture");
        index.update_file(&path).expect("incremental index update");
        black_box(index.indexed_bytes());
    });

    println!("RepositoryIndex benchmark (10,002 files)");
    println!(
        "  startup average:       {} ms",
        index_build.as_millis() / 3
    );
    println!(
        "  symbol lookup average: {} µs",
        symbol_search.as_micros() / 1_000
    );
    println!(
        "  ripgrep search average: {} ms",
        text_search.as_millis() / 50
    );
    println!(
        "  incremental file write average: {} µs",
        incremental_update.as_micros() / 200
    );
    println!("  indexed source bytes:  {}", index.indexed_bytes());
}

fn timed<T>(iterations: usize, mut operation: impl FnMut() -> T) -> (Duration, T) {
    let started = Instant::now();
    let mut result = None;
    for _ in 0..iterations {
        result = Some(operation());
    }
    (started.elapsed(), result.expect("nonzero iterations"))
}
