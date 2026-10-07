use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use harness_agent::{AgentLimits, AgentRunner, AgentTask, DenyApprovalHandler, SubagentConfig};
use harness_context::{ContextBuilder, WorkspaceMetadata};
use harness_models::{
    ContentBlock, FinishReason, ModelCapabilities, ModelDescriptor, ModelProvider, ModelRequest,
    ModelResponse, ModelStreamEvent, ProviderError, Role, ToolCall, Usage,
};
use harness_policy::AllowAllPolicy;
use harness_session::{
    EventBus, EventPayload, HarnessEvent, JsonlSessionStore, SessionStore, TaskCompletionStatus,
    TaskMode,
};
use harness_tools::{CancellationToken, ToolRegistry};
use serde_json::{json, Value};

const PARENT_HISTORY_SENTINEL: &str = "PARENT_HISTORY_MUST_STAY_PRIVATE_TO_CHILD";

#[derive(Clone, Copy)]
enum ChildBehavior {
    Report,
    Fail,
    Delay,
    TryWrite,
    ReadUntilBudget,
}

struct DelegationProvider {
    tasks: Value,
    behavior: ChildBehavior,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    active_children: Arc<AtomicUsize>,
    max_active_children: Arc<AtomicUsize>,
}

impl DelegationProvider {
    fn new(
        tasks: Value,
        behavior: ChildBehavior,
    ) -> (Self, Arc<Mutex<Vec<ModelRequest>>>, Arc<AtomicUsize>) {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let active_children = Arc::new(AtomicUsize::new(0));
        let max_active_children = Arc::new(AtomicUsize::new(0));
        (
            Self {
                tasks,
                behavior,
                requests: Arc::clone(&requests),
                active_children,
                max_active_children: Arc::clone(&max_active_children),
            },
            requests,
            max_active_children,
        )
    }

    fn respond(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.requests
            .lock()
            .expect("request record lock poisoned")
            .push(request.clone());
        let is_parent = request
            .tools
            .iter()
            .any(|tool| tool.name == "delegate_subagents");
        let has_tool_result = request
            .messages
            .iter()
            .any(|message| message.role == Role::Tool);
        let response = if is_parent && !has_tool_result {
            response_tool(
                "delegate_subagents",
                self.tasks.clone(),
                "parallel delegation request",
            )
        } else if is_parent {
            response_text("Parent reviewed the child reports.")
        } else if has_tool_result {
            response_text(
                r#"{"summary":"Inspected the requested code.","findings":["Found the relevant implementation."],"relevant_files":["src/lib.rs"],"evidence":["The read-only source inspection identifies the call path."],"recommended_next_action":"Review the result in the parent task."}"#,
            )
        } else {
            match self.behavior {
                ChildBehavior::Report => response_text(
                    r#"{"summary":"Inspected the requested code.","findings":["Found the relevant implementation."],"relevant_files":["src/lib.rs"],"evidence":["The read-only source inspection identifies the call path."],"recommended_next_action":"Review the result in the parent task."}"#,
                ),
                ChildBehavior::Fail => {
                    return Err(ProviderError::Transport {
                        provider: "deterministic-child",
                    });
                }
                ChildBehavior::Delay => response_text("should be cancelled by the child timeout"),
                ChildBehavior::TryWrite => response_tool(
                    "write_file",
                    json!({"path":"child-write.txt","content":"must not be written"}),
                    "attempt a forbidden edit",
                ),
                ChildBehavior::ReadUntilBudget => {
                    response_tool("read_file", json!({"path":"src/lib.rs"}), "read a file")
                }
            }
        };
        Ok(response)
    }

    fn stream_response(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        let response = self.respond(request)?;
        on_event(ModelStreamEvent::ResponseStarted {
            id: Some(response.id.clone()),
            model: response.model.clone(),
        })?;
        let text = response.text();
        if !text.is_empty() {
            on_event(ModelStreamEvent::TextDelta { text })?;
        }
        for (index, call) in response.tool_calls.iter().enumerate() {
            on_event(ModelStreamEvent::ToolCallStarted {
                index: index as u32,
                id: Some(call.id.clone()),
                name: Some(call.name.clone()),
            })?;
            on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                index: index as u32,
                delta: call.arguments.to_string(),
            })?;
            on_event(ModelStreamEvent::ToolCallCompleted {
                index: index as u32,
                call: call.clone(),
            })?;
        }
        on_event(ModelStreamEvent::ResponseCompleted {
            finish_reason: response.finish_reason.clone(),
        })?;
        Ok(response)
    }
}

impl ModelProvider for DelegationProvider {
    fn descriptor(&self) -> ModelDescriptor {
        ModelDescriptor {
            provider: "deterministic".to_owned(),
            id: "delegation-test".to_owned(),
            display_name: "Delegation test model".to_owned(),
            capabilities: ModelCapabilities {
                text_input: true,
                streaming: true,
                tool_calling: true,
                system_instructions: true,
                context_window: Some(16_384),
                ..ModelCapabilities::default()
            },
            metadata: Default::default(),
        }
    }

    fn complete(&self, request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
        self.respond(request)
    }

    fn stream(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        self.stream_response(request, on_event)
    }

    fn stream_cancellable(
        &self,
        request: &ModelRequest,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<ModelResponse, ProviderError> {
        self.requests
            .lock()
            .expect("request record lock poisoned")
            .push(request.clone());
        let is_parent = request
            .tools
            .iter()
            .any(|tool| tool.name == "delegate_subagents");
        if !is_parent && matches!(self.behavior, ChildBehavior::Delay) {
            let active = self.active_children.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active_children.fetch_max(active, Ordering::AcqRel);
            let deadline = Instant::now() + Duration::from_millis(500);
            while Instant::now() < deadline {
                if is_cancelled() {
                    self.active_children.fetch_sub(1, Ordering::AcqRel);
                    return Err(ProviderError::Cancelled);
                }
                thread::sleep(Duration::from_millis(2));
            }
            self.active_children.fetch_sub(1, Ordering::AcqRel);
            return self.emit_response(response_text("late child response"), on_event);
        }
        // Avoid recording the same request twice in stream_response.
        self.respond_without_recording(request)
            .and_then(|response| self.emit_response(response, on_event))
    }
}

impl DelegationProvider {
    fn respond_without_recording(
        &self,
        request: &ModelRequest,
    ) -> Result<ModelResponse, ProviderError> {
        // `respond` owns the deterministic branching; remove the duplicate
        // recording performed by this cancellation-aware transport first.
        self.requests
            .lock()
            .expect("request record lock poisoned")
            .pop();
        self.respond(request)
    }

    fn emit_response(
        &self,
        response: ModelResponse,
        on_event: &mut dyn FnMut(ModelStreamEvent) -> Result<(), ProviderError>,
    ) -> Result<ModelResponse, ProviderError> {
        on_event(ModelStreamEvent::ResponseStarted {
            id: Some(response.id.clone()),
            model: response.model.clone(),
        })?;
        let text = response.text();
        if !text.is_empty() {
            on_event(ModelStreamEvent::TextDelta { text })?;
        }
        for (index, call) in response.tool_calls.iter().enumerate() {
            on_event(ModelStreamEvent::ToolCallStarted {
                index: index as u32,
                id: Some(call.id.clone()),
                name: Some(call.name.clone()),
            })?;
            on_event(ModelStreamEvent::ToolCallArgumentsDelta {
                index: index as u32,
                delta: call.arguments.to_string(),
            })?;
            on_event(ModelStreamEvent::ToolCallCompleted {
                index: index as u32,
                call: call.clone(),
            })?;
        }
        on_event(ModelStreamEvent::ResponseCompleted {
            finish_reason: response.finish_reason.clone(),
        })?;
        Ok(response)
    }
}

fn response_tool(name: &str, arguments: Value, text: &str) -> ModelResponse {
    ModelResponse {
        id: format!("response-{name}"),
        model: "delegation-test".to_owned(),
        content: if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text {
                text: text.to_owned(),
            }]
        },
        tool_calls: vec![ToolCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments,
        }],
        finish_reason: FinishReason::ToolCalls,
        usage: Some(Usage::new(20, 8)),
    }
}

fn response_text(text: &str) -> ModelResponse {
    ModelResponse {
        id: "response-text".to_owned(),
        model: "delegation-test".to_owned(),
        content: vec![ContentBlock::Text {
            text: text.to_owned(),
        }],
        tool_calls: Vec::new(),
        finish_reason: FinishReason::Stop,
        usage: Some(Usage::new(20, 8)),
    }
}

fn tasks(tasks: Vec<Value>) -> Value {
    json!({"tasks": tasks})
}

fn task(role: &str, task: &str, selected_context: Vec<Value>) -> Value {
    json!({
        "role": role,
        "task": task,
        "selected_context": selected_context
    })
}

struct TestHarness {
    sessions: Arc<JsonlSessionStore>,
    requests: Arc<Mutex<Vec<ModelRequest>>>,
    max_active_children: Arc<AtomicUsize>,
    runner: AgentRunner,
}

fn setup(
    root: &Path,
    tasks: Value,
    behavior: ChildBehavior,
    config: SubagentConfig,
) -> TestHarness {
    let event_bus = EventBus::new();
    let sessions = Arc::new(
        JsonlSessionStore::with_event_bus(root.join(".cogito/sessions"), event_bus.clone())
            .expect("session store"),
    );
    let (provider, requests, max_active_children) = DelegationProvider::new(tasks, behavior);
    let runner = AgentRunner::new(
        Arc::new(provider),
        "delegation-test",
        ToolRegistry::with_workspace_tools(),
        Arc::new(AllowAllPolicy),
        Arc::clone(&sessions) as Arc<dyn SessionStore>,
        ContextBuilder::default(),
        AgentLimits::default(),
        Arc::new(DenyApprovalHandler),
    )
    .with_event_bus(event_bus)
    .with_subagents_config(config);
    TestHarness {
        sessions,
        requests,
        max_active_children,
        runner,
    }
}

fn run_parent(
    root: &Path,
    sessions: &JsonlSessionStore,
    runner: &AgentRunner,
    resume_session: Option<String>,
) -> harness_agent::AgentOutcome {
    let task = AgentTask {
        workspace_root: root.to_path_buf(),
        user_task: "Investigate this bounded task".to_owned(),
        task_mode: TaskMode::Code,
        system_instructions: "Parent task instructions.".to_owned(),
        workspace: WorkspaceMetadata {
            root: Some(root.to_path_buf()),
            ..WorkspaceMetadata::default()
        },
        resume_session: resume_session.map(|id| harness_core::SessionId::new(id).unwrap()),
        ..AgentTask::default()
    };
    let _ = sessions;
    runner
        .run(&task, &CancellationToken::new())
        .expect("parent agent run")
}

fn output_report(session: &harness_session::Session) -> Value {
    let output = session
        .events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::ToolOutput { tool, output } if tool == "delegate_subagents" => {
                Some(output)
            }
            _ => None,
        })
        .expect("delegation output event");
    serde_json::from_str(output).expect("structured delegation output")
}

fn request_text(request: &ModelRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter_map(|block| match block {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn one_read_only_child_returns_structured_findings_and_parent_child_events() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n").unwrap();
    let child_task = task(
        "explore",
        "Find the value function and summarize its behavior.",
        vec![json!({"label":"selected note","content":"The caller expects a small integer."})],
    );
    let harness = setup(
        root,
        tasks(vec![child_task]),
        ChildBehavior::Report,
        SubagentConfig::default(),
    );
    let outcome = run_parent(root, &harness.sessions, &harness.runner, None);
    let parent = harness.sessions.load(&outcome.session_id).unwrap();
    let report_value = output_report(&parent);
    let report = &report_value["results"][0];
    assert_eq!(report["role"], "explore");
    assert_eq!(report["status"], "completed");
    assert_eq!(report["findings"][0], "Found the relevant implementation.");
    assert!(
        report["context_metrics"]["child_context_tokens_sent"]
            .as_u64()
            .unwrap()
            > 0
    );

    let (child_id, correlation) = parent
        .events
        .iter()
        .find_map(|event| match &event.payload {
            EventPayload::SubagentStarted {
                child_session_id,
                delegation_id,
                ..
            } => Some((child_session_id.clone(), delegation_id.clone())),
            _ => None,
        })
        .expect("parent start event");
    assert!(parent.events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::SubagentCompleted {
            child_session_id,
            delegation_id,
            ..
        } if child_session_id == &child_id && delegation_id == &correlation
    )));
    let child = harness.sessions.load(&child_id).unwrap();
    assert!(child.events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::SubagentLinked { parent_session_id, delegation_id, .. }
            if parent_session_id == &outcome.session_id && delegation_id == &correlation
    )));
}

#[test]
fn independent_children_run_in_parallel_and_do_not_receive_parent_history() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let tasks = tasks(vec![
        task(
            "explore",
            "Inspect the repository entry point.",
            vec![json!({"label":"selected snippet","content":"ONLY_THIS_SELECTED_CONTEXT"})],
        ),
        task(
            "documentation",
            "Find documentation that describes the entry point.",
            Vec::new(),
        ),
    ]);
    let harness = setup(root, tasks, ChildBehavior::Delay, SubagentConfig::default());
    let parent_session = harness.sessions.create(root).unwrap();
    for index in 0..24 {
        harness
            .sessions
            .append_event(
                &parent_session.id,
                HarnessEvent::new(
                    parent_session.id.clone(),
                    EventPayload::AssistantMessage {
                        text: format!(
                            "{PARENT_HISTORY_SENTINEL} {}",
                            "historic context ".repeat(300 + index)
                        ),
                    },
                    None,
                    None,
                ),
            )
            .unwrap();
    }
    let outcome = run_parent(
        root,
        &harness.sessions,
        &harness.runner,
        Some(parent_session.id.to_string()),
    );
    assert!(harness.max_active_children.load(Ordering::Acquire) >= 2);
    let parent = harness.sessions.load(&outcome.session_id).unwrap();
    let report_value = output_report(&parent);
    assert_eq!(report_value["results"].as_array().unwrap().len(), 2);

    let requests = harness.requests.lock().unwrap();
    let child_requests = requests
        .iter()
        .filter(|request| {
            !request
                .tools
                .iter()
                .any(|tool| tool.name == "delegate_subagents")
        })
        .collect::<Vec<_>>();
    assert_eq!(child_requests.len(), 2);
    assert!(child_requests
        .iter()
        .all(|request| !request_text(request).contains(PARENT_HISTORY_SENTINEL)));
    assert!(child_requests
        .iter()
        .any(|request| request_text(request).contains("ONLY_THIS_SELECTED_CONTEXT")));
    for request in &child_requests {
        let child_tool_names = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect::<Vec<_>>();
        assert!(child_tool_names.contains(&"read_file"));
        assert!(child_tool_names.contains(&"get_git_diff"));
        assert!(!child_tool_names.iter().any(|name| {
            matches!(
                *name,
                "write_file"
                    | "apply_patch"
                    | "delete_file"
                    | "rename_file"
                    | "run_command"
                    | "shell"
                    | "web_search"
                    | "web_fetch"
                    | "get_diagnostics"
                    | "delegate_subagents"
            )
        }));
    }
    assert!(requests
        .iter()
        .any(|request| request_text(request).contains(PARENT_HISTORY_SENTINEL)));

    let context_metrics = report_value["results"].as_array().unwrap();
    let parent_history_tokens = context_metrics[0]["context_metrics"]
        ["parent_history_estimated_tokens"]
        .as_u64()
        .unwrap();
    let child_tokens = context_metrics
        .iter()
        .map(|result| {
            result["context_metrics"]["child_context_tokens_sent"]
                .as_u64()
                .unwrap()
        })
        .sum::<u64>();
    assert!(parent_history_tokens > child_tokens);
}

#[test]
fn child_provider_failure_is_reported_without_failing_parent_run() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let harness = setup(
        root,
        tasks(vec![task(
            "test",
            "Investigate the failing test output.",
            Vec::new(),
        )]),
        ChildBehavior::Fail,
        SubagentConfig::default(),
    );
    let outcome = run_parent(root, &harness.sessions, &harness.runner, None);
    let parent = harness.sessions.load(&outcome.session_id).unwrap();
    let report = output_report(&parent);
    assert_eq!(report["results"][0]["status"], "failed");
    assert!(parent
        .events
        .iter()
        .any(|event| matches!(event.payload, EventPayload::SubagentFailed { .. })));
}

#[test]
fn child_timeout_is_bounded_and_cancels_the_read_only_worker() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let config = SubagentConfig {
        max_runtime: Duration::from_millis(40),
        ..SubagentConfig::default()
    };
    let harness = setup(
        root,
        tasks(vec![task(
            "explore",
            "Wait for the slow investigation.",
            Vec::new(),
        )]),
        ChildBehavior::Delay,
        config,
    );
    let started = Instant::now();
    let outcome = run_parent(root, &harness.sessions, &harness.runner, None);
    assert!(started.elapsed() < Duration::from_secs(2));
    let parent = harness.sessions.load(&outcome.session_id).unwrap();
    assert_eq!(output_report(&parent)["results"][0]["status"], "timed_out");
}

#[test]
fn child_cannot_write_even_when_the_model_requests_a_write_tool() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    let harness = setup(
        root,
        tasks(vec![task(
            "review",
            "Inspect the implementation.",
            Vec::new(),
        )]),
        ChildBehavior::TryWrite,
        SubagentConfig::default(),
    );
    let outcome = run_parent(root, &harness.sessions, &harness.runner, None);
    assert!(!root.join("child-write.txt").exists());
    let parent = harness.sessions.load(&outcome.session_id).unwrap();
    let report = output_report(&parent);
    let child_id =
        harness_core::SessionId::new(report["results"][0]["child_session_id"].as_str().unwrap())
            .unwrap();
    let child = harness.sessions.load(&child_id).unwrap();
    assert!(child.events.iter().any(|event| matches!(
        &event.payload,
        EventPayload::ToolDenied { tool, reason }
            if tool == "write_file" && reason.contains("read-only")
    )));
    assert_eq!(
        child.state().unwrap().task_run.unwrap().task_mode,
        TaskMode::Explore
    );
}

#[test]
fn child_turn_budget_is_enforced_and_batches_cannot_exceed_parallel_cap() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/lib.rs"), "pub fn test_value() {}\n").unwrap();
    let config = SubagentConfig {
        max_turns: 1,
        ..SubagentConfig::default()
    };
    let harness = setup(
        root,
        tasks(vec![task("test", "Inspect this test source.", Vec::new())]),
        ChildBehavior::ReadUntilBudget,
        config,
    );
    let outcome = run_parent(root, &harness.sessions, &harness.runner, None);
    let parent = harness.sessions.load(&outcome.session_id).unwrap();
    assert_eq!(
        output_report(&parent)["results"][0]["status"],
        "budget_reached"
    );
    assert_eq!(outcome.completion_status, TaskCompletionStatus::Done);

    let invalid = ToolRegistry::with_read_only_workspace_tools();
    assert!(!invalid.names().iter().any(|name| {
        matches!(
            name.as_str(),
            "write_file" | "apply_patch" | "run_command" | "shell"
        )
    }));
}
