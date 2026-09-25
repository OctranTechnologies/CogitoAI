use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_context::{ContextBuilder, ContextInput, ToolContextResult, WorkspaceMetadata};
use harness_core::{Error, SessionId};
use harness_models::{Message, ModelProvider, ModelRequest, ProviderError, StreamDeltaKind, Usage};
use harness_policy::{ExecutionMode, Policy, PolicyDecision, PolicyEvaluation, PolicyRequest};
use harness_session::{
    CompactState, ConversationMessage as SessionConversationMessage, EventBus, EventPayload,
    HarnessEvent, MessageRole, SessionStore,
};
use harness_tools::{CancellationToken, ToolContext, ToolRegistry, ToolRequest, ToolResult};
use harness_verification::{VerificationPlan, VerificationRequest, Verifier};
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

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactionConfig {
    pub threshold_tokens: u32,
    pub keep_recent_messages: usize,
}

impl Default for CompactionConfig {
    fn default() -> Self {
        Self {
            threshold_tokens: 24 * 1024,
            keep_recent_messages: 4,
        }
    }
}

pub struct CompactionRequest<'a> {
    pub task: &'a str,
    pub conversation: &'a [SessionConversationMessage],
    pub tool_results: &'a [ToolContextResult],
}

pub trait CompactionStrategy: Send + Sync {
    fn compact(&self, request: &CompactionRequest<'_>) -> Result<CompactState, String>;
}

#[derive(Clone, Copy, Default)]
pub struct DeriveCompactionStrategy;

impl CompactionStrategy for DeriveCompactionStrategy {
    fn compact(&self, request: &CompactionRequest<'_>) -> Result<CompactState, String> {
        let mut state = CompactState {
            task: request.task.to_owned(),
            current_approach: request
                .conversation
                .iter()
                .rev()
                .find(|message| message.role == MessageRole::Assistant)
                .map_or_else(|| request.task.to_owned(), |message| message.text.clone()),
            ..CompactState::default()
        };
        for result in request.tool_results {
            let summary = bounded_summary(&format!("{}: {}", result.name, result.result.output));
            if result.name.contains("test") || summary.to_ascii_lowercase().contains("test") {
                state.test_status.push(summary);
            } else if summary.to_ascii_lowercase().contains("fail")
                || summary.to_ascii_lowercase().contains("error")
            {
                state.failed_attempts.push(summary);
            } else {
                state.discoveries.push(summary);
            }
            for path in &result.result.changed_files {
                let path = path.display().to_string();
                if !state.files_modified.contains(&path) {
                    state.files_modified.push(path.clone());
                }
                if !state.important_files.contains(&path) {
                    state.important_files.push(path);
                }
            }
        }
        for message in request.conversation.iter().rev().take(5) {
            if message.role == MessageRole::Assistant
                && !message.text.trim().is_empty()
                && !state.decisions.contains(&message.text)
            {
                state.decisions.push(message.text.clone());
            }
        }
        Ok(state)
    }
}

fn bounded_summary(value: &str) -> String {
    const LIMIT: usize = 240;
    if value.len() <= LIMIT {
        return value.to_owned();
    }
    let mut end = LIMIT;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}…", &value[..end])
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
    pub verification_plan: Option<VerificationPlan>,
    pub resume_session: Option<SessionId>,
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
    verifier: Option<Arc<dyn Verifier>>,
    compaction_config: CompactionConfig,
    compaction_strategy: Arc<dyn CompactionStrategy>,
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
            verifier: None,
            compaction_config: CompactionConfig::default(),
            compaction_strategy: Arc::new(DeriveCompactionStrategy),
            event_bus: EventBus::new(),
        }
    }

    pub fn with_event_bus(mut self, event_bus: EventBus) -> Self {
        self.event_bus = event_bus;
        self
    }

    pub fn with_verifier(mut self, verifier: Arc<dyn Verifier>) -> Self {
        self.verifier = Some(verifier);
        self
    }

    pub fn with_compaction_config(mut self, config: CompactionConfig) -> Self {
        self.compaction_config = config;
        self
    }

    pub fn with_compaction_strategy(mut self, strategy: Arc<dyn CompactionStrategy>) -> Self {
        self.compaction_strategy = strategy;
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
        let session = if let Some(session_id) = &task.resume_session {
            let existing = self
                .sessions
                .load(session_id)
                .map_err(|error| AgentError::Core(error.to_string()))?;
            let requested_root = std::fs::canonicalize(&task.workspace_root)
                .map_err(|error| AgentError::Core(error.to_string()))?;
            if existing.workspace_root != requested_root {
                return Err(AgentError::Core(format!(
                    "session {} belongs to {}, not {}",
                    session_id,
                    existing.workspace_root.display(),
                    requested_root.display()
                )));
            }
            self.sessions
                .resume(session_id)
                .map_err(|error| AgentError::Core(error.to_string()))?
        } else {
            self.sessions
                .create(&task.workspace_root)
                .map_err(|error| AgentError::Core(error.to_string()))?
        };
        let previous_state = session
            .state()
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
            conversation: if task.recent_conversation.is_empty() {
                if previous_state.continuation.is_some() {
                    previous_state.working_messages
                } else {
                    previous_state.messages
                }
            } else {
                task.recent_conversation.clone()
            },
            files: task.selected_files.clone(),
            tool_results: task.initial_tool_results.clone(),
            compacted_state: previous_state.continuation,
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
            let mut assembly = match self.context_builder.build(&context_input) {
                Ok(assembly) => assembly,
                Err(error) => {
                    return self.fail(session_id, collector, AgentError::Core(error.to_string()))
                }
            };
            if self.compaction_config.threshold_tokens > 0
                && assembly.estimated_tokens >= self.compaction_config.threshold_tokens
            {
                self.compact_context(&session_id, &mut context_input, &collector)?;
                assembly = match self.context_builder.build(&context_input) {
                    Ok(assembly) => assembly,
                    Err(error) => {
                        return self.fail(
                            session_id,
                            collector,
                            AgentError::Core(error.to_string()),
                        )
                    }
                };
            }
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
                let changed_files = result.changed_files.clone();
                context_input.tool_results.push(ToolContextResult {
                    name: tool_call.name,
                    result,
                    is_shell,
                });
                self.run_verification_if_needed(
                    &session_id,
                    &task.workspace_root,
                    task.verification_plan.as_ref(),
                    self.verifier.as_ref(),
                    &changed_files,
                    &mut context_input,
                    &collector,
                )?;
            }
        }
    }

    fn compact_context(
        &self,
        session_id: &SessionId,
        context_input: &mut ContextInput,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
    ) -> Result<(), AgentError> {
        let request = CompactionRequest {
            task: &context_input.user_request,
            conversation: &context_input.conversation,
            tool_results: &context_input.tool_results,
        };
        let compacted = self
            .compaction_strategy
            .compact(&request)
            .map_err(|error| AgentError::Core(format!("context compaction failed: {error}")))?;
        let removed_items = context_input
            .conversation
            .len()
            .saturating_sub(self.compaction_config.keep_recent_messages)
            + context_input.tool_results.len();
        self.emit(
            session_id,
            EventPayload::ContextCompacted {
                removed_items,
                summary: compacted.render(),
                state: compacted.clone(),
            },
            collector,
        )?;
        let keep = self.compaction_config.keep_recent_messages;
        let split = context_input.conversation.len().saturating_sub(keep);
        if split > 0 {
            context_input.conversation.drain(..split);
        }
        context_input.tool_results.clear();
        context_input.compacted_state = Some(compacted);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn run_verification_if_needed(
        &self,
        session_id: &SessionId,
        workspace_root: &std::path::Path,
        plan: Option<&VerificationPlan>,
        verifier: Option<&Arc<dyn Verifier>>,
        changed_files: &[PathBuf],
        context_input: &mut ContextInput,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
    ) -> Result<(), AgentError> {
        if changed_files.is_empty() {
            return Ok(());
        }
        let (Some(plan), Some(verifier)) = (plan, verifier) else {
            return Ok(());
        };
        let plan = plan.targeted();
        if plan.steps.is_empty() {
            return Ok(());
        }
        self.emit(
            session_id,
            EventPayload::VerificationStarted {
                commands: plan
                    .steps
                    .iter()
                    .map(|step| format_command(&step.command))
                    .collect(),
            },
            collector,
        )?;
        let request = VerificationRequest {
            working_directory: workspace_root.to_path_buf(),
            plan,
            max_output_bytes: 64 * 1024,
        };
        match verifier.verify(&request) {
            Ok(reports) => {
                for report in reports {
                    self.emit(
                        session_id,
                        EventPayload::VerificationResult {
                            command: report.command.clone(),
                            category: format!("{:?}", report.category),
                            duration_ms: report.duration_ms,
                            passed: report.passed,
                            exit_code: report.exit_code,
                            output: report.output.clone(),
                            diagnostics: report.diagnostics.clone(),
                        },
                        collector,
                    )?;
                    let summary = format!(
                        "verification passed={} exit_code={:?} diagnostics={:?}\n{}",
                        report.passed, report.exit_code, report.diagnostics, report.output
                    );
                    context_input.tool_results.push(ToolContextResult {
                        name: "verification".to_owned(),
                        result: ToolResult::new(summary),
                        is_shell: false,
                    });
                }
            }
            Err(error) => {
                let message = error.to_string();
                self.emit(
                    session_id,
                    EventPayload::VerificationResult {
                        command: "verification".to_owned(),
                        category: "runner".to_owned(),
                        duration_ms: 0,
                        passed: false,
                        exit_code: None,
                        output: message.clone(),
                        diagnostics: vec![message.clone()],
                    },
                    collector,
                )?;
                context_input.tool_results.push(ToolContextResult {
                    name: "verification".to_owned(),
                    result: ToolResult::new(message),
                    is_shell: false,
                });
            }
        }
        Ok(())
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

fn format_command(command: &harness_core::CommandSpec) -> String {
    std::iter::once(&command.program)
        .chain(command.args.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join(" ")
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
