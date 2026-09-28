use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
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
use harness_session::{JsonlSessionStore, SessionStatus, SessionStore};
use harness_tools::{CancellationToken, LocalProcessRunner, ToolRegistry};
use harness_verification::{
    CommandVerifier, VerificationCategory, VerificationPlan, VerificationStep,
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
    );

    let outcome = runner
        .run(
            &AgentTask {
                workspace_root: workspace.to_path_buf(),
                user_task: "Read readme.txt, change before to after, and report the result"
                    .to_owned(),
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
