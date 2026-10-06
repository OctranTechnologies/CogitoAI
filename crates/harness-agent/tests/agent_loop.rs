use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use harness_agent::{
    AgentLimits, AgentRunner, AgentTask, ApprovalHandler, CompactionConfig, DenyApprovalHandler,
};
use harness_context::{ContextBudget, ContextBuilder, WorkspaceMetadata};
use harness_models::{
    AnthropicProvider, ContentBlock, GeminiProvider, ModelCapabilities, ModelDescriptor,
    ModelProvider, ModelRequest, ModelResponse, ModelStreamEvent, OpenAIProvider, ProviderError,
    ScriptedMockProvider, ToolCall, Usage,
};
use harness_policy::{AllowAllPolicy, DenyAllPolicy, ExecutionMode, PolicyEngine};
use harness_session::{JsonlSessionStore, SessionStatus, SessionStore, TaskMode};
use harness_tools::{CancellationToken, ToolRegistry};
use harness_verification::{
    FailureOrigin, VerificationCategory, VerificationFailure, VerificationPlan, VerificationReport,
    VerificationRequest, VerificationStep, Verifier,
};
use tempfile::tempdir;

fn read_http_request(stream: &mut TcpStream) -> (String, String) {
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    let (header_end, content_length) = loop {
        let count = stream.read(&mut chunk).expect("read test request");
        assert!(count > 0, "request closed before headers");
        bytes.extend_from_slice(&chunk[..count]);
        if let Some(end) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            let header_end = end + 4;
            let headers = String::from_utf8_lossy(&bytes[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
            if bytes.len() >= header_end + content_length {
                break (header_end, content_length);
            }
        }
    };
    while bytes.len() < header_end + content_length {
        let count = stream.read(&mut chunk).expect("read test request body");
        assert!(count > 0, "request closed before body");
        bytes.extend_from_slice(&chunk[..count]);
    }
    (
        String::from_utf8_lossy(&bytes[..header_end]).into_owned(),
        String::from_utf8_lossy(&bytes[header_end..header_end + content_length]).into_owned(),
    )
}

fn respond_with_sse(stream: &mut TcpStream, events: &[serde_json::Value]) {
    let body = events
        .iter()
        .map(|event| format!("data: {event}\n\n"))
        .collect::<String>();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .expect("write SSE headers");
    stream.write_all(body.as_bytes()).expect("write SSE body");
    stream.flush().expect("flush SSE response");
}

fn response(text: &str, tool: Option<(&str, serde_json::Value)>) -> ModelResponse {
    response_many(text, tool.into_iter().collect())
}

fn response_many(text: &str, tools: Vec<(&str, serde_json::Value)>) -> ModelResponse {
    let has_tool = !tools.is_empty();
    let mut response = ModelResponse {
        id: "scripted".to_owned(),
        model: "scripted".to_owned(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }]
        },
        tool_calls: tools
            .into_iter()
            .enumerate()
            .map(|(index, (name, arguments))| ToolCall {
                id: format!("call-{name}-{index}"),
                name: name.to_owned(),
                arguments,
            })
            .collect(),
        finish_reason: if has_tool {
            harness_models::FinishReason::ToolCalls
        } else {
            harness_models::FinishReason::Stop
        },
        usage: Some(Usage::new(10, 2)),
    };
    response.model = "scripted".to_owned();
    response
}

struct ScriptedVerifier {
    results: Mutex<VecDeque<bool>>,
}

impl ScriptedVerifier {
    fn new(results: impl IntoIterator<Item = bool>) -> Self {
        Self {
            results: Mutex::new(results.into_iter().collect()),
        }
    }
}

impl Verifier for ScriptedVerifier {
    fn verify(
        &self,
        request: &VerificationRequest,
    ) -> Result<Vec<VerificationReport>, harness_core::Error> {
        let mut results = self
            .results
            .lock()
            .expect("verification script lock poisoned");
        Ok(request
            .plan
            .steps
            .iter()
            .map(|step| {
                let passed = results.pop_front().unwrap_or(true);
                VerificationReport {
                    category: step.category,
                    command: "mock test command".to_owned(),
                    duration_ms: 1,
                    passed,
                    exit_code: Some(if passed { 0 } else { 7 }),
                    output: if passed {
                        "tests passed".to_owned()
                    } else {
                        "verification failure".to_owned()
                    },
                    diagnostics: Vec::new(),
                    failure: None,
                }
            })
            .collect())
    }
}

struct RecordingProvider {
    responses: Mutex<VecDeque<ModelResponse>>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
}

impl RecordingProvider {
    fn new(requests: Arc<Mutex<Vec<ModelRequest>>>, responses: Vec<ModelResponse>) -> Self {
        Self {
            responses: Mutex::new(responses.into_iter().collect()),
            requests,
        }
    }

    fn response(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.requests
            .lock()
            .expect("model request lock poisoned")
            .push(request.clone());
        self.responses
            .lock()
            .expect("model response lock poisoned")
            .pop_front()
            .ok_or(ProviderError::Transport {
                provider: "recording",
            })
    }
}

impl ModelProvider for RecordingProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            provider: "recording".to_owned(),
            id: "recording".to_owned(),
            display_name: "Recording".to_owned(),
            capabilities: ModelCapabilities {
                text_input: true,
                streaming: true,
                tool_calling: true,
                system_instructions: true,
                context_window: Some(4096),
                ..ModelCapabilities::default()
            },
            metadata: Default::default(),
        }
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.response(request)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.response(request)?;
        on_event(ModelStreamEvent::ResponseStarted {
            id: Some(response.id.clone()),
            model: response.model.clone(),
        })?;
        on_event(ModelStreamEvent::TextDelta {
            text: response.text(),
        })?;
        on_event(ModelStreamEvent::ResponseCompleted {
            finish_reason: response.finish_reason.clone(),
        })?;
        Ok(response)
    }
}

struct ApproveAll;

impl ApprovalHandler for ApproveAll {
    fn request(
        &self,
        _tool: &harness_tools::ToolRequest,
    ) -> Result<bool, harness_agent::AgentError> {
        Ok(true)
    }
}

fn task(workspace: &Path) -> AgentTask {
    AgentTask {
        workspace_root: workspace.to_path_buf(),
        user_task: "Read, edit, run, and finish".to_owned(),
        system_instructions: "Use tools carefully.".to_owned(),
        workspace: WorkspaceMetadata {
            root: Some(workspace.to_path_buf()),
            ..WorkspaceMetadata::default()
        },
        ..AgentTask::default()
    }
}

fn mock_verification_plan() -> VerificationPlan {
    VerificationPlan {
        steps: vec![VerificationStep {
            category: VerificationCategory::Build,
            command: harness_core::CommandSpec::new("mock-test", ["--quiet"]),
            source: "test fixture".to_owned(),
        }],
    }
}

fn runner(
    provider: Arc<ScriptedMockProvider>,
    workspace: &Path,
    policy: Arc<dyn harness_policy::Policy>,
    approval: Arc<dyn ApprovalHandler>,
) -> (AgentRunner, Arc<JsonlSessionStore>) {
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let runner = AgentRunner::new(
        provider,
        "scripted",
        ToolRegistry::with_workspace_tools(),
        policy,
        sessions.clone(),
        ContextBuilder::default(),
        AgentLimits::default(),
        approval,
    );
    (runner, sessions)
}

#[test]
fn mock_agent_reads_edits_runs_observes_and_finishes() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(workspace.join("file.txt"), "before\n").unwrap();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response(
                "",
                Some(("read_file", serde_json::json!({ "path": "file.txt" }))),
            ),
            response(
                "",
                Some((
                    "write_file",
                    serde_json::json!({ "path": "file.txt", "content": "after\n" }),
                )),
            ),
            response(
                "",
                Some(("shell", serde_json::json!({ "command": "echo command-ok" }))),
            ),
            response("All done", None),
        ],
    ));
    let policy = Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace));
    let (runner, sessions) = runner(provider, workspace, policy, Arc::new(ApproveAll));
    let runner = runner.with_verifier(Arc::new(ScriptedVerifier::new([true])));
    let task = AgentTask {
        verification_plan: Some(mock_verification_plan()),
        ..task(workspace)
    };

    let outcome = runner.run(&task, &CancellationToken::new()).unwrap();

    assert_eq!(outcome.final_message, "All done");
    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Done
    );
    assert_eq!(outcome.turns, 4);
    assert_eq!(outcome.tool_calls, 3);
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "after\n"
    );
    let session = sessions.load(&outcome.session_id).unwrap();
    let state = session.state().unwrap();
    assert_eq!(state.status, SessionStatus::Completed);
    let task_run = state.task_run.expect("persisted task run");
    assert_eq!(task_run.original_goal, "Read, edit, run, and finish");
    assert_eq!(
        task_run.completion_status,
        harness_session::TaskCompletionStatus::Done
    );
    assert!(task_run.relevant_files.contains(&"file.txt".into()));
    assert!(task_run
        .changed_files
        .iter()
        .any(|path| path.ends_with("file.txt")));
    assert!(task_run
        .commands_executed
        .iter()
        .any(|command| command == "echo command-ok"));
    assert!(task_run.unresolved_errors.is_empty());
    let event_types = session
        .events
        .iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(event_types.contains(&harness_session::EventType::ToolRequested));
    assert!(event_types.contains(&harness_session::EventType::TaskRunUpdated));
    assert!(event_types.contains(&harness_session::EventType::PolicyDecision));
    assert!(event_types.contains(&harness_session::EventType::FileChanged));
    assert!(event_types.contains(&harness_session::EventType::ProcessExited));
    assert_eq!(
        event_types.last(),
        Some(&harness_session::EventType::SessionCompleted)
    );
}

#[test]
fn task_modes_and_permission_modes_compose_without_read_only_bypass() {
    let cases = [
        (TaskMode::Explore, ExecutionMode::ReadOnly),
        (TaskMode::Explore, ExecutionMode::Safe),
        (TaskMode::Explore, ExecutionMode::Normal),
        (TaskMode::Explore, ExecutionMode::Auto),
        (TaskMode::Plan, ExecutionMode::ReadOnly),
        (TaskMode::Plan, ExecutionMode::Safe),
        (TaskMode::Plan, ExecutionMode::Normal),
        (TaskMode::Plan, ExecutionMode::Auto),
        (TaskMode::Code, ExecutionMode::ReadOnly),
        (TaskMode::Code, ExecutionMode::Safe),
        (TaskMode::Code, ExecutionMode::Normal),
        (TaskMode::Code, ExecutionMode::Auto),
    ];

    for (task_mode, permission_mode) in cases {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path();
        let plan = "Goal\nImplement a harmless test fixture update.\n\nRelevant architecture\n- Existing file-backed fixture.\n\nFiles likely affected\n- target.txt\n\nImplementation steps\n- Update target.txt.\n\nValidation\n- Run the fixture check.\n\nRisks/unknowns\n- None known.";
        let final_text = match task_mode {
            TaskMode::Explore => "The repository contains a small file-backed fixture.",
            TaskMode::Plan => plan,
            TaskMode::Code => "Implementation complete and verified.",
        };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(
            requests.clone(),
            vec![
                response(
                    "",
                    Some((
                        "write_file",
                        serde_json::json!({ "path": "target.txt", "content": "changed" }),
                    )),
                ),
                response(final_text, None),
            ],
        ));
        let policy = Arc::new(PolicyEngine::new(permission_mode, workspace));
        let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
        let runner = AgentRunner::new(
            provider,
            "recording",
            ToolRegistry::with_workspace_tools(),
            policy,
            sessions.clone(),
            ContextBuilder::default(),
            AgentLimits::default(),
            Arc::new(ApproveAll),
        );
        let runner = runner.with_verifier(Arc::new(ScriptedVerifier::new([true])));
        let mut task = task(workspace);
        task.task_mode = task_mode;
        if task_mode == TaskMode::Code {
            task.verification_plan = Some(mock_verification_plan());
        }

        let result = runner.run(&task, &CancellationToken::new());
        let should_edit = task_mode == TaskMode::Code && permission_mode != ExecutionMode::ReadOnly;
        assert_eq!(
            workspace.join("target.txt").exists(),
            should_edit,
            "{task_mode:?} + {permission_mode:?}"
        );
        let requests = requests.lock().unwrap();
        for request in requests.iter() {
            let advertises_write = request
                .tools
                .iter()
                .any(|tool| tool.name == "write_file" || tool.name == "apply_patch");
            let advertises_command = request.tools.iter().any(|tool| tool.name == "shell");
            assert_eq!(
                advertises_write,
                task_mode == TaskMode::Code,
                "write tools must follow task behavior, not permission mode: {task_mode:?} + {permission_mode:?}"
            );
            assert_eq!(
                advertises_command,
                task_mode == TaskMode::Code,
                "shell must follow task behavior, not permission mode: {task_mode:?} + {permission_mode:?}"
            );
        }
        drop(requests);
        if should_edit {
            assert!(
                result.is_ok(),
                "{task_mode:?} + {permission_mode:?}: {result:?}"
            );
        } else if task_mode == TaskMode::Code {
            assert!(
                result.is_err(),
                "read-only execution permission must deny CODE writes"
            );
        } else {
            let outcome = result
                .unwrap_or_else(|error| panic!("{task_mode:?} + {permission_mode:?}: {error}"));
            assert_eq!(
                outcome.completion_status,
                harness_session::TaskCompletionStatus::Done
            );
        }

        let session_id = sessions.recent(1).unwrap()[0].id.clone();
        let state = sessions.load(&session_id).unwrap().state().unwrap();
        let task_run = state.task_run.expect("task mode is persisted");
        assert_eq!(task_run.task_mode, task_mode);
        if task_mode == TaskMode::Plan {
            let plan = task_run
                .structured_plan
                .expect("PLAN response should be structured");
            assert_eq!(plan.goal, "Read, edit, run, and finish");
            assert!(!plan.relevant_architecture.is_empty());
            assert!(plan
                .files_likely_affected
                .iter()
                .any(|file| file == "target.txt"));
            assert!(!plan.implementation_steps.is_empty());
            assert!(!plan.validation.is_empty());
            assert!(!plan.risks_or_unknowns.is_empty());
        }
    }
}

#[test]
fn approved_plan_continues_into_code_with_the_same_persisted_context() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(workspace.join("target.txt"), "before").unwrap();
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let policy = Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace));
    let plan_text = "Goal\nUpdate the fixture.\n\nRelevant architecture\n- The target is a text fixture.\n\nFiles likely affected\n- target.txt\n\nImplementation steps\n- Write the requested value.\n\nValidation\n- Run fixture validation.\n\nRisks/unknowns\n- None.";
    let first_requests = Arc::new(Mutex::new(Vec::new()));
    let planning_runner = AgentRunner::new(
        Arc::new(RecordingProvider::new(
            first_requests,
            vec![response(plan_text, None)],
        )),
        "recording",
        ToolRegistry::with_workspace_tools(),
        policy.clone(),
        sessions.clone(),
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    );
    let plan_task = AgentTask {
        task_mode: TaskMode::Plan,
        user_task: "Update the fixture after I approve the plan".to_owned(),
        ..task(workspace)
    };
    let plan_outcome = planning_runner
        .run(&plan_task, &CancellationToken::new())
        .unwrap();
    let plan_state = sessions
        .load(&plan_outcome.session_id)
        .unwrap()
        .state()
        .unwrap();
    let persisted_plan = plan_state.task_run.unwrap().structured_plan.unwrap();
    assert_eq!(persisted_plan.goal, plan_task.user_task);
    assert!(persisted_plan
        .implementation_steps
        .iter()
        .any(|step| step.contains("Write")));

    let requests = Arc::new(Mutex::new(Vec::new()));
    let code_runner = AgentRunner::new(
        Arc::new(RecordingProvider::new(
            requests.clone(),
            vec![
                response(
                    "",
                    Some((
                        "write_file",
                        serde_json::json!({ "path": "target.txt", "content": "after" }),
                    )),
                ),
                response("Done", None),
            ],
        )),
        "recording",
        ToolRegistry::with_workspace_tools(),
        policy,
        sessions.clone(),
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    )
    .with_verifier(Arc::new(ScriptedVerifier::new([true])));
    let code_task = AgentTask {
        task_mode: TaskMode::Code,
        user_task: "Implement the approved plan".to_owned(),
        resume_session: Some(plan_outcome.session_id.clone()),
        verification_plan: Some(mock_verification_plan()),
        ..task(workspace)
    };
    let code_outcome = code_runner
        .run(&code_task, &CancellationToken::new())
        .unwrap();

    assert_eq!(code_outcome.session_id, plan_outcome.session_id);
    assert_eq!(
        std::fs::read_to_string(workspace.join("target.txt")).unwrap(),
        "after"
    );
    let final_state = sessions
        .load(&code_outcome.session_id)
        .unwrap()
        .state()
        .unwrap();
    let task_run = final_state.task_run.unwrap();
    assert_eq!(task_run.task_mode, TaskMode::Code);
    assert_eq!(task_run.original_goal, plan_task.user_task);
    assert_eq!(task_run.structured_plan, Some(persisted_plan));
    let request = requests.lock().unwrap();
    let system_prompt = request[0]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(system_prompt.contains("Approved implementation plan"));
    assert!(system_prompt.contains("Write the requested value"));
}

#[test]
fn denied_tool_cannot_bypass_policy() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![response(
            "",
            Some((
                "write_file",
                serde_json::json!({ "path": "blocked.txt", "content": "blocked" }),
            )),
        )],
    ));
    let (runner, sessions) = runner(
        provider,
        workspace,
        Arc::new(DenyAllPolicy),
        Arc::new(DenyApprovalHandler),
    );

    let result = runner.run(&task(workspace), &CancellationToken::new());

    assert!(result.is_err());
    assert!(!workspace.join("blocked.txt").exists());
    let session_id = sessions.recent(1).unwrap()[0].id.clone();
    let session = sessions.load(&session_id).unwrap();
    assert!(session
        .events
        .iter()
        .any(|event| event.event_type == harness_session::EventType::ToolDenied));
}

#[test]
fn cancellation_records_failed_session_before_model_call() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new("scripted", vec![]));
    let (runner, sessions) = runner(
        provider,
        workspace,
        Arc::new(AllowAllPolicy),
        Arc::new(DenyApprovalHandler),
    );
    let cancellation = CancellationToken::new();
    cancellation.cancel();

    let result = runner.run(&task(workspace), &cancellation);

    assert!(matches!(result, Err(harness_agent::AgentError::Cancelled)));
    let session_id = sessions.recent(1).unwrap()[0].id.clone();
    let session = sessions.load(&session_id).unwrap();
    assert_eq!(session.state().unwrap().status, SessionStatus::Failed);
    assert_eq!(
        session.state().unwrap().task_run.unwrap().completion_status,
        harness_session::TaskCompletionStatus::Cancelled
    );
}

#[test]
fn turn_limit_stops_before_extra_tool_execution() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![response(
            "",
            Some((
                "write_file",
                serde_json::json!({ "path": "blocked.txt", "content": "blocked" }),
            )),
        )],
    ));
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let runner = AgentRunner::new(
        provider,
        "scripted",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        sessions,
        ContextBuilder::default(),
        AgentLimits {
            max_turns: 1,
            ..AgentLimits::default()
        },
        Arc::new(DenyApprovalHandler),
    );

    let result = runner.run(&task(workspace), &CancellationToken::new());

    assert!(matches!(
        result,
        Err(harness_agent::AgentError::LimitExceeded { .. })
    ));
    assert!(!workspace.join("blocked.txt").exists());
}

#[test]
fn failing_verification_is_repaired_before_task_is_marked_done() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(workspace.join("file.txt"), "before").unwrap();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response(
                "",
                Some((
                    "write_file",
                    serde_json::json!({ "path": "file.txt", "content": "broken" }),
                )),
            ),
            response(
                "The test failed; repairing the file now.",
                Some((
                    "write_file",
                    serde_json::json!({ "path": "file.txt", "content": "fixed" }),
                )),
            ),
            response("Fixed the issue and the test passes.", None),
        ],
    ));
    let policy = Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace));
    let (runner, sessions) = runner(provider, workspace, policy, Arc::new(ApproveAll));
    let plan = VerificationPlan {
        steps: vec![VerificationStep {
            category: VerificationCategory::Build,
            command: harness_core::CommandSpec::new("mock-test", ["file.txt"]),
            source: "test".to_owned(),
        }],
    };
    let mut task = task(workspace);
    task.verification_plan = Some(plan);
    let runner = runner.with_verifier(Arc::new(ScriptedVerifier::new([false, true])));

    let outcome = runner.run(&task, &CancellationToken::new()).unwrap();

    assert_eq!(
        outcome.final_message,
        "Fixed the issue and the test passes."
    );
    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Done
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "fixed"
    );
    let session = sessions.load(&outcome.session_id).unwrap();
    let results = session
        .events
        .iter()
        .filter_map(|event| match &event.payload {
            harness_session::EventPayload::VerificationResult {
                category, passed, ..
            } if category == "Build" => Some(*passed),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(results, vec![false, true]);
    assert_eq!(
        session.state().unwrap().task_run.unwrap().completion_status,
        harness_session::TaskCompletionStatus::Done
    );
}

#[test]
fn final_success_claim_cannot_override_a_failed_verification() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response(
                "",
                Some((
                    "write_file",
                    serde_json::json!({ "path": "file.txt", "content": "changed" }),
                )),
            ),
            response("Everything is fixed and all tests pass.", None),
            response("Done; the tests pass.", None),
            response("The task is complete.", None),
        ],
    ));
    let (runner, sessions) = runner(
        provider,
        workspace,
        Arc::new(AllowAllPolicy),
        Arc::new(ApproveAll),
    );
    let runner = runner.with_verifier(Arc::new(ScriptedVerifier::new([false])));
    let task = AgentTask {
        verification_plan: Some(mock_verification_plan()),
        ..task(workspace)
    };

    let outcome = runner.run(&task, &CancellationToken::new()).unwrap();

    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Blocked
    );
    assert!(outcome.final_message.starts_with("Blocked:"));
    assert!(outcome
        .remaining_work
        .iter()
        .any(|item| item.contains("verification")));
    let state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
    assert_eq!(
        state.task_run.unwrap().completion_status,
        harness_session::TaskCompletionStatus::Blocked
    );
}

#[test]
fn agent_can_finish_while_explicitly_attributing_an_unrelated_existing_failure() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response(
                "",
                Some((
                    "write_file",
                    serde_json::json!({"path":"file.txt","content":"changed"}),
                )),
            ),
            response(
                "The requested file change is complete. The legacy test failure is unrelated.\n[UNRELATED_VERIFICATION] mock test command :: its assertion is in an unchanged legacy test fixture",
                None,
            ),
        ],
    ));
    let (runner, sessions) = runner(
        provider,
        workspace,
        Arc::new(AllowAllPolicy),
        Arc::new(ApproveAll),
    );
    let runner = runner.with_verifier(Arc::new(ScriptedVerifier::new([false])));
    let task = AgentTask {
        verification_plan: Some(mock_verification_plan()),
        ..task(workspace)
    };

    let outcome = runner.run(&task, &CancellationToken::new()).unwrap();
    let state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
    let task_run = state.task_run.unwrap();

    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Done
    );
    assert!(outcome.final_message.contains("legacy test failure"));
    assert!(!outcome.final_message.contains("[UNRELATED_VERIFICATION]"));
    assert!(task_run
        .verification_results
        .iter()
        .any(|result| { !result.passed && result.failure_origin.as_deref() == Some("unrelated") }));
}

#[test]
fn structured_failure_context_is_relevant_and_bounded_before_the_next_model_turn() {
    struct LargeFailureVerifier(Mutex<usize>);

    impl Verifier for LargeFailureVerifier {
        fn verify(
            &self,
            request: &VerificationRequest,
        ) -> Result<Vec<VerificationReport>, harness_core::Error> {
            let mut runs = self.0.lock().unwrap();
            let passed = *runs > 0;
            *runs += 1;
            Ok(request
                .plan
                .steps
                .iter()
                .map(|step| VerificationReport {
                    category: step.category,
                    command: format!("{} {}", step.command.program, step.command.args.join(" ")),
                    duration_ms: 1,
                    passed,
                    exit_code: Some(if passed { 0 } else { 1 }),
                    output: if passed {
                        "tests passed".to_owned()
                    } else {
                        format!("{}raw-tail-sentinel", "full-log ".repeat(20_000))
                    },
                    diagnostics: if passed {
                        Vec::new()
                    } else {
                        vec!["diagnostic-marker: expected fixed output".to_owned()]
                    },
                    failure: (!passed).then(|| VerificationFailure {
                        category: step.category,
                        command: format!(
                            "{} {}",
                            step.command.program,
                            step.command.args.join(" ")
                        ),
                        exit_code: Some(1),
                        diagnostics: vec!["diagnostic-marker: expected fixed output".to_owned()],
                        relevant_output: format!(
                            "diagnostic-marker: expected fixed output\n{}",
                            "relevant excerpt ".repeat(1000)
                        ),
                        affected_files: vec![PathBuf::from("file.txt")],
                        origin: FailureOrigin::Introduced,
                    }),
                })
                .collect())
        }
    }

    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(workspace.join("file.txt"), "before").unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let provider = Arc::new(RecordingProvider::new(
        requests.clone(),
        vec![
            response(
                "",
                Some((
                    "write_file",
                    serde_json::json!({"path":"file.txt","content":"broken"}),
                )),
            ),
            response(
                "",
                Some((
                    "write_file",
                    serde_json::json!({"path":"file.txt","content":"fixed"}),
                )),
            ),
            response("The fix is verified.", None),
        ],
    ));
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let runner = AgentRunner::new(
        provider,
        "recording",
        ToolRegistry::with_workspace_tools(),
        Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace)),
        sessions,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    )
    .with_verifier(Arc::new(LargeFailureVerifier(Mutex::new(0))));
    let mut task = task(workspace);
    task.verification_plan = Some(VerificationPlan {
        steps: vec![VerificationStep {
            category: VerificationCategory::Build,
            command: harness_core::CommandSpec::new("mock-check", ["--quiet"]),
            source: "context fixture".to_owned(),
        }],
    });

    let outcome = runner.run(&task, &CancellationToken::new()).unwrap();
    let requests = requests.lock().unwrap();
    let repair_prompt = requests[1]
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            harness_models::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");

    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Done
    );
    assert!(repair_prompt.contains("category=Build"));
    assert!(repair_prompt.contains("file.txt"));
    assert!(repair_prompt.contains("likely_origin=introduced"));
    assert!(repair_prompt.contains("diagnostic-marker"));
    assert!(repair_prompt.contains("relevant verification output truncated"));
    assert!(!repair_prompt.contains("raw-tail-sentinel"));
    assert!(
        repair_prompt.len() < 24 * 1024,
        "failure context must stay bounded"
    );
}

#[test]
fn multi_file_feature_persists_plan_and_acceptance_criteria() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response_many(
                "Plan: add the module and its test\n- create the implementation\n- add a test",
                vec![
                    (
                        "write_file",
                        serde_json::json!({ "path": "src/module.rs", "content": "pub fn answer() -> u8 { 42 }\n" }),
                    ),
                    (
                        "write_file",
                        serde_json::json!({ "path": "tests/module.rs", "content": "#[test] fn answer_is_42() { assert_eq!(my_crate::answer(), 42); }\n" }),
                    ),
                ],
            ),
            response("Added the module and test.", None),
        ],
    ));
    let policy = Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace));
    let (runner, sessions) = runner(provider, workspace, policy, Arc::new(ApproveAll));
    let runner = runner.with_verifier(Arc::new(ScriptedVerifier::new([true, true])));
    let task = AgentTask {
        user_task: "Add answer() and a test.\nAcceptance criteria:\n- answer() returns 42\n- test covers answer()".to_owned(),
        acceptance_criteria: vec![
            "answer() returns 42".to_owned(),
            "test covers answer()".to_owned(),
        ],
        verification_plan: Some(mock_verification_plan()),
        ..task(workspace)
    };

    let outcome = runner.run(&task, &CancellationToken::new()).unwrap();

    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Done
    );
    let state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
    let task_run = state.task_run.unwrap();
    assert_eq!(task_run.acceptance_criteria, task.acceptance_criteria);
    assert!(task_run
        .current_plan
        .iter()
        .any(|item| item.contains("create the implementation")));
    assert!(task_run
        .changed_files
        .iter()
        .any(|path| path.ends_with("module.rs")));
    assert!(task_run
        .changed_files
        .iter()
        .any(|path| path.ends_with("tests/module.rs")));
    assert!(workspace.join("src/module.rs").exists());
    assert!(workspace.join("tests/module.rs").exists());
}

#[test]
fn long_runs_keep_the_goal_and_plan_across_compaction_at_ten_and_one_hundred_turns() {
    assert!(AgentLimits::default().max_turns >= 100);
    for expected_turns in [10_u32, 100_u32] {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path();
        let tool_turns = expected_turns - 1;
        let mut responses = Vec::with_capacity(expected_turns as usize);
        for index in 0..tool_turns {
            let path = format!("fixture-{index:03}.txt");
            std::fs::write(workspace.join(&path), format!("fixture {index}\n")).unwrap();
            let text = if index == 0 {
                "Plan:\n- Read the requested fixture files\n- Summarize the findings"
            } else {
                ""
            };
            responses.push(response_many(
                text,
                vec![("read_file", serde_json::json!({ "path": path }))],
            ));
        }
        responses.push(response(
            "All fixture files were read and summarized.",
            None,
        ));

        let requests = Arc::new(Mutex::new(Vec::new()));
        let provider = Arc::new(RecordingProvider::new(requests.clone(), responses));
        let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
        let runner = AgentRunner::new(
            provider,
            "recording",
            ToolRegistry::with_workspace_tools(),
            Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace)),
            sessions.clone(),
            ContextBuilder::default(),
            AgentLimits::default(),
            Arc::new(ApproveAll),
        )
        .with_compaction_config(CompactionConfig {
            threshold_tokens: 1,
            keep_recent_messages: 2,
        });
        let goal = format!(
            "Inspect {tool_turns} fixture files and summarize them.\nConstraints:\n- Read only.\nAcceptance criteria:\n- Read every fixture file\n- Preserve the findings across compaction"
        );
        let objective = format!("Inspect {tool_turns} fixture files and summarize them.");
        let task = AgentTask {
            user_task: goal.clone(),
            acceptance_criteria: vec![
                "Read every fixture file".to_owned(),
                "Preserve the findings across compaction".to_owned(),
            ],
            ..task(workspace)
        };

        let outcome = runner.run(&task, &CancellationToken::new()).unwrap();
        assert_eq!(outcome.turns, expected_turns);
        assert_eq!(
            outcome.completion_status,
            harness_session::TaskCompletionStatus::Done
        );
        let state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
        assert!(state.context_compactions > 0);
        let task_run = state.task_run.unwrap();
        assert_eq!(task_run.goal.objective, objective);
        assert_eq!(task_run.goal.constraints, ["Read only."]);
        assert_eq!(task_run.goal.acceptance_criteria, task.acceptance_criteria);
        let plan = task_run.execution_plan.unwrap();
        assert_eq!(
            plan.revision, 1,
            "ordinary repeated plans must not rewrite state"
        );
        assert_eq!(plan.status, harness_session::PlanItemStatus::Completed);
        assert!(state.continuation.as_ref().is_some_and(|state| {
            state
                .goal
                .as_ref()
                .is_some_and(|goal| goal.objective == objective)
                && state.execution_plan.is_some()
        }));
        let requests = requests.lock().unwrap();
        let compacted_prompt = requests
            .iter()
            .skip(1)
            .flat_map(|request| &request.messages)
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                harness_models::ContentBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .find(|text| text.contains("Persistent task state"))
            .expect("later turns should receive the persisted task state");
        assert!(compacted_prompt.contains(&goal));
        assert!(compacted_prompt.contains("Execution plan revision 1"));
    }
}

#[test]
fn cancelled_task_resumes_from_its_persisted_goal_after_a_process_restart() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let first = Arc::new(ScriptedMockProvider::new("interrupted", Vec::new()));
    let first_runner = AgentRunner::new(
        first,
        "interrupted",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        sessions.clone(),
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    );
    let original_goal = "Implement a multi-step change.\nConstraints:\n- Keep the public API stable\nAcceptance criteria:\n- Verify behavior";
    let interrupted_task = AgentTask {
        user_task: original_goal.to_owned(),
        acceptance_criteria: vec!["Verify behavior".to_owned()],
        ..task(workspace)
    };
    let cancellation = CancellationToken::new();
    cancellation.cancel();
    assert!(first_runner.run(&interrupted_task, &cancellation).is_err());
    let session_id = sessions.recent(1).unwrap().into_iter().next().unwrap().id;
    let persisted = sessions.load(&session_id).unwrap().state().unwrap();
    assert_eq!(
        persisted.task_run.as_ref().unwrap().goal.objective,
        "Implement a multi-step change."
    );
    assert_eq!(
        persisted.task_run.as_ref().unwrap().completion_status,
        harness_session::TaskCompletionStatus::Cancelled
    );

    // A new provider/runner models a new CLI or desktop process. The session
    // history, not the original in-memory run, restores the objective.
    let requests = Arc::new(Mutex::new(Vec::new()));
    let resumed_provider = Arc::new(RecordingProvider::new(
        requests.clone(),
        vec![response(
            "The saved goal is complete after validation.",
            None,
        )],
    ));
    let resumed_runner = AgentRunner::new(
        resumed_provider,
        "recording",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        sessions.clone(),
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    );
    let resumed = AgentTask {
        workspace_root: workspace.to_path_buf(),
        user_task: "Continue from the saved task.".to_owned(),
        resume_session: Some(session_id.clone()),
        ..AgentTask::default()
    };
    let outcome = resumed_runner
        .run(&resumed, &CancellationToken::new())
        .unwrap();
    assert_eq!(outcome.session_id, session_id);
    let prompt = requests.lock().unwrap()[0]
        .messages
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            harness_models::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(prompt.contains(original_goal));
    let resumed_state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
    assert_eq!(
        resumed_state.task_run.unwrap().goal.objective,
        "Implement a multi-step change."
    );
}

#[test]
fn blocked_task_marks_its_active_milestone_as_blocked() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response(
                "Plan:\n- Inspect the workspace\n- Apply the repair",
                Some((
                    "shell",
                    serde_json::json!({ "command": "harness_missing_goal_fixture_command" }),
                )),
            ),
            response("[BLOCKED] The required command is unavailable.", None),
        ],
    ));
    let (runner, sessions) = runner(
        provider,
        workspace,
        Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace)),
        Arc::new(ApproveAll),
    );

    let outcome = runner
        .run(&task(workspace), &CancellationToken::new())
        .unwrap();
    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Blocked
    );
    let task_run = sessions
        .load(&outcome.session_id)
        .unwrap()
        .state()
        .unwrap()
        .task_run
        .unwrap();
    let plan = task_run.execution_plan.unwrap();
    assert_eq!(plan.status, harness_session::PlanItemStatus::Blocked);
    assert_eq!(
        plan.milestones[0].status,
        harness_session::PlanItemStatus::Blocked
    );
}

#[test]
fn failed_command_is_reported_to_model_and_can_be_repaired() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let missing_command = "harness_missing_dependency_for_deterministic_test_6d91";
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            response(
                "",
                Some(("shell", serde_json::json!({ "command": missing_command }))),
            ),
            response(
                "The command was unavailable; using a portable check.",
                Some(("shell", serde_json::json!({ "command": "echo recovered" }))),
            ),
            response("The task is complete after the fallback check.", None),
        ],
    ));
    let policy = Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace));
    let (runner, sessions) = runner(provider, workspace, policy, Arc::new(ApproveAll));
    let outcome = runner
        .run(&task(workspace), &CancellationToken::new())
        .unwrap();

    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Done
    );
    let state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
    let task_run = state.task_run.unwrap();
    assert!(task_run.unresolved_errors.is_empty());
    assert!(task_run
        .commands_executed
        .iter()
        .any(|command| command == missing_command));
    assert!(task_run
        .commands_executed
        .iter()
        .any(|command| command == "echo recovered"));
}

#[test]
fn impossible_and_clarification_tasks_do_not_claim_success() {
    for (text, expected) in [
        (
            "[BLOCKED] The requested dependency is not available and no equivalent exists.",
            harness_session::TaskCompletionStatus::Blocked,
        ),
        (
            "[USER_INPUT_REQUIRED] Should the migration preserve the legacy endpoint?",
            harness_session::TaskCompletionStatus::UserInputRequired,
        ),
    ] {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path();
        let provider = Arc::new(ScriptedMockProvider::new(
            "scripted",
            vec![response(text, None)],
        ));
        let (runner, sessions) = runner(
            provider,
            workspace,
            Arc::new(AllowAllPolicy),
            Arc::new(ApproveAll),
        );
        let outcome = runner
            .run(&task(workspace), &CancellationToken::new())
            .unwrap();
        assert_eq!(outcome.completion_status, expected);
        assert_ne!(
            outcome.completion_status,
            harness_session::TaskCompletionStatus::Done
        );
        assert!(outcome.final_message.contains(
            if expected == harness_session::TaskCompletionStatus::Blocked {
                "requested dependency"
            } else {
                "legacy endpoint"
            }
        ));
        let state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
        assert_eq!(state.task_run.unwrap().completion_status, expected);
    }
}

#[test]
fn repeated_identical_command_failure_is_stopped_as_blocked() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let command = "harness_missing_dependency_for_repeat_guard_331a";
    let failure_response = || {
        response(
            "",
            Some(("shell", serde_json::json!({ "command": command }))),
        )
    };
    let provider = Arc::new(ScriptedMockProvider::new(
        "scripted",
        vec![
            failure_response(),
            failure_response(),
            response("should not finish", None),
        ],
    ));
    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let runner = AgentRunner::new(
        provider,
        "scripted",
        ToolRegistry::with_workspace_tools(),
        Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace)),
        sessions.clone(),
        ContextBuilder::default(),
        AgentLimits {
            max_repeated_tool_calls: 8,
            max_repeated_failures: 2,
            ..AgentLimits::default()
        },
        Arc::new(ApproveAll),
    );

    let outcome = runner
        .run(&task(workspace), &CancellationToken::new())
        .unwrap();

    assert_eq!(
        outcome.completion_status,
        harness_session::TaskCompletionStatus::Blocked
    );
    assert!(outcome.final_message.starts_with("Blocked:"));
    let state = sessions.load(&outcome.session_id).unwrap().state().unwrap();
    assert_eq!(
        state.task_run.unwrap().completion_status,
        harness_session::TaskCompletionStatus::Blocked
    );
}

#[test]
fn long_mock_session_compacts_resumes_and_preserves_history() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    let sessions = Arc::new(JsonlSessionStore::new(temporary.path().join("sessions")).unwrap());
    let session_store: Arc<dyn SessionStore> = sessions.clone();
    let first_requests = Arc::new(Mutex::new(Vec::new()));
    let first_provider = Arc::new(RecordingProvider::new(
        Arc::clone(&first_requests),
        vec![response("first run summary", None)],
    ));
    let first_task = AgentTask {
        workspace_root: workspace.to_path_buf(),
        user_task: "Implement durable context compaction and resume".to_owned(),
        ..task(workspace)
    };
    let first_runner = AgentRunner::new(
        first_provider,
        "recording",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        Arc::clone(&session_store),
        ContextBuilder::new(ContextBudget {
            max_working_context_tokens: 1024,
            ..ContextBudget::default()
        }),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    )
    .with_compaction_config(CompactionConfig {
        threshold_tokens: 1,
        keep_recent_messages: 1,
    });

    let first_outcome = first_runner
        .run(&first_task, &CancellationToken::new())
        .unwrap();
    let after_first = sessions.load(&first_outcome.session_id).unwrap();
    let first_compaction = after_first
        .events
        .iter()
        .find_map(|event| match &event.payload {
            harness_session::EventPayload::ContextCompacted { state, .. } => Some(state),
            _ => None,
        })
        .expect("compaction event");
    assert_eq!(
        first_compaction.task,
        "Implement durable context compaction and resume"
    );
    let original_event_count = after_first.events.len();

    let second_requests = Arc::new(Mutex::new(Vec::new()));
    let second_provider = Arc::new(RecordingProvider::new(
        Arc::clone(&second_requests),
        vec![response("resumed run summary", None)],
    ));
    let second_task = AgentTask {
        workspace_root: workspace.to_path_buf(),
        user_task: "Continue the remaining verification work".to_owned(),
        resume_session: Some(first_outcome.session_id.clone()),
        ..task(workspace)
    };
    let second_runner = AgentRunner::new(
        second_provider,
        "recording",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        Arc::clone(&session_store),
        ContextBuilder::new(ContextBudget {
            max_working_context_tokens: 1024,
            ..ContextBudget::default()
        }),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    )
    .with_compaction_config(CompactionConfig {
        threshold_tokens: 1,
        keep_recent_messages: 1,
    });
    let second_outcome = second_runner
        .run(&second_task, &CancellationToken::new())
        .unwrap();

    let prompt = second_requests.lock().expect("model request lock poisoned")[0].messages[0]
        .content[0]
        .clone();
    let text = match prompt {
        harness_models::ContentBlock::Text { text } => text,
        _ => panic!("expected text model prompt"),
    };
    assert!(text.contains("Compacted working state"));
    assert!(text.contains("first run summary"));

    let resumed = sessions.load(&second_outcome.session_id).unwrap();
    assert!(resumed.events.len() > original_event_count);
    assert!(resumed
        .events
        .iter()
        .any(|event| matches!(event.payload, harness_session::EventPayload::UserMessage { ref text } if text == "Implement durable context compaction and resume")));
    assert!(resumed.events.iter().any(|event| {
        matches!(
            event.payload,
            harness_session::EventPayload::SessionResumed { .. }
        )
    }));
    assert_eq!(resumed.state().unwrap().context_compactions, 2);
}

#[test]
fn openai_mock_tool_use_round_trip_executes_and_returns_tool_observation() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(workspace.join("readme.txt"), "agent-visible-file-content").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock OpenAI server");
    let address = listener.local_addr().expect("mock OpenAI address");
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let server_requests = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept Responses request");
            let headers_and_body = read_http_request_for_response(&mut stream);
            let events = if turn == 0 {
                vec![
                    serde_json::json!({ "type": "response.created", "response": { "id": "resp-tool" } }),
                    serde_json::json!({
                        "type": "response.output_item.added",
                        "output_index": 0,
                        "item": { "type": "function_call", "id": "fc-read", "call_id": "call-read", "name": "read_file", "arguments": "" }
                    }),
                    serde_json::json!({ "type": "response.function_call_arguments.delta", "output_index": 0, "delta": "{\"path\":\"readme.txt\"}" }),
                    serde_json::json!({
                        "type": "response.output_item.done",
                        "output_index": 0,
                        "item": { "type": "function_call", "id": "fc-read", "call_id": "call-read", "name": "read_file", "arguments": "{\"path\":\"readme.txt\"}" }
                    }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp-tool", "model": "gpt-test", "status": "completed",
                            "output": [{ "type": "function_call", "id": "fc-read", "call_id": "call-read", "name": "read_file", "arguments": "{\"path\":\"readme.txt\"}" }],
                            "usage": { "input_tokens": 20, "output_tokens": 5, "total_tokens": 25 }
                        }
                    }),
                ]
            } else {
                vec![
                    serde_json::json!({ "type": "response.created", "response": { "id": "resp-final" } }),
                    serde_json::json!({ "type": "response.output_text.delta", "output_index": 0, "delta": "The file contains agent-visible-file-content." }),
                    serde_json::json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp-final", "model": "gpt-test", "status": "completed",
                            "output": [{ "type": "message", "content": [{ "type": "output_text", "text": "The file contains agent-visible-file-content." }] }],
                            "usage": { "input_tokens": 45, "output_tokens": 10, "total_tokens": 55 }
                        }
                    }),
                ]
            };
            server_requests
                .lock()
                .expect("OpenAI requests lock")
                .push(headers_and_body);
            respond_with_sse(&mut stream, &events);
        }
    });

    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let session_store: Arc<dyn SessionStore> = sessions.clone();
    let provider = Arc::new(OpenAIProvider::with_api_key(
        format!("http://{address}/v1"),
        "gpt-test",
        "MOCK_OPENAI_KEY",
        "agent-test-key",
    ));
    let runner = AgentRunner::new(
        provider,
        "gpt-test",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        session_store,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    );

    let outcome = runner
        .run(
            &AgentTask {
                workspace_root: workspace.to_path_buf(),
                user_task: "Read readme.txt and tell me what it contains".to_owned(),
                ..AgentTask::default()
            },
            &CancellationToken::new(),
        )
        .unwrap();
    server.join().expect("mock OpenAI server");

    assert_eq!(outcome.tool_calls, 1);
    assert_eq!(outcome.turns, 2);
    assert_eq!(
        outcome.final_message,
        "The file contains agent-visible-file-content."
    );
    let session = sessions.load(&outcome.session_id).unwrap();
    assert!(!serde_json::to_string(&session.events)
        .unwrap()
        .contains("agent-test-key"));
    let requests = requests.lock().expect("OpenAI requests lock");
    assert_eq!(requests.len(), 2);
    let follow_up = requests[1]["body"]["input"]
        .as_array()
        .unwrap()
        .last()
        .unwrap();
    assert_eq!(follow_up["type"], "function_call_output");
    assert!(follow_up["output"]
        .as_str()
        .expect("tool output is text")
        .contains("agent-visible-file-content"));
    assert_eq!(requests[0]["body"]["tools"][0]["name"], "read_file");
}

#[test]
fn anthropic_messages_tool_use_runs_through_policy_and_workspace_tool() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(
        workspace.join("readme.txt"),
        "anthropic-agent-visible-content",
    )
    .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Anthropic server");
    let address = listener.local_addr().expect("mock server address");
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let server_requests = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        for turn in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept Messages request");
            let captured = read_http_request_for_response(&mut stream);
            server_requests
                .lock()
                .expect("Anthropic requests lock")
                .push(captured);
            let events = if turn == 0 {
                vec![
                    serde_json::json!({
                        "type": "message_start",
                        "message": {
                            "id": "msg-tool",
                            "type": "message",
                            "role": "assistant",
                            "model": "claude-sonnet-4-6",
                            "content": [],
                            "stop_reason": null,
                            "usage": { "input_tokens": 32, "output_tokens": 1 }
                        }
                    }),
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "tool_use", "id": "toolu-readme", "name": "read_file", "input": {} }
                    }),
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "input_json_delta", "partial_json": "{\"path\":\"readme.txt\"}" }
                    }),
                    serde_json::json!({ "type": "content_block_stop", "index": 0 }),
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": "tool_use" },
                        "usage": { "output_tokens": 6 }
                    }),
                    serde_json::json!({ "type": "message_stop" }),
                ]
            } else {
                vec![
                    serde_json::json!({
                        "type": "message_start",
                        "message": {
                            "id": "msg-final",
                            "type": "message",
                            "role": "assistant",
                            "model": "claude-sonnet-4-6",
                            "content": [],
                            "stop_reason": null,
                            "usage": { "input_tokens": 48, "output_tokens": 1 }
                        }
                    }),
                    serde_json::json!({
                        "type": "content_block_start",
                        "index": 0,
                        "content_block": { "type": "text", "text": "" }
                    }),
                    serde_json::json!({
                        "type": "content_block_delta",
                        "index": 0,
                        "delta": { "type": "text_delta", "text": "The file contains anthropic-agent-visible-content." }
                    }),
                    serde_json::json!({ "type": "content_block_stop", "index": 0 }),
                    serde_json::json!({
                        "type": "message_delta",
                        "delta": { "stop_reason": "end_turn" },
                        "usage": { "output_tokens": 11 }
                    }),
                    serde_json::json!({ "type": "message_stop" }),
                ]
            };
            respond_with_sse(&mut stream, &events);
        }
    });

    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let session_store: Arc<dyn SessionStore> = sessions.clone();
    let provider = Arc::new(AnthropicProvider::with_api_key(
        format!("http://{address}/v1"),
        "claude-sonnet-4-6",
        "MOCK_ANTHROPIC_KEY",
        "anthropic-agent-test-key",
    ));
    let runner = AgentRunner::new(
        provider,
        "claude-sonnet-4-6",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        session_store,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    );

    let outcome = runner
        .run(
            &AgentTask {
                workspace_root: workspace.to_path_buf(),
                user_task: "Read readme.txt and report its contents".to_owned(),
                ..AgentTask::default()
            },
            &CancellationToken::new(),
        )
        .unwrap();
    server.join().expect("mock Anthropic server");

    assert_eq!(outcome.tool_calls, 1);
    assert_eq!(outcome.turns, 2);
    assert_eq!(
        outcome.final_message,
        "The file contains anthropic-agent-visible-content."
    );
    let session = sessions.load(&outcome.session_id).unwrap();
    assert!(!serde_json::to_string(&session.events)
        .unwrap()
        .contains("anthropic-agent-test-key"));
    let requests = requests.lock().expect("Anthropic requests lock");
    assert_eq!(requests.len(), 2);
    assert!(requests[0]["headers"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase()
        .contains("anthropic-version: 2023-06-01"));
    assert_eq!(requests[0]["body"]["tools"][0]["name"], "read_file");
    assert_eq!(requests[0]["body"]["stream"], true);
    let follow_up = requests[1]["body"]["messages"].as_array().unwrap();
    assert_eq!(follow_up.len(), 3);
    assert_eq!(follow_up[2]["role"], "user");
    assert_eq!(follow_up[2]["content"][0]["type"], "tool_result");
    assert!(follow_up[2]["content"][0]["content"]
        .as_str()
        .unwrap()
        .contains("anthropic-agent-visible-content"));
}

#[test]
fn gemini_native_flow_reads_patches_and_returns_tool_results_to_the_model() {
    let temporary = tempdir().unwrap();
    let workspace = temporary.path();
    std::fs::write(workspace.join("readme.txt"), "before\n").unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Gemini server");
    let address = listener.local_addr().expect("mock Gemini address");
    let requests = Arc::new(Mutex::new(Vec::<serde_json::Value>::new()));
    let server_requests = Arc::clone(&requests);
    let server = std::thread::spawn(move || {
        for turn in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept Generate Content request");
            let captured = read_http_request_for_response(&mut stream);
            server_requests
                .lock()
                .expect("Gemini requests lock")
                .push(captured);
            let event = match turn {
                0 => serde_json::json!({
                    "responseId": "gemini-read-response",
                    "candidates": [{ "content": { "parts": [
                        { "functionCall": { "id": "gemini-read-call", "name": "read_file", "args": { "path": "readme.txt" } }, "thoughtSignature": "private-read-signature" }
                    ] }, "finishReason": "STOP" }],
                    "usageMetadata": { "promptTokenCount": 30, "candidatesTokenCount": 8, "totalTokenCount": 38 }
                }),
                1 => serde_json::json!({
                    "responseId": "gemini-patch-response",
                    "candidates": [{ "content": { "parts": [
                        { "functionCall": { "id": "gemini-patch-call", "name": "apply_patch", "args": { "path": "readme.txt", "old_text": "before\n", "new_text": "after\n" } }, "thoughtSignature": "private-patch-signature" }
                    ] }, "finishReason": "STOP" }],
                    "usageMetadata": { "promptTokenCount": 44, "candidatesTokenCount": 10, "totalTokenCount": 54 }
                }),
                _ => serde_json::json!({
                    "responseId": "gemini-final-response",
                    "candidates": [{ "content": { "parts": [{ "text": "Updated readme.txt from before to after." }] }, "finishReason": "STOP" }],
                    "usageMetadata": { "promptTokenCount": 58, "candidatesTokenCount": 9, "totalTokenCount": 67 }
                }),
            };
            respond_with_sse(&mut stream, &[event]);
        }
    });

    let sessions = Arc::new(JsonlSessionStore::new(workspace.join("sessions")).unwrap());
    let session_store: Arc<dyn SessionStore> = sessions.clone();
    let provider = Arc::new(GeminiProvider::with_api_key(
        format!("http://{address}/v1beta"),
        "gemini-3.8-flash",
        "MOCK_GEMINI_KEY",
        "gemini-agent-test-secret",
    ));
    let runner = AgentRunner::new(
        provider,
        "gemini-3.8-flash",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        session_store,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(ApproveAll),
    )
    .with_verifier(Arc::new(ScriptedVerifier::new([true])));

    let outcome = runner
        .run(
            &AgentTask {
                workspace_root: workspace.to_path_buf(),
                user_task: "Read readme.txt, change before to after, and report the result"
                    .to_owned(),
                verification_plan: Some(mock_verification_plan()),
                ..AgentTask::default()
            },
            &CancellationToken::new(),
        )
        .unwrap();
    server.join().expect("mock Gemini server");

    assert_eq!(outcome.turns, 3);
    assert_eq!(outcome.tool_calls, 2);
    assert_eq!(
        outcome.final_message,
        "Updated readme.txt from before to after."
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("readme.txt")).unwrap(),
        "after\n"
    );
    let session = sessions.load(&outcome.session_id).unwrap();
    let session_json = serde_json::to_string(&session.events).unwrap();
    assert!(!session_json.contains("gemini-agent-test-secret"));
    assert!(!session_json.contains("private-read-signature"));
    assert!(!session_json.contains("private-patch-signature"));

    let requests = requests.lock().expect("Gemini requests lock");
    assert_eq!(requests.len(), 3);
    assert!(requests[0]["headers"]
        .as_str()
        .unwrap()
        .to_ascii_lowercase()
        .contains("x-goog-api-key: gemini-agent-test-secret"));
    let second_contents = requests[1]["body"]["contents"].as_array().unwrap();
    assert_eq!(second_contents[1]["role"], "model");
    assert_eq!(
        second_contents[1]["parts"][0]["thoughtSignature"],
        "private-read-signature"
    );
    assert_eq!(second_contents[2]["role"], "function");
    assert_eq!(
        second_contents[2]["parts"][0]["functionResponse"]["name"],
        "read_file"
    );
    let third_contents = requests[2]["body"]["contents"].as_array().unwrap();
    assert_eq!(third_contents.len(), 3);
    assert_eq!(third_contents[1]["role"], "model");
    assert_eq!(
        third_contents[1]["parts"][0]["thoughtSignature"],
        "private-patch-signature"
    );
    assert_eq!(third_contents[2]["role"], "function");
    assert_eq!(
        third_contents[2]["parts"][0]["functionResponse"]["name"],
        "apply_patch"
    );
    assert!(
        !third_contents[2]["parts"][0]["functionResponse"]["response"]["result"]
            .as_str()
            .unwrap()
            .is_empty()
    );
}

fn read_http_request_for_response(stream: &mut TcpStream) -> serde_json::Value {
    let (headers, body) = read_http_request(stream);
    serde_json::json!({
        "headers": headers,
        "body": serde_json::from_str::<serde_json::Value>(&body).expect("Responses request JSON"),
    })
}
