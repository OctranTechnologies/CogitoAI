use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use harness_core::{Id, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(String);

static EVENT_COUNTER: AtomicU64 = AtomicU64::new(0);

impl Default for EventId {
    fn default() -> Self {
        Self::new()
    }
}

impl EventId {
    pub fn new() -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |duration| duration.as_nanos());
        let counter = EVENT_COUNTER.fetch_add(1, Ordering::Relaxed);
        Self(format!("{}-{timestamp}-{counter}", std::process::id()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EventId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Timestamp(u64);

impl Timestamp {
    pub fn now() -> Self {
        Self(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis() as u64),
        )
    }

    pub fn unix_millis(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "user.message")]
    UserMessage,
    #[serde(rename = "assistant.delta")]
    AssistantDelta,
    #[serde(rename = "assistant.message")]
    AssistantMessage,
    #[serde(rename = "model.requested")]
    ModelRequested,
    #[serde(rename = "model.response")]
    ModelResponse,
    #[serde(rename = "model.changed")]
    ModelChanged,
    #[serde(rename = "tool.requested")]
    ToolRequested,
    #[serde(rename = "tool.approved")]
    ToolApproved,
    #[serde(rename = "tool.denied")]
    ToolDenied,
    #[serde(rename = "tool.started")]
    ToolStarted,
    #[serde(rename = "tool.output")]
    ToolOutput,
    #[serde(rename = "tool.completed")]
    ToolCompleted,
    #[serde(rename = "tool.failed")]
    ToolFailed,
    #[serde(rename = "process.started")]
    ProcessStarted,
    #[serde(rename = "process.stdout")]
    ProcessStdout,
    #[serde(rename = "process.stderr")]
    ProcessStderr,
    #[serde(rename = "process.exited")]
    ProcessExited,
    #[serde(rename = "policy.decision")]
    PolicyDecision,
    #[serde(rename = "file.changed")]
    FileChanged,
    #[serde(rename = "checkpoint.created")]
    CheckpointCreated,
    #[serde(rename = "checkpoint.restored")]
    CheckpointRestored,
    #[serde(rename = "verification.started")]
    VerificationStarted,
    #[serde(rename = "verification.result")]
    VerificationResult,
    #[serde(rename = "session.resumed")]
    SessionResumed,
    #[serde(rename = "context.compacted")]
    ContextCompacted,
    #[serde(rename = "task.run.updated")]
    TaskRunUpdated,
    #[serde(rename = "session.completed")]
    SessionCompleted,
    #[serde(rename = "session.failed")]
    SessionFailed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CompactState {
    pub task: String,
    /// Durable objective carried across context compaction.
    #[serde(default)]
    pub goal: Option<Goal>,
    /// A compact copy of the current execution plan, including progress.
    #[serde(default)]
    pub execution_plan: Option<ExecutionPlan>,
    pub current_approach: String,
    pub discoveries: Vec<String>,
    pub important_files: Vec<String>,
    pub files_modified: Vec<String>,
    pub decisions: Vec<String>,
    pub failed_attempts: Vec<String>,
    pub test_status: Vec<String>,
    pub remaining_work: Vec<String>,
}

/// Provider-neutral state for one coding task, persisted as snapshots in the
/// session event log so clients can recover the runtime's current understanding.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskPhase {
    #[default]
    Understand,
    Plan,
    SearchRead,
    Edit,
    Verify,
    InspectDiff,
    Repair,
    Finish,
}

/// Task behavior is separate from the permission mode that governs execution.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskMode {
    Explore,
    Plan,
    #[default]
    Code,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImplementationPlan {
    pub goal: String,
    #[serde(default)]
    pub relevant_architecture: Vec<String>,
    #[serde(default)]
    pub files_likely_affected: Vec<String>,
    #[serde(default)]
    pub implementation_steps: Vec<String>,
    #[serde(default)]
    pub validation: Vec<String>,
    #[serde(default)]
    pub risks_or_unknowns: Vec<String>,
}

/// Durable statement of what a long-running task is intended to accomplish.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct Goal {
    #[serde(default)]
    pub objective: String,
    #[serde(default)]
    pub constraints: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub non_goals: Vec<String>,
    #[serde(default)]
    pub current_milestone: Option<String>,
    #[serde(default)]
    pub completion_condition: String,
}

impl Goal {
    pub fn new(objective: impl Into<String>) -> Self {
        Self {
            objective: objective.into(),
            completion_condition: "Acceptance criteria pass, relevant validation succeeds, and the final diff is inspected; otherwise report the blocker or remaining work.".to_owned(),
            ..Self::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    #[default]
    Pending,
    InProgress,
    Completed,
    Blocked,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionTask {
    pub description: String,
    #[serde(default)]
    pub status: PlanItemStatus,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionMilestone {
    pub title: String,
    #[serde(default)]
    pub tasks: Vec<ExecutionTask>,
    #[serde(default)]
    pub affected_architecture: Vec<String>,
    #[serde(default)]
    pub validation_commands: Vec<String>,
    #[serde(default)]
    pub completion_criteria: Vec<String>,
    #[serde(default)]
    pub status: PlanItemStatus,
}

/// Runtime-owned, revisioned plan stored with the session's task snapshots.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPlan {
    #[serde(default)]
    pub revision: u32,
    #[serde(default)]
    pub milestones: Vec<ExecutionMilestone>,
    #[serde(default)]
    pub decision_notes: Vec<String>,
    #[serde(default)]
    pub status: PlanItemStatus,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskCompletionStatus {
    #[default]
    InProgress,
    Done,
    Blocked,
    UserInputRequired,
    ResourceLimitReached,
    Cancelled,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskVerificationResult {
    pub command: String,
    pub category: String,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub summary: String,
    #[serde(default)]
    pub affected_files: Vec<PathBuf>,
    #[serde(default)]
    pub failure_origin: Option<String>,
    #[serde(default)]
    pub relevant_output: String,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct TaskRun {
    pub original_goal: String,
    #[serde(default)]
    pub goal: Goal,
    #[serde(default)]
    pub task_mode: TaskMode,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub current_phase: TaskPhase,
    #[serde(default)]
    pub current_plan: Vec<String>,
    #[serde(default)]
    pub structured_plan: Option<ImplementationPlan>,
    #[serde(default)]
    pub execution_plan: Option<ExecutionPlan>,
    #[serde(default)]
    pub relevant_files: Vec<PathBuf>,
    #[serde(default)]
    pub changed_files: Vec<PathBuf>,
    #[serde(default)]
    pub commands_executed: Vec<String>,
    #[serde(default)]
    pub verification_results: Vec<TaskVerificationResult>,
    #[serde(default)]
    pub final_diff_inspected: bool,
    #[serde(default)]
    pub unresolved_errors: Vec<String>,
    #[serde(default)]
    pub remaining_work: Vec<String>,
    #[serde(default)]
    pub completion_status: TaskCompletionStatus,
}

impl TaskRun {
    pub fn new(goal: impl Into<String>) -> Self {
        let original_goal = goal.into();
        Self {
            goal: Goal::new(original_goal.clone()),
            original_goal,
            ..Self::default()
        }
    }
}

impl CompactState {
    pub fn render(&self) -> String {
        let mut sections = Vec::new();
        push_section(&mut sections, "task", std::slice::from_ref(&self.task));
        if let Some(goal) = &self.goal {
            push_section(
                &mut sections,
                "persistent objective",
                std::slice::from_ref(&goal.objective),
            );
            push_section(
                &mut sections,
                "acceptance criteria",
                &goal.acceptance_criteria,
            );
            push_section(&mut sections, "constraints", &goal.constraints);
            push_section(&mut sections, "non-goals", &goal.non_goals);
            push_section(
                &mut sections,
                "completion condition",
                std::slice::from_ref(&goal.completion_condition),
            );
        }
        if let Some(plan) = &self.execution_plan {
            let progress = plan
                .milestones
                .iter()
                .flat_map(|milestone| {
                    std::iter::once(format!("{} [{:?}]", milestone.title, milestone.status)).chain(
                        milestone
                            .tasks
                            .iter()
                            .map(|task| format!("- {:?}: {}", task.status, task.description)),
                    )
                })
                .collect::<Vec<_>>();
            push_section(&mut sections, "execution plan progress", &progress);
            push_section(&mut sections, "plan decisions", &plan.decision_notes);
        }
        push_section(
            &mut sections,
            "current approach",
            std::slice::from_ref(&self.current_approach),
        );
        push_section(&mut sections, "discoveries", &self.discoveries);
        push_section(&mut sections, "important files", &self.important_files);
        push_section(&mut sections, "files modified", &self.files_modified);
        push_section(&mut sections, "decisions", &self.decisions);
        push_section(&mut sections, "failed attempts", &self.failed_attempts);
        push_section(&mut sections, "test status", &self.test_status);
        push_section(&mut sections, "remaining work", &self.remaining_work);
        sections.join("\n\n")
    }
}

fn push_section(sections: &mut Vec<String>, name: &str, values: &[String]) {
    if values.is_empty() || values.iter().all(String::is_empty) {
        return;
    }
    let value = values
        .iter()
        .filter(|value| !value.trim().is_empty())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    sections.push(format!("### {name}\n{value}"));
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum FileChange {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", content = "data")]
pub enum EventPayload {
    #[serde(rename = "session.started")]
    SessionStarted { workspace_root: PathBuf },
    #[serde(rename = "user.message")]
    UserMessage { text: String },
    #[serde(rename = "assistant.delta")]
    AssistantDelta { text: String },
    #[serde(rename = "assistant.message")]
    AssistantMessage { text: String },
    #[serde(rename = "model.requested")]
    ModelRequested {
        provider: String,
        model: String,
        prompt_tokens: Option<u32>,
    },
    #[serde(rename = "model.response")]
    ModelResponse {
        provider: String,
        model: String,
        text: String,
        input_tokens: Option<u32>,
        output_tokens: Option<u32>,
    },
    #[serde(rename = "model.changed")]
    ModelChanged {
        provider: String,
        model: String,
        #[serde(default)]
        reasoning_effort: Option<String>,
    },
    #[serde(rename = "tool.requested")]
    ToolRequested {
        tool: String,
        arguments: BTreeMap<String, String>,
    },
    #[serde(rename = "tool.approved")]
    ToolApproved {
        tool: String,
        reason: Option<String>,
    },
    #[serde(rename = "tool.denied")]
    ToolDenied { tool: String, reason: String },
    #[serde(rename = "tool.started")]
    ToolStarted { tool: String },
    #[serde(rename = "tool.output")]
    ToolOutput { tool: String, output: String },
    #[serde(rename = "tool.completed")]
    ToolCompleted { tool: String },
    #[serde(rename = "tool.failed")]
    ToolFailed { tool: String, error: String },
    #[serde(rename = "process.started")]
    ProcessStarted {
        command: String,
        working_directory: PathBuf,
        timeout_ms: u64,
    },
    #[serde(rename = "process.stdout")]
    ProcessStdout { chunk: String },
    #[serde(rename = "process.stderr")]
    ProcessStderr { chunk: String },
    #[serde(rename = "process.exited")]
    ProcessExited {
        exit_code: Option<i32>,
        timed_out: bool,
        cancelled: bool,
    },
    #[serde(rename = "policy.decision")]
    PolicyDecision {
        tool: String,
        action: String,
        reason: String,
        rule: String,
        operation: String,
        mode: String,
    },
    #[serde(rename = "file.changed")]
    FileChanged { path: PathBuf, change: FileChange },
    #[serde(rename = "checkpoint.created")]
    CheckpointCreated {
        checkpoint_id: Id,
        reference: String,
    },
    #[serde(rename = "checkpoint.restored")]
    CheckpointRestored {
        checkpoint_id: Id,
        restored_files: Vec<PathBuf>,
        conflicts: Vec<PathBuf>,
    },
    #[serde(rename = "verification.started")]
    VerificationStarted { commands: Vec<String> },
    #[serde(rename = "verification.result")]
    VerificationResult {
        command: String,
        category: String,
        duration_ms: u64,
        passed: bool,
        exit_code: Option<i32>,
        output: String,
        diagnostics: Vec<String>,
        #[serde(default)]
        affected_files: Vec<PathBuf>,
        #[serde(default)]
        failure_origin: Option<String>,
        #[serde(default)]
        relevant_output: String,
    },
    #[serde(rename = "session.resumed")]
    SessionResumed { reason: Option<String> },
    #[serde(rename = "context.compacted")]
    ContextCompacted {
        removed_items: usize,
        summary: String,
        #[serde(default)]
        state: CompactState,
    },
    #[serde(rename = "task.run.updated")]
    TaskRunUpdated { task_run: TaskRun },
    #[serde(rename = "session.completed")]
    SessionCompleted { reason: Option<String> },
    #[serde(rename = "session.failed")]
    SessionFailed { error: String },
}

impl EventPayload {
    pub fn event_type(&self) -> EventType {
        match self {
            Self::SessionStarted { .. } => EventType::SessionStarted,
            Self::UserMessage { .. } => EventType::UserMessage,
            Self::AssistantDelta { .. } => EventType::AssistantDelta,
            Self::AssistantMessage { .. } => EventType::AssistantMessage,
            Self::ModelRequested { .. } => EventType::ModelRequested,
            Self::ModelResponse { .. } => EventType::ModelResponse,
            Self::ModelChanged { .. } => EventType::ModelChanged,
            Self::ToolRequested { .. } => EventType::ToolRequested,
            Self::ToolApproved { .. } => EventType::ToolApproved,
            Self::ToolDenied { .. } => EventType::ToolDenied,
            Self::ToolStarted { .. } => EventType::ToolStarted,
            Self::ToolOutput { .. } => EventType::ToolOutput,
            Self::ToolCompleted { .. } => EventType::ToolCompleted,
            Self::ToolFailed { .. } => EventType::ToolFailed,
            Self::ProcessStarted { .. } => EventType::ProcessStarted,
            Self::ProcessStdout { .. } => EventType::ProcessStdout,
            Self::ProcessStderr { .. } => EventType::ProcessStderr,
            Self::ProcessExited { .. } => EventType::ProcessExited,
            Self::PolicyDecision { .. } => EventType::PolicyDecision,
            Self::FileChanged { .. } => EventType::FileChanged,
            Self::CheckpointCreated { .. } => EventType::CheckpointCreated,
            Self::CheckpointRestored { .. } => EventType::CheckpointRestored,
            Self::VerificationStarted { .. } => EventType::VerificationStarted,
            Self::VerificationResult { .. } => EventType::VerificationResult,
            Self::SessionResumed { .. } => EventType::SessionResumed,
            Self::ContextCompacted { .. } => EventType::ContextCompacted,
            Self::TaskRunUpdated { .. } => EventType::TaskRunUpdated,
            Self::SessionCompleted { .. } => EventType::SessionCompleted,
            Self::SessionFailed { .. } => EventType::SessionFailed,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct HarnessEvent {
    pub schema_version: u32,
    pub event_id: EventId,
    pub session_id: SessionId,
    pub timestamp: Timestamp,
    pub event_type: EventType,
    pub parent_id: Option<Id>,
    pub correlation_id: Option<Id>,
    pub payload: EventPayload,
}

impl HarnessEvent {
    pub fn new(
        session_id: SessionId,
        payload: EventPayload,
        parent_id: Option<Id>,
        correlation_id: Option<Id>,
    ) -> Self {
        Self {
            schema_version: 1,
            event_id: EventId::new(),
            session_id,
            timestamp: Timestamp::now(),
            event_type: payload.event_type(),
            parent_id,
            correlation_id,
            payload,
        }
    }
}
