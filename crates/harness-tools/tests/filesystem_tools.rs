use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};

use harness_core::{Id, SessionId};
use harness_policy::{AllowAllPolicy, DenyAllPolicy, ExecutionMode, PolicyEngine};
use harness_session::{EventBus, EventType};
use harness_tools::{ToolContext, ToolRegistry, ToolRequest};
use serde_json::json;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap()
}

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
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    registry.execute(&context, request(name, arguments))
}

#[test]
fn reads_writes_patches_lists_globs_and_greps() {
    let temporary = tempdir();
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
    assert!(write.output.starts_with("wrote notes.txt\n--- /dev/null"));
    assert_eq!(write.metadata["conflict_status"], "none");
    assert_eq!(write.metadata["insertions"], 2);
    assert_eq!(write.changed_files.len(), 1);

    let patch = execute(
        &registry,
        workspace,
        "apply_patch",
        json!({ "path": "notes.txt", "old_text": "alpha", "new_text": "gamma" }),
    )
    .unwrap();
    assert!(patch
        .output
        .starts_with("patched notes.txt\n--- a/notes.txt"));
    assert_eq!(patch.metadata["conflict_status"], "none");
    assert_eq!(patch.metadata["insertions"], 1);
    assert_eq!(patch.metadata["deletions"], 1);
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
    let temporary = tempdir();
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
    .unwrap();
    assert!(conflict.is_error);
    assert_eq!(conflict.metadata["conflict_status"], "stale");

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
    let temporary = tempdir();
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
fn records_tool_arguments_as_the_model_wrote_them() {
    // The event log is persisted and read back by every client, so a string
    // argument must be stored as the string itself. Serialising the JSON value
    // would store its encoding instead, and readers would show literal quotes.
    let temporary = tempdir();
    let workspace = temporary.path();
    fs::write(workspace.join("file.txt"), "content").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let bus = EventBus::new();
    let requested = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&requested);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        if let harness_session::EventPayload::ToolRequested { tool, arguments } = &event.payload {
            if tool == "read_file" {
                *captured.lock().unwrap() = Some(arguments.clone());
            }
        }
    }));
    let session_id = SessionId::new("session-arguments").unwrap();
    let context = ToolContext {
        policy: &AllowAllPolicy,
        working_directory: workspace,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };

    registry
        .execute(
            &context,
            request("read_file", json!({ "path": "file.txt" })),
        )
        .unwrap();

    let arguments = requested
        .lock()
        .unwrap()
        .clone()
        .expect("a tool.requested event should have been emitted");
    assert_eq!(arguments.get("path").map(String::as_str), Some("file.txt"));
}

#[test]
fn records_non_string_tool_arguments_as_json() {
    // Numbers, booleans, and objects have no plain-string form, so they keep
    // their JSON representation rather than being flattened.
    let temporary = tempdir();
    let workspace = temporary.path();
    fs::write(workspace.join("file.txt"), "content").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let bus = EventBus::new();
    let requested = Arc::new(Mutex::new(None));
    let captured = Arc::clone(&requested);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        if let harness_session::EventPayload::ToolRequested { tool, arguments } = &event.payload {
            if tool == "read_file" {
                *captured.lock().unwrap() = Some(arguments.clone());
            }
        }
    }));
    let session_id = SessionId::new("session-arguments-non-string").unwrap();
    let context = ToolContext {
        policy: &AllowAllPolicy,
        working_directory: workspace,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };

    registry
        .execute(
            &context,
            request(
                "read_file",
                json!({ "path": "file.txt", "line": 12, "whole": true }),
            ),
        )
        .unwrap();

    let arguments = requested.lock().unwrap().clone().unwrap();
    assert_eq!(arguments.get("line").map(String::as_str), Some("12"));
    assert_eq!(arguments.get("whole").map(String::as_str), Some("true"));
    assert_eq!(arguments.get("path").map(String::as_str), Some("file.txt"));
}

#[test]
fn emits_tool_lifecycle_events_through_the_common_registry() {
    let temporary = tempdir();
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
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
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
            EventType::PolicyDecision,
            EventType::ToolApproved,
            EventType::ToolStarted,
            EventType::ToolOutput,
            EventType::ToolCompleted,
        ]
    );
}

#[test]
fn emits_file_change_lifecycle_events() {
    let temporary = tempdir();
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
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
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
            "create_file",
            "write_file",
            "apply_patch",
            "replace_text",
            "replace_range",
            "delete_file",
            "rename_file",
            "list_directory",
            "glob",
            "grep",
            "shell",
            "run_command",
            "start_background_command",
            "read_process_output",
            "list_processes",
            "stop_process",
            "wait_for_process_output",
            "search_files",
            "search_text",
            "find_symbol",
            "find_references",
            "goto_definition",
            "get_diagnostics",
            "get_file_outline",
            "get_repo_tree",
            "get_instructions",
            "list_skills",
            "load_skill",
            "web_search",
            "web_fetch"
        ]
    );
}

#[test]
fn emits_denied_event_without_executing_a_tool() {
    let temporary = tempdir();
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
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
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
        &[
            EventType::ToolRequested,
            EventType::PolicyDecision,
            EventType::ToolDenied,
        ]
    );
    assert!(!workspace.join("blocked.txt").exists());
}

#[test]
fn safe_mode_asks_before_mutation_and_auto_mode_denies_secret_paths() {
    let temporary = tempdir();
    let workspace = temporary.path();
    fs::write(workspace.join(".env"), "SECRET=value").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        captured.lock().unwrap().push(event.event_type);
    }));
    let session_id = SessionId::new("policy-session").unwrap();
    let safe = PolicyEngine::new(ExecutionMode::Safe, workspace);
    let context = ToolContext {
        policy: &safe,
        working_directory: workspace,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };
    let result = registry.execute(
        &context,
        request("write_file", json!({ "path": "new.txt", "content": "new" })),
    );
    assert!(matches!(
        result,
        Err(harness_core::Error::PermissionRequired { .. })
    ));
    assert!(!workspace.join("new.txt").exists());
    assert_eq!(
        events.lock().unwrap().as_slice(),
        &[EventType::ToolRequested, EventType::PolicyDecision]
    );

    let auto = PolicyEngine::new(ExecutionMode::Auto, workspace);
    let context = ToolContext {
        policy: &auto,
        working_directory: workspace,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };
    let result = registry.execute(&context, request("read_file", json!({ "path": ".env" })));
    assert!(matches!(
        result,
        Err(harness_core::Error::PermissionDenied { .. })
    ));
}

#[test]
fn every_mutable_tool_is_checked_by_policy() {
    let temporary = tempdir();
    let workspace = temporary.path();
    fs::write(workspace.join("file.txt"), "before").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let context = ToolContext {
        policy: &DenyAllPolicy,
        working_directory: workspace,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let requests = [
        request(
            "write_file",
            json!({ "path": "file.txt", "content": "after" }),
        ),
        request(
            "apply_patch",
            json!({ "path": "file.txt", "old_text": "before", "new_text": "after" }),
        ),
        request("create_file", json!({"path":"new.txt","content":""})),
        request(
            "replace_text",
            json!({"path":"file.txt","old_text":"before","new_text":"after"}),
        ),
        request(
            "replace_range",
            json!({"path":"file.txt","start_line":1,"end_line":1,"expected_text":"before","new_text":"after"}),
        ),
        request("delete_file", json!({"path":"file.txt"})),
        request(
            "rename_file",
            json!({"source_path":"file.txt","destination_path":"other.txt"}),
        ),
        request("shell", json!({ "command": "echo denied" })),
    ];

    for request in requests {
        assert!(matches!(
            registry.execute(&context, request),
            Err(harness_core::Error::PermissionDenied { .. })
        ));
    }
    assert_eq!(
        fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "before"
    );
}

#[test]
fn detects_a_stale_revision_even_when_the_patch_text_is_still_present() {
    let temporary = tempdir();
    let root = temporary.path();
    fs::write(root.join("source.txt"), "keep this line\nold value\n").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    execute(&registry, root, "read_file", json!({"path":"source.txt"})).unwrap();
    fs::write(
        root.join("source.txt"),
        "keep this line\nold value\nexternal update\n",
    )
    .unwrap();

    let result = execute(
        &registry,
        root,
        "apply_patch",
        json!({"path":"source.txt","old_text":"old value","new_text":"new value"}),
    )
    .unwrap();
    assert!(result.is_error);
    assert_eq!(result.metadata["conflict_status"], "stale");
    assert_eq!(
        fs::read_to_string(root.join("source.txt")).unwrap(),
        "keep this line\nold value\nexternal update\n"
    );
}

#[test]
fn sequential_edits_refresh_the_session_revision() {
    let temporary = tempdir();
    let root = temporary.path();
    fs::write(root.join("source.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    execute(&registry, root, "read_file", json!({"path":"source.rs"})).unwrap();
    let broken = execute(
        &registry,
        root,
        "write_file",
        json!({"path":"source.rs","content":"BROKEN pub fn value() -> u32 { 2 }\n"}),
    )
    .unwrap();
    assert!(!broken.is_error, "{broken:?}");
    let fixed = execute(
        &registry,
        root,
        "write_file",
        json!({"path":"source.rs","content":"pub fn value() -> u32 { 2 }\n"}),
    )
    .unwrap();
    assert!(!fixed.is_error, "{fixed:?}");
    assert_eq!(
        fs::read_to_string(root.join("source.rs")).unwrap(),
        "pub fn value() -> u32 { 2 }\n"
    );
}

#[test]
fn rejects_ambiguous_text_and_supports_crlf_and_unicode_edits() {
    let temporary = tempdir();
    let root = temporary.path();
    let registry = ToolRegistry::with_workspace_tools();
    fs::write(root.join("ambiguous.txt"), "same\nsame\n").unwrap();
    let ambiguous = execute(
        &registry,
        root,
        "replace_text",
        json!({"path":"ambiguous.txt","old_text":"same","new_text":"unique"}),
    )
    .unwrap();
    assert!(ambiguous.is_error);
    assert_eq!(ambiguous.metadata["conflict_status"], "ambiguous");

    fs::write(root.join("windows.txt"), "before\r\n雪と café\r\nafter\r\n").unwrap();
    let changed = execute(
        &registry,
        root,
        "apply_patch",
        json!({"path":"windows.txt","old_text":"雪と café\nafter\n","new_text":"雪と 東京\nnext\n"}),
    )
    .unwrap();
    assert!(!changed.is_error);
    let bytes = fs::read(root.join("windows.txt")).unwrap();
    assert_eq!(
        String::from_utf8(bytes.clone()).unwrap(),
        "before\r\n雪と 東京\r\nnext\r\n"
    );
    assert!(!bytes
        .windows(2)
        .any(|pair| pair[1] == b'\n' && pair[0] != b'\r'));
}

#[test]
fn replaces_a_verified_line_range_and_preserves_utf8_bom_and_crlf() {
    let temporary = tempdir();
    let root = temporary.path();
    let registry = ToolRegistry::with_workspace_tools();
    fs::write(
        root.join("range.txt"),
        b"\xEF\xBB\xBFfirst\r\nsecond\r\nthird\r\n",
    )
    .unwrap();
    let read = execute(
        &registry,
        root,
        "read_file",
        json!({"path":"range.txt","start_line":2,"end_line":2}),
    )
    .unwrap();
    assert_eq!(read.output, "second\r\n");
    assert!(read.metadata["revision"]
        .as_str()
        .unwrap()
        .starts_with("sha256:"));

    let changed = execute(
        &registry,
        root,
        "replace_range",
        json!({"path":"range.txt","start_line":2,"end_line":2,"expected_text":"second\n","new_text":"updated\n"}),
    )
    .unwrap();
    assert!(!changed.is_error);
    assert_eq!(
        fs::read(root.join("range.txt")).unwrap(),
        b"\xEF\xBB\xBFfirst\r\nupdated\r\nthird\r\n"
    );
}

#[test]
fn creates_empty_files_deletes_and_renames_with_structured_results() {
    let temporary = tempdir();
    let root = temporary.path();
    let registry = ToolRegistry::with_workspace_tools();
    let empty = execute(
        &registry,
        root,
        "create_file",
        json!({"path":"empty.txt","content":""}),
    )
    .unwrap();
    assert!(!empty.is_error);
    assert_eq!(fs::read(root.join("empty.txt")).unwrap(), b"");
    assert_eq!(empty.metadata["insertions"], 0);

    execute(
        &registry,
        root,
        "create_file",
        json!({"path":"old.txt","content":"hello\n"}),
    )
    .unwrap();
    let moved = execute(
        &registry,
        root,
        "rename_file",
        json!({"source_path":"old.txt","destination_path":"nested/new.txt"}),
    )
    .unwrap();
    assert!(!moved.is_error);
    assert!(!root.join("old.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("nested/new.txt")).unwrap(),
        "hello\n"
    );

    let deleted = execute(
        &registry,
        root,
        "delete_file",
        json!({"path":"nested/new.txt"}),
    )
    .unwrap();
    assert!(!deleted.is_error);
    assert_eq!(deleted.metadata["deletions"], 1);
    assert!(!root.join("nested/new.txt").exists());
}

#[test]
fn rejects_binary_mutation_and_keeps_large_edit_results_bounded() {
    let temporary = tempdir();
    let root = temporary.path();
    let registry = ToolRegistry::with_workspace_tools();
    fs::write(root.join("binary.bin"), [0, 1, 2, 3]).unwrap();
    let binary = execute(&registry, root, "delete_file", json!({"path":"binary.bin"})).unwrap_err();
    assert!(binary.to_string().contains("binary"));

    let content = format!("{}needle{}", "a".repeat(1_500_000), "z".repeat(1_500_000));
    let created = execute(
        &registry,
        root,
        "create_file",
        json!({"path":"large.txt","content":content}),
    )
    .unwrap();
    assert!(created.truncated);
    assert!(created.output.len() < 20_000);
    assert_eq!(
        fs::metadata(root.join("large.txt")).unwrap().len(),
        3_000_006
    );
}

#[test]
fn large_files_can_be_read_and_edited_by_bounded_line_range() {
    let temporary = tempdir();
    let root = temporary.path();
    let registry = ToolRegistry::with_workspace_tools();
    let contents = (0..40_000)
        .map(|line| format!("line {line:05} payload\r\n"))
        .collect::<String>();
    fs::write(root.join("many-lines.txt"), contents).unwrap();
    let read = execute(
        &registry,
        root,
        "read_file",
        json!({"path":"many-lines.txt","start_line":20_000,"end_line":20_001}),
    )
    .unwrap();
    assert_eq!(read.output, "line 19999 payload\r\nline 20000 payload\r\n");
    assert!(read.metadata["revision"].as_str().is_some());

    let changed = execute(
        &registry,
        root,
        "replace_range",
        json!({"path":"many-lines.txt","start_line":20_000,"end_line":20_000,"expected_text":"line 19999 payload\n","new_text":"updated payload\n"}),
    )
    .unwrap();
    assert!(!changed.is_error);
    let updated = fs::read_to_string(root.join("many-lines.txt")).unwrap();
    assert!(updated.contains("updated payload\r\nline 20000 payload\r\n"));
}

#[test]
fn file_mutation_tools_reject_workspace_traversal() {
    let temporary = tempdir();
    let workspace = temporary.path().join("workspace");
    fs::create_dir_all(&workspace).unwrap();
    fs::write(temporary.path().join("outside.txt"), "outside").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let cases = [
        (
            "create_file",
            json!({"path":"../outside.txt","content":"x"}),
        ),
        (
            "replace_range",
            json!({"path":"../outside.txt","start_line":1,"end_line":1,"expected_text":"outside","new_text":"changed"}),
        ),
        ("delete_file", json!({"path":"../outside.txt"})),
        (
            "rename_file",
            json!({"source_path":"../outside.txt","destination_path":"inside.txt"}),
        ),
    ];
    for (name, arguments) in cases {
        let error = execute(&registry, &workspace, name, arguments).unwrap_err();
        assert!(
            error.to_string().contains("outside the workspace"),
            "{name}: {error}"
        );
    }
    assert_eq!(
        fs::read_to_string(temporary.path().join("outside.txt")).unwrap(),
        "outside"
    );
}

#[test]
fn rename_checks_policy_for_both_source_and_destination() {
    let temporary = tempdir();
    let root = temporary.path();
    fs::write(root.join("source.txt"), "safe\n").unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let policy = PolicyEngine::new(ExecutionMode::Auto, root);
    let context = ToolContext {
        policy: &policy,
        working_directory: root,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let result = registry.execute(
        &context,
        request(
            "rename_file",
            json!({"source_path":"source.txt","destination_path":".env"}),
        ),
    );
    assert!(matches!(
        result,
        Err(harness_core::Error::PermissionDenied { .. })
    ));
    assert!(root.join("source.txt").exists());
    assert!(!root.join(".env").exists());
}
