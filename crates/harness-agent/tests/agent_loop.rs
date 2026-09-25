use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use harness_agent::{
    AgentLimits, AgentRunner, AgentTask, ApprovalHandler, CompactionConfig, DenyApprovalHandler,
};
use harness_context::{ContextBudget, ContextBuilder, WorkspaceMetadata};
use harness_models::{
    ContentBlock, ModelCapabilities, ModelProvider, ModelRequest, ModelResponse, ProviderError,
    ScriptedMockProvider, StreamDelta, StreamDeltaKind, ToolCall, Usage,
};
use harness_policy::{AllowAllPolicy, DenyAllPolicy, ExecutionMode, PolicyEngine};
use harness_session::{JsonlSessionStore, SessionStatus, SessionStore};
use harness_tools::{CancellationToken, LocalProcessRunner, ToolRegistry};
use harness_verification::{
    CommandVerifier, VerificationCategory, VerificationPlan, VerificationStep,
};
use tempfile::tempdir;

fn response(text: &str, tool: Option<(&str, serde_json::Value)>) -> ModelResponse {
    let has_tool = tool.is_some();
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
        tool_calls: tool
            .map(|(name, arguments)| ToolCall {
                id: format!("call-{name}"),
                name: name.to_owned(),
                arguments,
            })
            .into_iter()
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
    fn name(&self) -> &str {
        "recording"
    }

    fn capabilities(&self) -> ModelCapabilities {
        ModelCapabilities {
            streaming: true,
            tool_calling: true,
            vision: false,
            reasoning: false,
            context_window: Some(4096),
        }
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.response(request)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_delta: &mut dyn FnMut(StreamDelta) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.response(request)?;
        on_delta(StreamDelta {
            sequence: 0,
            delta: StreamDeltaKind::Text {
                text: response.text(),
            },
            finish_reason: Some(response.finish_reason.clone()),
            usage: response.usage.clone(),
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

    let outcome = runner
        .run(&task(workspace), &CancellationToken::new())
        .unwrap();

    assert_eq!(outcome.final_message, "All done");
    assert_eq!(outcome.turns, 4);
    assert_eq!(outcome.tool_calls, 3);
    assert_eq!(
        std::fs::read_to_string(workspace.join("file.txt")).unwrap(),
        "after\n"
    );
    let session = sessions.load(&outcome.session_id).unwrap();
    assert_eq!(session.state().unwrap().status, SessionStatus::Completed);
    let event_types = session
        .events
        .iter()
        .map(|event| event.event_type)
        .collect::<Vec<_>>();
    assert!(event_types.contains(&harness_session::EventType::ToolRequested));
    assert!(event_types.contains(&harness_session::EventType::PolicyDecision));
    assert!(event_types.contains(&harness_session::EventType::FileChanged));
    assert!(event_types.contains(&harness_session::EventType::ProcessExited));
    assert_eq!(
        event_types.last(),
        Some(&harness_session::EventType::SessionCompleted)
    );
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
fn verification_failure_is_persisted_and_agent_continues() {
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
                    serde_json::json!({ "path": "file.txt", "content": "after" }),
                )),
            ),
            response("Finished after verification", None),
        ],
    ));
    let policy = Arc::new(PolicyEngine::new(ExecutionMode::Normal, workspace));
    let (runner, sessions) = runner(provider, workspace, policy, Arc::new(ApproveAll));
    let failing_command = if cfg!(windows) {
        "echo verification failure & exit /B 7"
    } else {
        "echo 'verification failure'; exit 7"
    };
    let plan = VerificationPlan {
        steps: vec![VerificationStep {
            category: VerificationCategory::Build,
            command: if cfg!(windows) {
                harness_core::CommandSpec::new("cmd", ["/C", failing_command])
            } else {
                harness_core::CommandSpec::new("sh", ["-c", failing_command])
            },
            source: "test".to_owned(),
        }],
    };
    let mut task = task(workspace);
    task.verification_plan = Some(plan);
    let runner = runner.with_verifier(Arc::new(CommandVerifier::new(Arc::new(LocalProcessRunner))));

    let outcome = runner.run(&task, &CancellationToken::new()).unwrap();

    assert_eq!(outcome.final_message, "Finished after verification");
    let session = sessions.load(&outcome.session_id).unwrap();
    assert!(session.events.iter().any(|event| {
        event.event_type == harness_session::EventType::VerificationResult
            && matches!(&event.payload, harness_session::EventPayload::VerificationResult { output, .. } if output.contains("verification failure"))
    }));
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
