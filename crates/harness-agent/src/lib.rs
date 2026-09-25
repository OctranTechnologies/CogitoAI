use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_context::{ContextBuilder, ContextInput, ToolContextResult, WorkspaceMetadata};
use harness_core::{Error, SessionId};
use harness_models::{Message, ModelProvider, ModelRequest, ProviderError, StreamDeltaKind, Usage};
use harness_policy::{ExecutionMode, Policy, PolicyDecision, PolicyEvaluation, PolicyRequest};
use harness_session::{
    ConversationMessage as SessionConversationMessage, EventBus, EventPayload, HarnessEvent,
    MessageRole, SessionStore,
};
use harness_tools::{CancellationToken, ToolContext, ToolRegistry, ToolRequest, ToolResult};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLimits {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub max_runtime: Duration,
    pub max_model_tokens: u64,
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_turns: 8,
            max_tool_calls: 32,
            max_runtime: Duration::from_secs(600),
            max_model_tokens: 100_000,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentTask {
    pub workspace_root: PathBuf,
    pub user_task: String,
    pub system_instructions: String,
    pub workspace: WorkspaceMetadata,
    pub instructions: Vec<harness_core::InstructionFile>,
    pub recent_conversation: Vec<SessionConversationMessage>,
    pub selected_files: Vec<harness_context::ExplicitFile>,
    pub initial_tool_results: Vec<ToolContextResult>,
    pub git_status: Option<harness_git::GitStatus>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentOutcome {
    pub session_id: SessionId,
    pub final_message: String,
    pub turns: u32,
    pub tool_calls: u32,
    pub model_tokens: u64,
}

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("agent cancelled")]
    Cancelled,
    #[error("agent approval was denied for {tool}")]
    ApprovalDenied { tool: String },
    #[error("agent exceeded {limit}")]
    LimitExceeded { limit: String },
    #[error("model provider failed: {0}")]
    Model(#[from] ProviderError),
    #[error("core operation failed: {0}")]
    Core(String),
    #[error("tool execution failed: {0}")]
    Tool(String),
}

pub trait ApprovalHandler: Send + Sync {
    fn request(&self, tool: &ToolRequest) -> Result<bool, AgentError>;
}

#[derive(Default)]
pub struct DenyApprovalHandler;

impl ApprovalHandler for DenyApprovalHandler {
    fn request(&self, _tool: &ToolRequest) -> Result<bool, AgentError> {
        Ok(false)
    }
}

pub struct AgentRunner {
    provider: Arc<dyn ModelProvider>,
    model: String,
    tools: ToolRegistry,
    policy: Arc<dyn Policy>,
    sessions: Arc<dyn SessionStore>,
    context_builder: ContextBuilder,
    limits: AgentLimits,
    approval_handler: Arc<dyn ApprovalHandler>,
    event_bus: EventBus,
}

impl AgentRunner {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: Arc<dyn ModelProvider>,
        model: impl Into<String>,
        tools: ToolRegistry,
        policy: Arc<dyn Policy>,
        sessions: Arc<dyn SessionStore>,
        context_builder: ContextBuilder,
        limits: AgentLimits,
        approval_handler: Arc<dyn ApprovalHandler>,
    ) -> Self {
        Self {
            provider,
            model: model.into(),
            tools,
            policy,
            sessions,
            context_builder,
            limits,
            approval_handler,
            event_bus: EventBus::new(),
        }
    }

    pub fn with_event_bus(mut self, event_bus: EventBus) -> Self {
        self.event_bus = event_bus;
        self
    }

    pub fn event_bus(&self) -> EventBus {
        self.event_bus.clone()
    }

    pub fn run(
        &self,
        task: &AgentTask,
        cancellation: &CancellationToken,
    ) -> Result<AgentOutcome, AgentError> {
        let started_at = Instant::now();
        let session = self
            .sessions
            .create(&task.workspace_root)
            .map_err(|error| AgentError::Core(error.to_string()))?;
        let session_id = session.id.clone();
        let collector = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&collector);
        let collected_session = session_id.clone();
        let _subscription = self
            .event_bus
            .subscribe(Arc::new(move |event: &HarnessEvent| {
                if event.session_id == collected_session {
                    collected
                        .lock()
                        .expect("agent event collector poisoned")
                        .push(event.clone());
                }
            }));
        let approved = Arc::new(Mutex::new(HashSet::<String>::new()));
        let approval_policy = ApprovedPolicy {
            base: Arc::clone(&self.policy),
            approved: Arc::clone(&approved),
        };
        let mut context_input = ContextInput {
            system_instructions: task.system_instructions.clone(),
            workspace: task.workspace.clone(),
            instructions: task.instructions.clone(),
            user_request: task.user_task.clone(),
            conversation: task.recent_conversation.clone(),
            files: task.selected_files.clone(),
            tool_results: task.initial_tool_results.clone(),
            git_status: task.git_status.clone(),
        };
        self.emit(
            &session_id,
            EventPayload::UserMessage {
                text: task.user_task.clone(),
            },
            &collector,
        )?;
        let mut turns = 0;
        let mut tool_calls = 0;
        let mut model_tokens = 0;
        loop {
            self.check_limits(
                started_at,
                turns,
                tool_calls,
                model_tokens,
                cancellation,
                &session_id,
                &collector,
            )?;
            turns += 1;
            let assembly = match self.context_builder.build(&context_input) {
                Ok(assembly) => assembly,
                Err(error) => {
                    return self.fail(session_id, collector, AgentError::Core(error.to_string()))
                }
            };
            let model_request = ModelRequest {
                model: self.model.clone(),
                messages: vec![Message::user_text(assembly.prompt)],
                tools: self
                    .tools
                    .specs()
                    .into_iter()
                    .map(|spec| harness_models::ToolDefinition {
                        name: spec.name,
                        description: spec.description,
                        input_schema: spec.arguments_schema,
                    })
                    .collect(),
                max_output_tokens: None,
                temperature: None,
                metadata: Default::default(),
            };
            self.emit(
                &session_id,
                EventPayload::ModelRequested {
                    provider: self.provider.name().to_owned(),
                    model: self.model.clone(),
                    prompt_tokens: None,
                },
                &collector,
            )?;
            let response = match self.provider.stream(&model_request, &mut |delta| {
                if cancellation.is_cancelled() {
                    return Err(ProviderError::StreamConsumer);
                }
                if let StreamDeltaKind::Text { text } = delta.delta {
                    self.event_bus.publish(&HarnessEvent::new(
                        session_id.clone(),
                        EventPayload::AssistantDelta { text },
                        None,
                        None,
                    ));
                }
                Ok(())
            }) {
                Ok(response) => response,
                Err(error) => return self.fail(session_id, collector, AgentError::Model(error)),
            };
            self.flush(&session_id, &collector)?;
            model_tokens += usage_tokens(response.usage.as_ref());
            self.emit(
                &session_id,
                EventPayload::ModelResponse {
                    provider: self.provider.name().to_owned(),
                    model: response.model.clone(),
                    text: response.text(),
                    input_tokens: response.usage.as_ref().and_then(|usage| usage.input_tokens),
                    output_tokens: response
                        .usage
                        .as_ref()
                        .and_then(|usage| usage.output_tokens),
                },
                &collector,
            )?;
            if !response.text().is_empty() {
                self.emit(
                    &session_id,
                    EventPayload::AssistantMessage {
                        text: response.text(),
                    },
                    &collector,
                )?;
                context_input.conversation.push(SessionConversationMessage {
                    role: MessageRole::Assistant,
                    text: response.text(),
                });
            }
            self.check_limits(
                started_at,
                turns,
                tool_calls,
                model_tokens,
                cancellation,
                &session_id,
                &collector,
            )?;
            if !response.tool_calls.is_empty() && turns >= self.limits.max_turns {
                return self.fail(
                    session_id,
                    collector,
                    AgentError::LimitExceeded {
                        limit: "max_turns".to_owned(),
                    },
                );
            }
            if response.tool_calls.is_empty() {
                self.emit(
                    &session_id,
                    EventPayload::SessionCompleted { reason: None },
                    &collector,
                )?;
                return Ok(AgentOutcome {
                    session_id,
                    final_message: response.text(),
                    turns,
                    tool_calls,
                    model_tokens,
                });
            }
            for tool_call in response.tool_calls {
                if cancellation.is_cancelled() {
                    return self.fail(session_id, collector, AgentError::Cancelled);
                }
                if tool_calls >= self.limits.max_tool_calls {
                    return self.fail(
                        session_id,
                        collector,
                        AgentError::LimitExceeded {
                            limit: "max_tool_calls".to_owned(),
                        },
                    );
                }
                tool_calls += 1;
                let is_shell = tool_call.name == "shell";
                let request = ToolRequest::new(tool_call.name.clone(), tool_call.arguments.clone());
                let result = match self.execute_tool(
                    &session_id,
                    &task.workspace_root,
                    &approval_policy,
                    &approved,
                    request,
                    &collector,
                ) {
                    Ok(result) => result,
                    Err(error) => return self.fail(session_id, collector, error),
                };
                context_input.tool_results.push(ToolContextResult {
                    name: tool_call.name,
                    result,
                    is_shell,
                });
            }
        }
    }

    fn execute_tool(
        &self,
        session_id: &SessionId,
        workspace_root: &std::path::Path,
        policy: &ApprovedPolicy,
        approved: &Arc<Mutex<HashSet<String>>>,
        request: ToolRequest,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
    ) -> Result<ToolResult, AgentError> {
        let context = ToolContext {
            policy,
            working_directory: workspace_root,
            event_bus: Some(&self.event_bus),
            session_id: Some(session_id),
            correlation_id: None,
        };
        let result = self.tools.execute(&context, request.clone());
        self.flush(session_id, collector)?;
        match result {
            Ok(result) => Ok(result),
            Err(Error::PermissionRequired { .. }) => {
                let approved_by_user = self.approval_handler.request(&request)?;
                if !approved_by_user {
                    return Err(AgentError::ApprovalDenied { tool: request.name });
                }
                approved
                    .lock()
                    .expect("agent approval lock poisoned")
                    .insert(approval_key(&request));
                let result = self.tools.execute(&context, request);
                self.flush(session_id, collector)?;
                result.map_err(|error| AgentError::Tool(error.to_string()))
            }
            Err(error) => Err(AgentError::Tool(error.to_string())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn check_limits(
        &self,
        started_at: Instant,
        turns: u32,
        tool_calls: u32,
        model_tokens: u64,
        cancellation: &CancellationToken,
        session_id: &SessionId,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
    ) -> Result<(), AgentError> {
        if cancellation.is_cancelled() {
            return self.fail(session_id.clone(), collector.clone(), AgentError::Cancelled);
        }
        let exceeded = turns > self.limits.max_turns
            || tool_calls > self.limits.max_tool_calls
            || (self.limits.max_model_tokens > 0 && model_tokens > self.limits.max_model_tokens)
            || (self.limits.max_runtime != Duration::ZERO
                && started_at.elapsed() > self.limits.max_runtime);
        if exceeded {
            return self.fail(
                session_id.clone(),
                collector.clone(),
                AgentError::LimitExceeded {
                    limit: "agent safety/resource limit".to_owned(),
                },
            );
        }
        Ok(())
    }

    fn fail<T>(
        &self,
        session_id: SessionId,
        collector: Arc<Mutex<Vec<HarnessEvent>>>,
        error: AgentError,
    ) -> Result<T, AgentError> {
        let _ = self.flush(&session_id, &collector);
        let _ = self.emit(
            &session_id,
            EventPayload::SessionFailed {
                error: error.to_string(),
            },
            &collector,
        );
        Err(error)
    }

    fn emit(
        &self,
        session_id: &SessionId,
        payload: EventPayload,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
    ) -> Result<(), AgentError> {
        self.event_bus
            .publish(&HarnessEvent::new(session_id.clone(), payload, None, None));
        self.flush(session_id, collector)
    }

    fn flush(
        &self,
        session_id: &SessionId,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
    ) -> Result<(), AgentError> {
        let events =
            std::mem::take(&mut *collector.lock().expect("agent event collector poisoned"));
        for event in events {
            if event.session_id == *session_id {
                self.sessions
                    .append_event(session_id, event)
                    .map_err(|error| AgentError::Core(error.to_string()))?;
            }
        }
        Ok(())
    }
}

struct ApprovedPolicy {
    base: Arc<dyn Policy>,
    approved: Arc<Mutex<HashSet<String>>>,
}

impl Policy for ApprovedPolicy {
    fn check(&self, permission: harness_policy::Permission) -> PolicyDecision {
        self.base.check(permission)
    }

    fn mode(&self) -> ExecutionMode {
        self.base.mode()
    }

    fn evaluate(&self, request: &PolicyRequest) -> PolicyEvaluation {
        let evaluation = self.base.evaluate(request);
        if evaluation.decision == PolicyDecision::Ask
            && self
                .approved
                .lock()
                .expect("agent approval lock poisoned")
                .contains(&approval_key_from_request(request))
        {
            return PolicyEvaluation::new(
                PolicyDecision::Allow,
                "user-approval",
                "the user approved this ASK decision",
            );
        }
        evaluation
    }
}

fn approval_key(request: &ToolRequest) -> String {
    request.name.clone()
}

fn approval_key_from_request(request: &PolicyRequest) -> String {
    request.tool_name.clone()
}

fn usage_tokens(usage: Option<&Usage>) -> u64 {
    usage
        .and_then(|usage| usage.total_tokens.map(u64::from))
        .or_else(|| {
            usage
                .and_then(|usage| usage.input_tokens.zip(usage.output_tokens))
                .map(|(input, output)| u64::from(input) + u64::from(output))
        })
        .unwrap_or_default()
}
