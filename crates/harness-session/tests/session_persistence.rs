use std::collections::BTreeMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::sync::{Arc, Mutex};

use harness_core::{Error, Id};
use harness_session::{
    CompactState, EventBus, EventPayload, EventType, FileChange, HarnessEvent, JsonlSessionStore,
    MessageRole, Session, SessionState, SessionStatus, SessionStore,
};
use tempfile::tempdir;

fn append(store: &JsonlSessionStore, session: &Session, payload: EventPayload) {
    let event = HarnessEvent::new(session.id.clone(), payload, None, None);
    store.append_event(&session.id, event).unwrap();
}

fn compacted_state(task: &str) -> CompactState {
    CompactState {
        task: task.to_owned(),
        current_approach: "continue implementation".to_owned(),
        discoveries: vec!["workspace uses Rust".to_owned()],
        important_files: vec!["src/lib.rs".to_owned()],
        files_modified: vec!["src/lib.rs".to_owned()],
        decisions: vec!["use bounded context".to_owned()],
        failed_attempts: vec![],
        test_status: vec!["cargo test passed".to_owned()],
        remaining_work: vec!["resume verification".to_owned()],
    }
}

#[test]
fn persists_reloads_resumes_and_reconstructs_a_mock_session() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = JsonlSessionStore::new(temporary.path().join("sessions")).unwrap();
    let session = store.create(&workspace).unwrap();
    let observed = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&observed);
    let _subscription = store
        .event_bus()
        .subscribe(Arc::new(move |event: &HarnessEvent| {
            captured.lock().unwrap().push(event.event_id.to_string());
        }));

    append(
        &store,
        &session,
        EventPayload::UserMessage {
            text: "inspect this project".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ModelRequested {
            provider: "local".to_owned(),
            model: "test".to_owned(),
            prompt_tokens: Some(10),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ModelResponse {
            provider: "local".to_owned(),
            model: "test".to_owned(),
            text: "I will inspect it.".to_owned(),
            input_tokens: Some(10),
            output_tokens: Some(5),
        },
    );
    append(
        &store,
        &session,
        EventPayload::AssistantMessage {
            text: "I will inspect it.".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ToolRequested {
            tool: "read_file".to_owned(),
            arguments: BTreeMap::new(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ToolApproved {
            tool: "read_file".to_owned(),
            reason: Some("workspace read".to_owned()),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ToolStarted {
            tool: "read_file".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ToolOutput {
            tool: "read_file".to_owned(),
            output: "file contents".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ToolCompleted {
            tool: "read_file".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::FileChanged {
            path: workspace.join("README.md"),
            change: FileChange::Modified,
        },
    );
    append(
        &store,
        &session,
        EventPayload::CheckpointCreated {
            checkpoint_id: Id::new("checkpoint-1").unwrap(),
            reference: "HEAD".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::VerificationStarted {
            commands: vec!["cargo test".to_owned()],
        },
    );
    append(
        &store,
        &session,
        EventPayload::VerificationResult {
            command: "cargo test".to_owned(),
            category: "GeneralTest".to_owned(),
            duration_ms: 12,
            passed: true,
            exit_code: Some(0),
            output: "ok".to_owned(),
            diagnostics: Vec::new(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ContextCompacted {
            removed_items: 2,
            summary: "kept relevant context".to_owned(),
            state: compacted_state("Keep session state available"),
        },
    );
    append(
        &store,
        &session,
        EventPayload::SessionCompleted { reason: None },
    );

    let report = store.load_with_report(&session.id).unwrap();
    assert!(report.warnings.is_empty());
    assert_eq!(
        report
            .session
            .events
            .iter()
            .map(|event| event.event_type)
            .collect::<Vec<_>>(),
        vec![
            EventType::SessionStarted,
            EventType::UserMessage,
            EventType::ModelRequested,
            EventType::ModelResponse,
            EventType::AssistantMessage,
            EventType::ToolRequested,
            EventType::ToolApproved,
            EventType::ToolStarted,
            EventType::ToolOutput,
            EventType::ToolCompleted,
            EventType::FileChanged,
            EventType::CheckpointCreated,
            EventType::VerificationStarted,
            EventType::VerificationResult,
            EventType::ContextCompacted,
            EventType::SessionCompleted,
        ]
    );
    assert_eq!(
        report.session.state().unwrap().status,
        SessionStatus::Completed
    );
    assert_eq!(
        report.session.state().unwrap().messages[0].role,
        MessageRole::User
    );
    let resumed = store.resume(&session.id).unwrap();
    assert_eq!(resumed.events.len(), 17);
    assert_eq!(resumed.state().unwrap().status, SessionStatus::Active);
    assert_eq!(store.recent(10).unwrap()[0].id, session.id);
    assert_eq!(observed.lock().unwrap().len(), 16);
    assert_eq!(
        serde_json::from_str::<HarnessEvent>(
            &serde_json::to_string(&report.session.events[1]).unwrap()
        )
        .unwrap(),
        report.session.events[1]
    );
}

#[test]
fn repeated_compaction_preserves_history_and_latest_state() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = JsonlSessionStore::new(temporary.path().join("sessions")).unwrap();
    let session = store.create(&workspace).unwrap();
    append(
        &store,
        &session,
        EventPayload::UserMessage {
            text: "first task".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::AssistantMessage {
            text: "first approach".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ContextCompacted {
            removed_items: 2,
            summary: "first continuation".to_owned(),
            state: compacted_state("first task"),
        },
    );
    append(
        &store,
        &session,
        EventPayload::UserMessage {
            text: "next task".to_owned(),
        },
    );
    append(
        &store,
        &session,
        EventPayload::ContextCompacted {
            removed_items: 1,
            summary: "second continuation".to_owned(),
            state: compacted_state("next task"),
        },
    );
    append(
        &store,
        &session,
        EventPayload::SessionCompleted { reason: None },
    );

    let original_event_count = store.load(&session.id).unwrap().events.len();
    let state = store.load(&session.id).unwrap().state().unwrap();
    assert_eq!(state.context_compactions, 2);
    assert_eq!(state.continuation.unwrap().task, "next task");
    assert_eq!(state.messages.len(), 3);
    assert_eq!(state.working_messages.len(), 3);

    let resumed = store.resume(&session.id).unwrap();
    assert_eq!(resumed.events.len(), original_event_count + 1);
    assert_eq!(resumed.state().unwrap().status, SessionStatus::Active);
    assert_eq!(
        resumed.events[1].payload,
        EventPayload::UserMessage {
            text: "first task".to_owned(),
        }
    );
}

#[test]
fn ignores_only_a_truncated_trailing_record_and_preserves_prior_events() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path().join("workspace");
    std::fs::create_dir_all(&workspace).unwrap();
    let store = JsonlSessionStore::new(temporary.path().join("sessions")).unwrap();
    let session = store.create(&workspace).unwrap();
    append(
        &store,
        &session,
        EventPayload::UserMessage {
            text: "durable".to_owned(),
        },
    );
    let path = temporary
        .path()
        .join("sessions")
        .join(format!("{}.jsonl", session.id));
    let mut file = OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(b"{\"event_id\":").unwrap();
    drop(file);

    let report = store.load_with_report(&session.id).unwrap();
    assert_eq!(report.session.events.len(), 2);
    assert_eq!(report.warnings.len(), 1);
    assert_eq!(report.warnings[0].line, 3);
    assert_eq!(
        report.session.events[1].payload,
        EventPayload::UserMessage {
            text: "durable".to_owned()
        }
    );
    let next = HarnessEvent::new(
        session.id.clone(),
        EventPayload::UserMessage {
            text: "must not append".to_owned(),
        },
        None,
        None,
    );
    assert!(matches!(
        store.append_event(&session.id, next),
        Err(Error::Session { .. })
    ));
}

#[test]
fn serializes_every_event_type_with_schema_compatibility() {
    let payloads = vec![
        EventPayload::SessionStarted {
            workspace_root: "workspace".into(),
        },
        EventPayload::UserMessage {
            text: "hello".to_owned(),
        },
        EventPayload::AssistantMessage {
            text: "hi".to_owned(),
        },
        EventPayload::ModelRequested {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            prompt_tokens: None,
        },
        EventPayload::ModelResponse {
            provider: "p".to_owned(),
            model: "m".to_owned(),
            text: "ok".to_owned(),
            input_tokens: None,
            output_tokens: None,
        },
        EventPayload::ToolRequested {
            tool: "t".to_owned(),
            arguments: BTreeMap::new(),
        },
        EventPayload::ToolApproved {
            tool: "t".to_owned(),
            reason: None,
        },
        EventPayload::ToolDenied {
            tool: "t".to_owned(),
            reason: "denied".to_owned(),
        },
        EventPayload::ToolStarted {
            tool: "t".to_owned(),
        },
        EventPayload::ToolOutput {
            tool: "t".to_owned(),
            output: "out".to_owned(),
        },
        EventPayload::ToolCompleted {
            tool: "t".to_owned(),
        },
        EventPayload::ToolFailed {
            tool: "t".to_owned(),
            error: "failed".to_owned(),
        },
        EventPayload::FileChanged {
            path: "file".into(),
            change: FileChange::Added,
        },
        EventPayload::CheckpointCreated {
            checkpoint_id: Id::new("c").unwrap(),
            reference: "HEAD".to_owned(),
        },
        EventPayload::VerificationStarted {
            commands: vec!["test".to_owned()],
        },
        EventPayload::VerificationResult {
            command: "test".to_owned(),
            category: "GeneralTest".to_owned(),
            duration_ms: 12,
            passed: true,
            exit_code: Some(0),
            output: "ok".to_owned(),
            diagnostics: Vec::new(),
        },
        EventPayload::SessionResumed { reason: None },
        EventPayload::ContextCompacted {
            removed_items: 1,
            summary: "summary".to_owned(),
            state: CompactState::default(),
        },
        EventPayload::SessionCompleted { reason: None },
        EventPayload::SessionFailed {
            error: "failed".to_owned(),
        },
    ];
    let session_id = harness_core::SessionId::new("session").unwrap();
    for payload in payloads {
        let event = HarnessEvent::new(session_id.clone(), payload, None, None);
        let serialized = serde_json::to_string(&event).unwrap();
        let reloaded: HarnessEvent = serde_json::from_str(&serialized).unwrap();
        assert_eq!(reloaded, event);
        assert_eq!(reloaded.schema_version, 1);
    }
}

#[test]
fn event_bus_publishes_live_events_without_persistence_ownership() {
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |_: &HarnessEvent| {
        *counter.lock().unwrap() += 1;
    }));
    let session_id = harness_core::SessionId::new("session").unwrap();
    let started = HarnessEvent::new(
        session_id.clone(),
        EventPayload::SessionStarted {
            workspace_root: "workspace".into(),
        },
        None,
        None,
    );
    let event = HarnessEvent::new(
        session_id.clone(),
        EventPayload::UserMessage {
            text: "hello".to_owned(),
        },
        None,
        None,
    );
    bus.publish(&event);
    assert_eq!(*events.lock().unwrap(), 1);
    let state: SessionState = serde_json::from_str(
        &serde_json::to_string(
            &harness_session::reconstruct_state(&session_id, &[started, event]).unwrap(),
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(state.status, SessionStatus::Active);
}
