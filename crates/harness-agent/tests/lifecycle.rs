//! End-to-end lifecycle of the v0 harness.
//!
//! This drives the real agent loop against a real Git repository and covers the
//! workflow a user depends on: discover the workspace, start a session, read and
//! search, propose an edit, have policy ask for approval, checkpoint the edit,
//! run verification, observe a failure, make a corrective edit, verify
//! successfully, produce a diff, complete the session, reload it from disk after
//! a restart, and undo the checkpoint.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::Duration;

use harness_agent::{AgentLimits, AgentRunner, AgentTask, ApprovalHandler};
use harness_context::ContextBuilder;
use harness_core::{discover_workspace, CheckpointId, Error};
use harness_git::{CheckpointStore, GitClient, ShadowCheckpointStore};
use harness_models::{
    ContentBlock, FinishReason, ModelResponse, ScriptedMockProvider, ToolCall, Usage,
};
use harness_policy::{ExecutionMode, Policy, PolicyEngine, PolicyRequest};
use harness_session::{EventType, HarnessEvent, JsonlSessionStore, Session, SessionStore};
use harness_tools::{ProcessRunner, ToolRegistry};
use harness_verification::{VerificationCategory, VerificationPlan, VerificationReport, Verifier};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A sample repository with a real manifest, so discovery and verification have
/// something to work with.
fn sample_repository() -> tempfile::TempDir {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    git(root, &["init", "--quiet"]);
    git(root, &["config", "user.email", "lifecycle@example.invalid"]);
    git(root, &["config", "user.name", "Lifecycle"]);

    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"sample\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    // The verifier accepts the file only while it does not contain `BROKEN`, so
    // a deliberately wrong first edit fails verification and the corrective edit
    // makes it pass.
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    fs::write(root.join("README.md"), "# sample\n\nA sample repository.\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "--quiet", "-m", "initial"]);
    temporary
}

/// Stands in for a real command runner: it inspects the file the agent edited,
/// so pass or fail genuinely depends on what the agent wrote.
struct FileVerifier {
    marker: PathBuf,
}

impl Verifier for FileVerifier {
    fn verify(
        &self,
        _request: &harness_verification::VerificationRequest,
    ) -> Result<Vec<VerificationReport>, Error> {
        let contents = fs::read_to_string(&self.marker).unwrap_or_default();
        let passed = !contents.contains("BROKEN");
        Ok(vec![VerificationReport {
            category: VerificationCategory::GeneralTest,
            command: "cargo test".to_owned(),
            duration_ms: 3,
            passed,
            exit_code: Some(if passed { 0 } else { 101 }),
            output: if passed {
                "test result: ok. 1 passed; 0 failed".to_owned()
            } else {
                "error: test failed; assertion `value == 2` failed".to_owned()
            },
            diagnostics: if passed {
                Vec::new()
            } else {
                vec!["assertion failed: the file still contained a broken value".to_owned()]
            },
        }])
    }
}

/// Approves every request, standing in for a person clicking "allow once".
struct AllowAll;

impl ApprovalHandler for AllowAll {
    fn request(
        &self,
        _request: &harness_tools::ToolRequest,
    ) -> Result<bool, harness_agent::AgentError> {
        Ok(true)
    }
}

fn tool_call(name: &str, arguments: serde_json::Value) -> ModelResponse {
    ModelResponse {
        id: format!("call-{name}"),
        model: "lifecycle".to_owned(),
        content: Vec::new(),
        tool_calls: vec![ToolCall {
            id: format!("id-{name}"),
            name: name.to_owned(),
            arguments,
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: Some(Usage::new(1, 1)),
    }
}

fn final_answer(text: &str) -> ModelResponse {
    ModelResponse {
        id: "final".to_owned(),
        model: "lifecycle".to_owned(),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(Usage::new(1, 1)),
    }
}

/// The full lifecycle script: read, search, a broken edit, a corrective edit.
fn lifecycle_script() -> Vec<ModelResponse> {
    vec![
        tool_call("read_file", serde_json::json!({"path": "src/lib.rs"})),
        tool_call("grep", serde_json::json!({"pattern": "pub fn"})),
        tool_call(
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "BROKEN pub fn value() -> u32 { 2 }\n"}),
        ),
        tool_call(
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "pub fn value() -> u32 { 2 }\n"}),
        ),
        final_answer("Updated the value and verified the change."),
    ]
}

struct Fixture {
    runner: AgentRunner,
    sessions: Arc<JsonlSessionStore>,
    checkpoints: Arc<dyn CheckpointStore>,
}

fn fixture(root: &Path, mode: ExecutionMode, script: Vec<ModelResponse>) -> Fixture {
    let sessions = Arc::new(JsonlSessionStore::new(root.join(".cogito/sessions")).unwrap());
    let checkpoints: Arc<dyn CheckpointStore> =
        Arc::new(ShadowCheckpointStore::new(root.join(".cogito/checkpoints")).unwrap());

    let runner = AgentRunner::new(
        Arc::new(ScriptedMockProvider::new("lifecycle", script)),
        "lifecycle",
        ToolRegistry::with_workspace_tools(),
        Arc::new(PolicyEngine::new(mode, root)),
        Arc::clone(&sessions) as Arc<dyn SessionStore>,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(AllowAll),
    )
    .with_checkpoints(Arc::clone(&checkpoints))
    .with_verifier(Arc::new(FileVerifier {
        marker: root.join("src/lib.rs"),
    }));

    Fixture {
        runner,
        sessions,
        checkpoints,
    }
}

fn task_for(root: &Path, user_task: &str, resume: &harness_core::SessionId) -> AgentTask {
    let description = discover_workspace(root).expect("workspace discovery");
    AgentTask {
        workspace_root: root.to_path_buf(),
        user_task: user_task.to_owned(),
        system_instructions: "Use tools safely.".to_owned(),
        workspace: Default::default(),
        instructions: description.instructions.clone(),
        verification_plan: Some(VerificationPlan::all(&description)),
        resume_session: Some(resume.clone()),
        ..AgentTask::default()
    }
}

fn event_types(session: &Session) -> Vec<EventType> {
    session
        .events
        .iter()
        .map(|event| event.event_type)
        .collect()
}

fn tool_names(session: &Session) -> Vec<String> {
    session
        .events
        .iter()
        .filter(|event: &&HarnessEvent| event.event_type == EventType::ToolRequested)
        .filter_map(|event| match &event.payload {
            harness_session::EventPayload::ToolRequested { tool, .. } => Some(tool.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_complete_v0_lifecycle_works_end_to_end() {
    let temporary = sample_repository();
    let root = temporary.path().to_path_buf();
    let harness = fixture(&root, ExecutionMode::Normal, lifecycle_script());

    // Discover the workspace.
    let description = discover_workspace(&root).expect("workspace discovery");
    assert!(
        description.repository_root.is_some(),
        "expected a Git repository"
    );
    assert!(
        description
            .manifests
            .iter()
            .any(|manifest| manifest.path.ends_with("Cargo.toml")),
        "expected the Cargo manifest to be detected"
    );

    // Start a session.
    let session = harness.sessions.create(&root).expect("create session");
    let session_id = session.id.clone();
    assert!(
        harness
            .sessions
            .recent(10)
            .unwrap()
            .iter()
            .any(|entry| entry.id == session_id),
        "the new session must be listed"
    );

    // Run the task: read, search, broken edit, corrective edit.
    let outcome = harness
        .runner
        .run(
            &task_for(
                &root,
                "Change the value returned by src/lib.rs.",
                &session_id,
            ),
            &harness_tools::CancellationToken::new(),
        )
        .expect("the agent run must complete");

    assert_eq!(outcome.session_id, session_id);
    assert!(
        outcome.final_message.contains("verified"),
        "expected the final message to be reported: {:?}",
        outcome.final_message
    );
    assert_eq!(
        outcome.turns, 5,
        "expected read, search, broken edit, fix, and answer"
    );

    // The model really did read and search, recorded in the transcript.
    let tools = tool_names(&harness.sessions.load(&session_id).unwrap());
    assert!(
        tools.iter().any(|name| name == "read_file"),
        "expected a read: {tools:?}"
    );
    assert!(
        tools.iter().any(|name| name == "grep"),
        "expected a search: {tools:?}"
    );
    assert!(
        tools.iter().any(|name| name == "write_file"),
        "expected an edit: {tools:?}"
    );

    // Policy recorded a decision for the edit.
    let transcript = harness.sessions.load(&session_id).unwrap();
    assert!(
        transcript
            .events
            .iter()
            .any(|event| { event.event_type == EventType::PolicyDecision }),
        "expected the policy decision to be recorded"
    );

    // Verification ran, and the failure was fed back before the fix.
    let verifications: Vec<&HarnessEvent> = transcript
        .events
        .iter()
        .filter(|event| event.event_type == EventType::VerificationResult)
        .collect();
    assert!(
        !verifications.is_empty(),
        "expected the agent to run verification after editing"
    );
    let any_failed = verifications.iter().any(|event| {
        matches!(
            &event.payload,
            harness_session::EventPayload::VerificationResult { passed, .. } if !passed
        )
    });
    assert!(any_failed, "expected the first verification to fail");

    // The session completed and is durable on disk.
    let stored = harness.sessions.load(&session_id).expect("reload session");
    assert!(
        event_types(&stored).contains(&EventType::SessionCompleted),
        "expected a persisted session.completed"
    );

    // The corrective edit is what remains.
    let contents = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(
        !contents.contains("BROKEN"),
        "the corrective edit was not applied: {contents:?}"
    );
    assert!(
        contents.contains('2'),
        "the new value is missing: {contents:?}"
    );

    // A diff against HEAD is available.
    let client = GitClient::open(&root).unwrap();
    let change = client.file_change("src/lib.rs").expect("file change");
    assert_eq!(change.original, "pub fn value() -> u32 { 1 }\n");
    assert!(change.modified.contains('2'), "modified side: {change:?}");
    assert!(change.additions >= 1, "expected an added line: {change:?}");
    assert!(!change.patch.is_empty(), "expected a unified patch");

    // A checkpoint recorded the edit.
    let checkpoints = harness.checkpoints.list().expect("list checkpoints");
    assert!(!checkpoints.is_empty(), "expected a checkpoint for the run");
    let recorded: Vec<String> = checkpoints
        .iter()
        .flat_map(|entry| entry.recorded_changes.clone())
        .collect();
    assert!(
        recorded.iter().any(|path| path == "src/lib.rs"),
        "the checkpoint did not record the edit: {recorded:?}"
    );

    // Undo restores the file and leaves unrelated user work alone.
    fs::write(root.join("user-notes.txt"), "unrelated user work\n").unwrap();
    let target = checkpoints[0].id.clone();
    let report = harness.checkpoints.undo(&target).expect("undo checkpoint");
    assert!(
        report.conflicts.is_empty(),
        "unexpected conflict: {report:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn value() -> u32 { 1 }\n",
        "undo did not restore the original file"
    );
    assert_eq!(
        fs::read_to_string(root.join("user-notes.txt")).unwrap(),
        "unrelated user work\n",
        "undo must not touch unrelated user work"
    );

    // An unknown checkpoint id is reported, not silently ignored.
    let missing = harness
        .checkpoints
        .undo(&CheckpointId::new("checkpoint-does-not-exist").unwrap());
    assert!(
        missing.is_err(),
        "expected an error for a missing checkpoint"
    );
}

#[test]
fn a_session_survives_a_restart_and_resumes() {
    let temporary = sample_repository();
    let root = temporary.path().to_path_buf();
    let first = fixture(&root, ExecutionMode::Normal, lifecycle_script());
    let session = first.sessions.create(&root).unwrap();
    let session_id = session.id.clone();

    first
        .runner
        .run(
            &task_for(&root, "Change the value.", &session_id),
            &harness_tools::CancellationToken::new(),
        )
        .expect("first run");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn value() -> u32 { 2 }\n"
    );

    // Simulate an application restart: drop everything and rebuild from disk.
    drop(first);
    let reopened = fixture(
        &root,
        ExecutionMode::Normal,
        vec![final_answer("Resumed run done.")],
    );
    let restored = reopened
        .sessions
        .load(&session_id)
        .expect("reload after restart");
    assert!(
        event_types(&restored).contains(&EventType::SessionCompleted),
        "the session did not survive a restart"
    );
    assert!(
        restored.events.len() > 2,
        "the restored transcript looks truncated: {} events",
        restored.events.len()
    );

    // Resuming continues the same durable transcript.
    reopened
        .sessions
        .resume(&session_id)
        .expect("resume session");
    reopened
        .runner
        .run(
            &task_for(&root, "Continue.", &session_id),
            &harness_tools::CancellationToken::new(),
        )
        .expect("resumed run");

    let after = reopened.sessions.load(&session_id).unwrap();
    assert!(
        after.events.len() > restored.events.len(),
        "the resumed run must append to the transcript"
    );
    let completions = event_types(&after)
        .iter()
        .filter(|kind| **kind == EventType::SessionCompleted)
        .count();
    assert!(
        completions >= 2,
        "expected a second completion, got {completions}"
    );
}

#[test]
fn safe_mode_asks_before_an_edit() {
    let temporary = sample_repository();
    let root = temporary.path();
    let policy = PolicyEngine::new(ExecutionMode::Safe, root);

    let decision = policy.evaluate(&PolicyRequest {
        tool_name: "write_file".to_owned(),
        operation: harness_policy::OperationKind::Write,
        workspace_root: root.to_path_buf(),
        path: Some(root.join("src/lib.rs")),
        command: None,
        mode: ExecutionMode::Safe,
    });

    assert_eq!(
        decision.decision,
        harness_policy::PolicyDecision::Ask,
        "safe mode must ask before writing"
    );
}

#[test]
fn a_denied_edit_never_reaches_the_filesystem() {
    let temporary = sample_repository();
    let root = temporary.path();
    let before = fs::read_to_string(root.join("src/lib.rs")).unwrap();

    let context = harness_tools::ToolContext {
        policy: &harness_policy::DenyAllPolicy,
        working_directory: root,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let registry = ToolRegistry::with_workspace_tools();
    let result = registry.execute(
        &context,
        harness_tools::ToolRequest::new(
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "owned"}),
        ),
    );

    assert!(result.is_err(), "a denied edit must not succeed");
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        before,
        "the file must be untouched after a denial"
    );
}

#[test]
fn a_cancelled_run_fails_cleanly_and_writes_nothing() {
    let temporary = sample_repository();
    let root = temporary.path().to_path_buf();
    let mut script = vec![tool_call(
        "write_file",
        serde_json::json!({"path": "src/lib.rs", "content": "edited\n"}),
    )];
    for index in 0..8 {
        script.push(tool_call(
            "list_directory",
            serde_json::json!({"path": format!("iter-{index}")}),
        ));
    }
    script.push(final_answer("should not be reached"));

    let harness = fixture(&root, ExecutionMode::Normal, script);
    let session = harness.sessions.create(&root).unwrap();
    let token = harness_tools::CancellationToken::new();
    token.cancel();

    let result = harness
        .runner
        .run(&task_for(&root, "Edit the file.", &session.id), &token);

    assert!(result.is_err(), "a pre-cancelled run must not succeed");
    let events = event_types(&harness.sessions.load(&session.id).unwrap());
    assert!(
        events.contains(&EventType::SessionFailed),
        "a cancelled run must be recorded as failed, got {events:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn value() -> u32 { 1 }\n",
        "a cancelled run must not apply an edit"
    );
}

#[test]
fn a_missing_workspace_is_reported_clearly() {
    let missing = PathBuf::from("/definitely/not/a/real/path/xyz");
    assert!(
        discover_workspace(&missing).is_err(),
        "expected discovery to fail for a missing path"
    );
}

#[test]
fn a_torn_trailing_session_record_is_surfaced_as_a_warning() {
    // A crash mid-append leaves a half-written final line. Recovering the
    // earlier events is correct, but the damage must be reported rather than
    // hidden.
    let temporary = sample_repository();
    let root = temporary.path();
    let sessions = JsonlSessionStore::new(root.join(".cogito/sessions")).unwrap();
    let session = sessions.create(root).unwrap();
    sessions
        .append_event(
            &session.id,
            HarnessEvent::new(
                session.id.clone(),
                harness_session::EventPayload::SessionCompleted {
                    reason: Some("done".to_owned()),
                },
                None,
                None,
            ),
        )
        .unwrap();

    let path = root
        .join(".cogito/sessions")
        .join(format!("{}.jsonl", session.id));
    let mut contents = fs::read_to_string(&path).unwrap();
    let complete = contents.clone();
    contents.push_str("{ this record was never finished");
    fs::write(&path, contents).unwrap();

    let report = sessions
        .load_with_report(&session.id)
        .expect("a torn trailing record must still load");
    assert!(
        !report.warnings.is_empty(),
        "a torn trailing record must be reported to the caller"
    );
    assert!(
        report.session.events.len() == 2,
        "expected the two intact events to survive, got {}",
        report.session.events.len()
    );

    // A record damaged in the middle is real corruption and must fail loudly,
    // because silently dropping it would rewrite history.
    fs::write(&path, format!("{{ broken first record\n{complete}")).unwrap();
    assert!(
        sessions.load(&session.id).is_err(),
        "a corrupt record before the end must be an error, not a silent gap"
    );
}

#[test]
fn an_exhausted_agent_loop_still_closes_its_session() {
    let temporary = sample_repository();
    let root = temporary.path();
    let mut script = Vec::new();
    for index in 0..200 {
        script.push(tool_call(
            "list_directory",
            serde_json::json!({"path": "."}),
        ));
        let _ = index;
    }
    script.push(final_answer("never reached"));

    let harness = fixture(root, ExecutionMode::Normal, script);
    let session = harness.sessions.create(root).unwrap();
    let _ = harness.runner.run(
        &task_for(root, "Loop.", &session.id),
        &harness_tools::CancellationToken::new(),
    );

    let events = event_types(
        &harness
            .sessions
            .load(&session.id)
            .expect("session must load"),
    );
    assert!(
        events.contains(&EventType::SessionCompleted) || events.contains(&EventType::SessionFailed),
        "an exhausted loop must still close the session, got {events:?}"
    );
}

#[test]
fn verification_is_skipped_when_nothing_changed() {
    let temporary = sample_repository();
    let root = temporary.path().to_path_buf();
    let harness = fixture(
        &root,
        ExecutionMode::Normal,
        vec![
            tool_call("read_file", serde_json::json!({"path": "README.md"})),
            final_answer("Nothing to change."),
        ],
    );
    let session = harness.sessions.create(&root).unwrap();

    harness
        .runner
        .run(
            &task_for(&root, "Just look.", &session.id),
            &harness_tools::CancellationToken::new(),
        )
        .expect("read-only run");

    let events = event_types(&harness.sessions.load(&session.id).unwrap());
    assert!(
        !events.contains(&EventType::VerificationResult),
        "verification must not run when no files changed"
    );
}

#[test]
fn shell_commands_are_policy_gated() {
    let temporary = sample_repository();
    let root = temporary.path();
    let request_for = |mode: ExecutionMode, command: &str| PolicyRequest {
        tool_name: "shell".to_owned(),
        operation: harness_policy::OperationKind::Command,
        workspace_root: root.to_path_buf(),
        path: None,
        command: Some(command.to_owned()),
        mode,
    };

    let read_only = PolicyEngine::new(ExecutionMode::ReadOnly, root);
    assert_eq!(
        read_only
            .evaluate(&request_for(ExecutionMode::ReadOnly, "rm -rf /"))
            .decision,
        harness_policy::PolicyDecision::Deny,
        "read-only must deny a command"
    );

    let normal = PolicyEngine::new(ExecutionMode::Normal, root);
    assert_eq!(
        normal
            .evaluate(&request_for(
                ExecutionMode::Normal,
                "curl https://example.com"
            ))
            .decision,
        harness_policy::PolicyDecision::Ask,
        "normal mode must ask for an unknown command"
    );
}

#[test]
fn a_request_cannot_widen_the_configured_permission_mode() {
    // The engine holds the mode the user selected. A request that claims a more
    // permissive mode must not be able to talk its way past it.
    let temporary = sample_repository();
    let root = temporary.path();
    let engine = PolicyEngine::new(ExecutionMode::ReadOnly, root);

    let escalation = PolicyRequest {
        tool_name: "write_file".to_owned(),
        operation: harness_policy::OperationKind::Write,
        workspace_root: root.to_path_buf(),
        path: Some(root.join("src/lib.rs")),
        command: None,
        mode: ExecutionMode::Auto,
    };

    assert_eq!(
        engine.evaluate(&escalation).decision,
        harness_policy::PolicyDecision::Deny,
        "a request must not override the engine's read-only mode"
    );
}

#[test]
fn a_long_running_shell_command_respects_its_timeout() {
    let temporary = sample_repository();
    let root = temporary.path();
    let (program, args) = if cfg!(windows) {
        (
            "pwsh".to_owned(),
            vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "Start-Sleep -Seconds 30".to_owned(),
            ],
        )
    } else {
        ("sleep".to_owned(), vec!["30".to_owned()])
    };
    let request = harness_tools::ProcessRequest {
        program,
        args,
        working_directory: root.to_path_buf(),
        timeout: Duration::from_millis(500),
        max_output_bytes: 1024,
    };

    let started = std::time::Instant::now();
    let result = harness_tools::LocalProcessRunner.execute(
        request,
        &harness_tools::CancellationToken::new(),
        &mut |_| Ok(()),
    );

    let result = result.expect("the runner must return a result");
    assert!(result.timed_out, "a long command must time out");
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the timeout was not honoured"
    );
}
