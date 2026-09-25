use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use harness_core::{Id, SessionId};
use harness_policy::{AllowAllPolicy, DenyAllPolicy};
use harness_session::{EventBus, EventType};
use harness_tools::{ToolContext, ToolRegistry, ToolRequest};
use serde_json::json;
use tempfile::tempdir;

fn request(name: &str, arguments: serde_json::Value) -> ToolRequest {
    ToolRequest::new(name, arguments)
}

fn execute(
    registry: &ToolRegistry,
    workspace: impl AsRef<Path>,
    name: &str,
    arguments: serde_json::Value,
) -> Result<harness_tools::ToolResult, harness_core::Error> {
    let context = ToolContext {
        policy: &AllowAllPolicy,
        working_directory: workspace.as_ref(),
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    registry.execute(&context, request(name, arguments))
}

#[test]
fn reads_writes_patches_lists_globs_and_greps() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    fs::create_dir_all(workspace.join("src")).unwrap();
    fs::write(workspace.join("src/main.rs"), "fn main() {}\n").unwrap();
    let registry = ToolRegistry::with_workspace_tools();

    let read = execute(
        &registry,
        workspace,
        "read_file",
        json!({ "path": "src/main.rs" }),
    )
    .unwrap();
    assert_eq!(read.output, "fn main() {}\n");
    assert_eq!(read.metadata["path"], "src/main.rs");

    let write = execute(
        &registry,
        workspace,
        "write_file",
        json!({ "path": "notes.txt", "content": "alpha\nbeta\n" }),
    )
    .unwrap();
    assert_eq!(write.output, "wrote notes.txt");
    assert_eq!(write.changed_files.len(), 1);

    let patch = execute(
        &registry,
        workspace,
        "apply_patch",
        json!({ "path": "notes.txt", "old_text": "alpha", "new_text": "gamma" }),
    )
    .unwrap();
    assert_eq!(patch.output, "patched notes.txt");
    assert_eq!(
        fs::read_to_string(workspace.join("notes.txt")).unwrap(),
        "gamma\nbeta\n"
    );

    let listed = execute(
        &registry,
        workspace,
        "list_directory",
        json!({ "path": "." }),
    )
    .unwrap();
    assert!(listed.output.contains("notes.txt"));
    assert!(listed.output.contains("src"));

    let globbed = execute(
        &registry,
        workspace,
        "glob",
        json!({ "pattern": "src/**/*.rs" }),
    )
    .unwrap();
    assert_eq!(globbed.output, "src/main.rs");

    let grepped = execute(
        &registry,
        workspace,
        "grep",
        json!({ "pattern": "gamma", "path": "." }),
    )
    .unwrap();
    assert_eq!(grepped.output, "notes.txt:1:gamma");
}

#[test]
fn rejects_patch_conflicts_missing_files_and_path_traversal() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(workspace.join("file.txt"), "one\ntwo\n").unwrap();
    fs::write(temporary.path().join("outside.txt"), "secret").unwrap();
    let workspace = workspace.as_path();
    let registry = ToolRegistry::with_workspace_tools();

    let conflict = execute(
        &registry,
        workspace,
        "apply_patch",
        json!({ "path": "file.txt", "old_text": "missing", "new_text": "value" }),
    )
    .unwrap_err();
    assert!(conflict.to_string().contains("patch context did not match"));

    let missing = execute(
        &registry,
        workspace,
        "read_file",
        json!({ "path": "missing.txt" }),
    )
    .unwrap_err();
    assert!(missing.to_string().contains("path does not exist"));

    for name in [
        "read_file",
        "write_file",
        "apply_patch",
        "list_directory",
        "glob",
        "grep",
    ] {
        let arguments = match name {
            "read_file" | "list_directory" => json!({ "path": "../outside.txt" }),
            "write_file" => json!({ "path": "../outside.txt", "content": "changed" }),
            "apply_patch" => json!({
                "path": "../outside.txt",
                "old_text": "secret",
                "new_text": "changed"
            }),
            "glob" => json!({ "pattern": "*", "path": "../" }),
            _ => json!({ "pattern": "secret", "path": "../" }),
        };
        let error = execute(&registry, workspace, name, arguments).unwrap_err();
        assert!(
            error.to_string().contains("outside the workspace"),
            "{name}: {error}"
        );
    }
}

#[test]
fn rejects_binary_and_large_files() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    fs::write(workspace.join("binary.dat"), [0, 159, 146, 150]).unwrap();
    fs::write(workspace.join("large.txt"), vec![b'a'; 1_048_577]).unwrap();
    let registry = ToolRegistry::with_workspace_tools();

    let binary = execute(
        &registry,
        workspace,
        "read_file",
        json!({ "path": "binary.dat" }),
    )
    .unwrap_err();
    assert!(binary.to_string().contains("binary"));

    let large = execute(
        &registry,
        workspace,
        "read_file",
        json!({ "path": "large.txt" }),
    )
    .unwrap_err();
    assert!(large.to_string().contains("exceeds"));
}

#[test]
fn emits_tool_lifecycle_events_through_the_common_registry() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    fs::write(workspace.join("file.txt"), "content").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        captured.lock().unwrap().push(event.event_type);
    }));
    let session_id = SessionId::new("session-1").unwrap();
    let context = ToolContext {
        policy: &AllowAllPolicy,
        working_directory: workspace,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: Some(&Id::new("correlation-1").unwrap()),
    };

    registry
        .execute(
            &context,
            request("read_file", json!({ "path": "file.txt" })),
        )
        .unwrap();

    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[
            EventType::ToolRequested,
            EventType::ToolApproved,
            EventType::ToolStarted,
            EventType::ToolOutput,
            EventType::ToolCompleted,
        ]
    );
}

#[test]
fn emits_file_change_lifecycle_events() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let registry = ToolRegistry::with_workspace_tools();
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        captured.lock().unwrap().push(event.event_type);
    }));
    let session_id = SessionId::new("session-2").unwrap();
    let context = ToolContext {
        policy: &AllowAllPolicy,
        working_directory: workspace,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };
    registry
        .execute(
            &context,
            request("write_file", json!({ "path": "new.txt", "content": "new" })),
        )
        .unwrap();
    assert!(events.lock().unwrap().contains(&EventType::FileChanged));
    assert!(events.lock().unwrap().contains(&EventType::ToolCompleted));
    assert_eq!(
        registry.names(),
        vec![
            "read_file",
            "write_file",
            "apply_patch",
            "list_directory",
            "glob",
            "grep"
        ]
    );
}

#[test]
fn emits_denied_event_without_executing_a_tool() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let registry = ToolRegistry::with_workspace_tools();
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        captured.lock().unwrap().push(event.event_type);
    }));
    let session_id = SessionId::new("session-3").unwrap();
    let context = ToolContext {
        policy: &DenyAllPolicy,
        working_directory: workspace,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };

    let result = registry.execute(
        &context,
        request(
            "write_file",
            json!({ "path": "blocked.txt", "content": "blocked" }),
        ),
    );

    assert!(result.is_err());
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[EventType::ToolRequested, EventType::ToolDenied]
    );
    assert!(!workspace.join("blocked.txt").exists());
}
