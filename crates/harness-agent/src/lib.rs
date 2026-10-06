use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_context::{ContextBuilder, ContextInput, ToolContextResult, WorkspaceMetadata};
use harness_core::{discover_workspace, CommandSpec, Error, SessionId};
use harness_git::{CheckpointStore, GitClient};
use harness_models::{
    Message, ModelPricing, ModelProvider, ModelRequest, ModelStreamEvent, ProviderError,
    ReasoningConfig, Role, Usage,
};
use harness_policy::{
    ExecutionMode, OperationKind, Policy, PolicyDecision, PolicyEvaluation, PolicyRequest,
};
use harness_session::{
    CompactState, ConversationMessage as SessionConversationMessage, EventBus, EventPayload,
    ExecutionMilestone, ExecutionPlan, ExecutionTask, Goal, HarnessEvent, ImplementationPlan,
    MessageRole, PlanItemStatus, SessionStore, TaskCompletionStatus, TaskMode, TaskPhase, TaskRun,
    TaskVerificationResult,
};
use harness_tools::{CancellationToken, ToolContext, ToolRegistry, ToolRequest, ToolResult};
use harness_verification::{
    FailureOrigin, VerificationCategory, VerificationPlan, VerificationPlanner,
    VerificationRequest, VerificationStep, Verifier,
};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct AgentLimits {
    pub max_turns: u32,
    pub max_tool_calls: u32,
    pub max_runtime: Duration,
    pub max_model_tokens: u64,
    /// Estimated micro-USD. Enforced only when both usage and model pricing are known.
    #[serde(default)]
    pub max_estimated_cost_microusd: Option<u64>,
    /// Maximum executions of the same tool with identical arguments per task.
    #[serde(default = "default_repeated_call_limit")]
    pub max_repeated_tool_calls: u32,
    /// Maximum times the same failure may recur before the run is blocked.
    #[serde(default = "default_repeated_failure_limit")]
    pub max_repeated_failures: u32,
}

fn default_repeated_call_limit() -> u32 {
    3
}

fn default_repeated_failure_limit() -> u32 {
    3
}

impl Default for AgentLimits {
    fn default() -> Self {
        Self {
            max_turns: 256,
            max_tool_calls: 512,
            max_runtime: Duration::from_secs(3600),
            max_model_tokens: 1_000_000,
            max_estimated_cost_microusd: None,
            max_repeated_tool_calls: default_repeated_call_limit(),
            max_repeated_failures: default_repeated_failure_limit(),
        }
    }
}

impl AgentLimits {
    /// Reads optional per-runtime safeguards. Invalid values are ignored so a
    /// malformed limit cannot prevent the application from starting.
    pub fn from_env() -> Self {
        let mut limits = Self::default();
        apply_env_limit("COGITO_AGENT_MAX_TURNS", &mut limits.max_turns);
        apply_env_limit("COGITO_AGENT_MAX_TOOL_CALLS", &mut limits.max_tool_calls);
        apply_env_duration("COGITO_AGENT_MAX_RUNTIME_SECONDS", &mut limits.max_runtime);
        apply_env_limit(
            "COGITO_AGENT_MAX_MODEL_TOKENS",
            &mut limits.max_model_tokens,
        );
        apply_env_limit(
            "COGITO_AGENT_MAX_REPEATED_TOOL_CALLS",
            &mut limits.max_repeated_tool_calls,
        );
        apply_env_limit(
            "COGITO_AGENT_MAX_REPEATED_FAILURES",
            &mut limits.max_repeated_failures,
        );
        if let Ok(value) = std::env::var("COGITO_AGENT_MAX_COST_USD") {
            limits.max_estimated_cost_microusd = decimal_microusd(&value);
        }
        limits
    }
}

fn apply_env_limit<T>(name: &str, target: &mut T)
where
    T: std::str::FromStr,
{
    if let Ok(value) = std::env::var(name) {
        if let Ok(parsed) = value.parse() {
            *target = parsed;
        }
    }
}

fn apply_env_duration(name: &str, target: &mut Duration) {
    if let Ok(value) = std::env::var(name) {
        if let Ok(seconds) = value.parse::<u64>() {
            *target = Duration::from_secs(seconds);
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
    pub goal: Option<&'a Goal>,
    pub execution_plan: Option<&'a ExecutionPlan>,
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
            goal: request.goal.cloned(),
            execution_plan: request.execution_plan.cloned(),
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

fn bounded_context(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let suffix = "\n... relevant verification output truncated ...";
    let body_limit = limit.saturating_sub(suffix.len());
    let mut end = body_limit;
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}{suffix}", &value[..end])
}

fn task_final_diff(workspace_root: &std::path::Path, changed_files: &[PathBuf]) -> Option<String> {
    const LIMIT: usize = 16 * 1024;
    let workspace_root = std::fs::canonicalize(workspace_root).ok()?;
    let git = GitClient::open(&workspace_root).ok()?;
    let mut output = String::new();
    for changed in changed_files {
        let absolute = if changed.is_absolute() {
            changed.clone()
        } else {
            workspace_root.join(changed)
        };
        let relative = absolute.strip_prefix(git.root()).ok()?;
        let path = relative.to_str()?;
        let change = git.file_change(path).ok()?;
        if change.patch.is_empty() {
            continue;
        }
        output.push_str(&format!("--- task change: {} ---\n", change.path));
        output.push_str(&change.patch);
        if output.len() >= LIMIT {
            return Some(bounded_context(&output, LIMIT));
        }
    }
    Some(if output.is_empty() {
        "No task-owned file diffs were returned by Git.".to_owned()
    } else {
        output
    })
}

fn complete_edit_diff(result: &ToolResult, changed_files: &[PathBuf]) -> Option<String> {
    if changed_files.is_empty()
        || result.truncated
        || result
            .metadata
            .get("diff_truncated")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false)
    {
        return None;
    }
    let diff = result
        .metadata
        .get("diff")
        .and_then(serde_json::Value::as_str)?;
    (!diff.trim().is_empty()).then(|| diff.to_owned())
}

fn push_bounded<T>(values: &mut Vec<T>, value: T, limit: usize) {
    values.push(value);
    if values.len() > limit {
        values.drain(..values.len() - limit);
    }
}

fn push_unique_bounded<T: Eq>(values: &mut Vec<T>, value: T, limit: usize) {
    if !values.contains(&value) {
        push_bounded(values, value, limit);
    }
}

fn coding_agent_instructions(existing: &str) -> String {
    let guidance = "\
You are a software-engineering coding agent. Understand the request and inspect the repository before editing. Make focused changes, run the cheapest useful checks selected from workspace configuration and repository verification instructions, read structured diagnostics, inspect the final diff, and repair failures before finishing. Verification runs stop at the first failure so you can diagnose and repair before broader checks. For multi-step work, state a concise plan with a line beginning `Plan:`; skip planning for a straightforward one-file fix. For a broad or high-risk request, you may recommend that the user switch to PLAN mode first, but do not force simple tasks through a verbose plan. The runtime persists the goal and plan across turns, compaction, and restart: continue the current milestone and next incomplete task instead of recreating the plan. Only revise a saved plan when new evidence materially changes the approach; use `Plan revision: <specific reason>` followed by a `Plan:` list. Repeated `Plan:` text alone does not replace a saved plan. Revise your approach when a command or test fails. Never claim a task is complete while a known verification or tool error remains unresolved. If a failure is clearly pre-existing or unrelated to your patch, inspect the evidence and report its exact command and reason on a line beginning `[UNRELATED_VERIFICATION] ` followed by the exact command and ` :: ` plus the evidence. Use this only when the failure is not caused by your changes. If a necessary user decision blocks safe progress, finish with `[USER_INPUT_REQUIRED]` and one concise question. If the requested task is impossible with the available repository or tools, finish with `[BLOCKED]` and the concrete reason. Otherwise finish with a concise result and mention verification performed.";
    if existing.trim().is_empty() {
        guidance.to_owned()
    } else {
        format!("{}\n\n{guidance}", existing.trim())
    }
}

fn task_mode_instructions(existing: String, mode: TaskMode) -> String {
    let behavior = match mode {
        TaskMode::Explore => "\
Current task mode: EXPLORE. Inspect the repository and answer the user's question. You may only read, list, or search workspace content. Do not edit or create files, apply patches, or run commands. Do not perform destructive actions. If the user asks for implementation, explain what you found and recommend switching to PLAN or CODE.",
        TaskMode::Plan => "\
Current task mode: PLAN. Inspect the repository and produce a structured implementation plan without changing files or running commands. You may only read, list, or search workspace content. Do not edit or create files, apply patches, or execute commands. The final response must use these headings: Goal, Relevant architecture, Files likely affected, Implementation steps, Validation, Risks/unknowns. Be specific, and keep simple plans concise.",
        TaskMode::Code => "Current task mode: CODE. Follow the normal coding-agent workflow and the active execution permission mode.",
    };
    format!("{existing}\n\n{behavior}")
}

fn task_mode_allows_tool(mode: TaskMode, operation: Option<OperationKind>) -> bool {
    mode == TaskMode::Code || matches!(operation, Some(OperationKind::Read | OperationKind::Search))
}

fn append_acceptance_criteria(instructions: &mut String, criteria: &[String]) {
    if criteria.is_empty() {
        return;
    }
    instructions.push_str("\n\nTreat these acceptance criteria as required for completion:");
    for criterion in criteria {
        instructions.push_str("\n- ");
        instructions.push_str(criterion);
    }
    instructions.push_str(
        "\nIf any criterion cannot be met, do not claim completion; explain it with [BLOCKED] or ask for the specific decision with [USER_INPUT_REQUIRED].",
    );
}

fn append_prior_task_state(instructions: &mut String, previous: &TaskRun) {
    instructions
        .push_str("\n\nThis is a resumed task. Continue from the runtime's persisted state:");
    instructions.push_str(&format!("\nOriginal goal: {}", previous.original_goal));
    if !previous.goal.constraints.is_empty() {
        instructions.push_str("\nConstraints:");
        for item in &previous.goal.constraints {
            instructions.push_str("\n- ");
            instructions.push_str(item);
        }
    }
    if !previous.goal.non_goals.is_empty() {
        instructions.push_str("\nNon-goals:");
        for item in &previous.goal.non_goals {
            instructions.push_str("\n- ");
            instructions.push_str(item);
        }
    }
    if !previous.goal.completion_condition.is_empty() {
        instructions.push_str(&format!(
            "\nCompletion condition: {}",
            previous.goal.completion_condition
        ));
    }
    if !previous.current_plan.is_empty() {
        instructions.push_str("\nCurrent plan:");
        for step in &previous.current_plan {
            instructions.push_str("\n- ");
            instructions.push_str(step);
        }
    }
    if !previous.changed_files.is_empty() {
        instructions.push_str("\nFiles already changed:");
        for path in &previous.changed_files {
            instructions.push_str("\n- ");
            instructions.push_str(&path.display().to_string());
        }
    }
    if !previous.unresolved_errors.is_empty() {
        instructions.push_str("\nUnresolved errors:");
        for error in &previous.unresolved_errors {
            instructions.push_str("\n- ");
            instructions.push_str(error);
        }
    }
    if !previous.remaining_work.is_empty() {
        instructions.push_str("\nRemaining work:");
        for item in &previous.remaining_work {
            instructions.push_str("\n- ");
            instructions.push_str(item);
        }
    }
    if let Some(plan) = &previous.structured_plan {
        instructions.push_str("\nApproved implementation plan:");
        for (heading, items) in [
            ("Relevant architecture", &plan.relevant_architecture),
            ("Files likely affected", &plan.files_likely_affected),
            ("Implementation steps", &plan.implementation_steps),
            ("Validation", &plan.validation),
            ("Risks/unknowns", &plan.risks_or_unknowns),
        ] {
            if items.is_empty() {
                continue;
            }
            instructions.push_str(&format!("\n{heading}:"));
            for item in items {
                instructions.push_str("\n- ");
                instructions.push_str(item);
            }
        }
    }
}

fn append_live_task_state(instructions: &mut String, task_run: &TaskRun) {
    instructions.push_str("\n\nPersistent task state (runtime-owned; continue this goal after compaction or restart):");
    instructions.push_str(&format!("\nObjective: {}", task_run.goal.objective));
    instructions.push_str(&format!(
        "\nCompletion condition: {}",
        task_run.goal.completion_condition
    ));
    if !task_run.goal.acceptance_criteria.is_empty() {
        instructions.push_str("\nAcceptance criteria:");
        for criterion in &task_run.goal.acceptance_criteria {
            instructions.push_str("\n- ");
            instructions.push_str(criterion);
        }
    }
    if !task_run.goal.constraints.is_empty() {
        instructions.push_str("\nConstraints:");
        for constraint in &task_run.goal.constraints {
            instructions.push_str("\n- ");
            instructions.push_str(constraint);
        }
    }
    if !task_run.goal.non_goals.is_empty() {
        instructions.push_str("\nNon-goals:");
        for item in &task_run.goal.non_goals {
            instructions.push_str("\n- ");
            instructions.push_str(item);
        }
    }
    if let Some(plan) = &task_run.execution_plan {
        instructions.push_str(&format!(
            "\nExecution plan revision {} ({:?}):",
            plan.revision, plan.status
        ));
        for milestone in &plan.milestones {
            instructions.push_str(&format!(
                "\nMilestone {:?}: {}",
                milestone.status, milestone.title
            ));
            for task in &milestone.tasks {
                instructions.push_str(&format!("\n- [{:?}] {}", task.status, task.description));
            }
            for command in &milestone.validation_commands {
                instructions.push_str(&format!("\n  Validate with: {command}"));
            }
            for criterion in &milestone.completion_criteria {
                instructions.push_str(&format!("\n  Completion criterion: {criterion}"));
            }
        }
        if !plan.decision_notes.is_empty() {
            instructions.push_str("\nPlan decision notes:");
            for note in &plan.decision_notes {
                instructions.push_str("\n- ");
                instructions.push_str(note);
            }
        }
    }
    if let Some(next) = task_run.execution_plan.as_ref().and_then(|plan| {
        plan.milestones
            .iter()
            .flat_map(|milestone| &milestone.tasks)
            .find(|task| task.status != PlanItemStatus::Completed)
    }) {
        instructions.push_str(&format!("\nNext useful planned step: {}", next.description));
    } else if task_run.execution_plan.is_none() {
        instructions.push_str("\nNo explicit plan is needed yet; inspect the request and choose the next useful repository step.");
    }
    if !task_run.remaining_work.is_empty() {
        instructions.push_str("\nRemaining work:");
        for item in &task_run.remaining_work {
            instructions.push_str("\n- ");
            instructions.push_str(item);
        }
    }
}

#[derive(Clone, Copy)]
enum PlanSection {
    Goal,
    Architecture,
    Files,
    Steps,
    Validation,
    Risks,
}

fn plan_section(heading: &str) -> Option<PlanSection> {
    let heading = heading
        .trim_start_matches('#')
        .trim()
        .trim_end_matches(':')
        .trim()
        .to_ascii_lowercase()
        .replace(['_', '-'], " ");
    match heading.as_str() {
        "goal" | "objective" => Some(PlanSection::Goal),
        "relevant architecture" | "architecture" => Some(PlanSection::Architecture),
        "files likely affected" | "likely affected files" | "affected files" => {
            Some(PlanSection::Files)
        }
        "implementation steps" | "steps" | "plan" => Some(PlanSection::Steps),
        "validation" | "tests and validation" | "tests" => Some(PlanSection::Validation),
        "risks/unknowns" | "risks and unknowns" | "risks" | "unknowns" => Some(PlanSection::Risks),
        _ => None,
    }
}

fn plan_item(line: &str) -> String {
    let value = line.trim().trim_start_matches(['-', '*', '•']).trim();
    let value = value
        .split_once(". ")
        .filter(|(prefix, _)| prefix.chars().all(|character| character.is_ascii_digit()))
        .map_or(value, |(_, item)| item.trim());
    bounded_summary(value)
}

fn parse_implementation_plan(
    goal: &str,
    response: &str,
    relevant_files: &[PathBuf],
) -> ImplementationPlan {
    let mut plan = ImplementationPlan {
        goal: goal.to_owned(),
        ..ImplementationPlan::default()
    };
    let mut section = None;
    let mut structured = false;
    for line in response.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(next) = plan_section(line) {
            section = Some(next);
            structured = true;
            let value = line
                .trim_start_matches('#')
                .trim()
                .split_once(':')
                .map(|(_, value)| plan_item(value))
                .unwrap_or_default();
            if !value.is_empty() {
                push_plan_item(&mut plan, next, value);
            }
            continue;
        }
        let Some(section) = section else {
            continue;
        };
        let item = plan_item(line);
        if !item.is_empty() {
            push_plan_item(&mut plan, section, item);
        }
    }
    if !structured {
        plan.implementation_steps = response
            .lines()
            .map(plan_item)
            .filter(|line| !line.is_empty())
            .collect();
    }
    for path in relevant_files {
        let path = path.display().to_string();
        if !plan.files_likely_affected.contains(&path) {
            plan.files_likely_affected.push(path);
        }
    }
    plan
}

const MAX_PLAN_REVISIONS: u32 = 3;

fn parse_goal_details(goal: &str, acceptance_criteria: &[String]) -> Goal {
    let mut parsed = Goal::new(extract_goal_objective(goal));
    parsed.acceptance_criteria = acceptance_criteria.to_vec();
    let mut section = "";
    for line in goal.lines().map(str::trim) {
        let normalized = line
            .trim_start_matches('#')
            .trim()
            .trim_end_matches(':')
            .to_ascii_lowercase();
        if normalized == "acceptance criteria" || normalized == "criteria" {
            section = "acceptance";
            continue;
        }
        if normalized == "constraints" {
            section = "constraints";
            continue;
        }
        if normalized == "non-goals" || normalized == "non goals" {
            section = "non-goals";
            continue;
        }
        if let Some((heading, value)) = line.split_once(':') {
            let heading = heading.trim().trim_start_matches('#').trim();
            if heading.eq_ignore_ascii_case("constraints") {
                section = "constraints";
                if !value.trim().is_empty() {
                    push_unique_bounded(&mut parsed.constraints, plan_item(value), 32);
                }
                continue;
            }
            if heading.eq_ignore_ascii_case("non-goals")
                || heading.eq_ignore_ascii_case("non goals")
            {
                section = "non-goals";
                if !value.trim().is_empty() {
                    push_unique_bounded(&mut parsed.non_goals, plan_item(value), 32);
                }
                continue;
            }
            if heading.eq_ignore_ascii_case("completion condition") {
                parsed.completion_condition = value.trim().to_owned();
                section = "completion";
                continue;
            }
        }
        if line.is_empty() {
            continue;
        }
        if is_list_item(line) {
            let item = plan_item(line);
            match section {
                "constraints" => push_unique_bounded(&mut parsed.constraints, item, 32),
                "non-goals" => push_unique_bounded(&mut parsed.non_goals, item, 32),
                _ => {}
            }
        } else if !line.starts_with("- ") {
            section = "";
        }
    }
    parsed
}

fn extract_goal_objective(goal: &str) -> String {
    let objective = goal
        .lines()
        .take_while(|line| {
            let heading = line
                .trim()
                .trim_start_matches('#')
                .trim()
                .trim_end_matches(':')
                .to_ascii_lowercase();
            !matches!(
                heading.as_str(),
                "acceptance criteria"
                    | "criteria"
                    | "constraints"
                    | "non-goals"
                    | "non goals"
                    | "completion condition"
            ) && !line.split_once(':').is_some_and(|(heading, _)| {
                heading.trim().eq_ignore_ascii_case("completion condition")
            })
        })
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_owned();
    if objective.is_empty() {
        goal.trim().to_owned()
    } else {
        objective
    }
}

fn execution_plan_from_implementation(
    plan: &ImplementationPlan,
    acceptance_criteria: &[String],
) -> ExecutionPlan {
    let tasks = plan
        .implementation_steps
        .iter()
        .filter(|step| !step.trim().is_empty())
        .take(32)
        .map(|description| ExecutionTask {
            description: description.clone(),
            status: PlanItemStatus::Pending,
        })
        .collect::<Vec<_>>();
    let mut completion_criteria = acceptance_criteria.to_vec();
    if completion_criteria.is_empty() {
        completion_criteria
            .push("Complete the planned changes and inspect the final diff.".to_owned());
    }
    let milestones = tasks
        .chunks(4)
        .enumerate()
        .map(|(index, chunk)| ExecutionMilestone {
            title: if tasks.len() <= 4 {
                "Implementation".to_owned()
            } else {
                format!(
                    "Milestone {} · {}",
                    index + 1,
                    bounded_summary(&chunk[0].description)
                )
            },
            tasks: chunk.to_vec(),
            affected_architecture: plan.relevant_architecture.clone(),
            validation_commands: plan.validation.clone(),
            completion_criteria: completion_criteria.clone(),
            status: PlanItemStatus::Pending,
        })
        .collect();
    ExecutionPlan {
        revision: 1,
        milestones,
        decision_notes: plan.risks_or_unknowns.clone(),
        status: PlanItemStatus::Pending,
    }
}

fn sync_current_milestone(task_run: &mut TaskRun) {
    let current = task_run.execution_plan.as_ref().and_then(|plan| {
        plan.milestones
            .iter()
            .find(|milestone| milestone.status != PlanItemStatus::Completed)
            .map(|milestone| milestone.title.clone())
    });
    task_run.goal.current_milestone = current;
}

fn advance_execution_plan(task_run: &mut TaskRun) {
    let Some(plan) = &mut task_run.execution_plan else {
        return;
    };
    plan.status = PlanItemStatus::InProgress;
    if let Some(milestone) = plan
        .milestones
        .iter_mut()
        .find(|milestone| milestone.status != PlanItemStatus::Completed)
    {
        milestone.status = PlanItemStatus::InProgress;
        if let Some(task) = milestone
            .tasks
            .iter_mut()
            .find(|task| task.status == PlanItemStatus::Pending)
        {
            task.status = PlanItemStatus::InProgress;
        }
    }
    sync_current_milestone(task_run);
}

fn record_plan_progress(task_run: &mut TaskRun) {
    let Some(plan) = &mut task_run.execution_plan else {
        return;
    };
    let Some(milestone) = plan
        .milestones
        .iter_mut()
        .find(|milestone| milestone.status != PlanItemStatus::Completed)
    else {
        return;
    };
    if let Some(task) = milestone
        .tasks
        .iter_mut()
        .find(|task| task.status == PlanItemStatus::InProgress)
    {
        task.status = PlanItemStatus::Completed;
    }
    if let Some(task) = milestone
        .tasks
        .iter_mut()
        .find(|task| task.status == PlanItemStatus::Pending)
    {
        task.status = PlanItemStatus::InProgress;
    } else {
        milestone.status = PlanItemStatus::Completed;
    }
    if plan
        .milestones
        .iter()
        .all(|item| item.status == PlanItemStatus::Completed)
    {
        plan.status = PlanItemStatus::Completed;
    }
    sync_current_milestone(task_run);
}

fn finish_plan_state(task_run: &mut TaskRun) {
    let Some(plan) = &mut task_run.execution_plan else {
        return;
    };
    match task_run.completion_status {
        TaskCompletionStatus::Done => {
            for milestone in &mut plan.milestones {
                milestone.status = PlanItemStatus::Completed;
                for task in &mut milestone.tasks {
                    task.status = PlanItemStatus::Completed;
                }
            }
            plan.status = PlanItemStatus::Completed;
        }
        TaskCompletionStatus::Blocked
        | TaskCompletionStatus::UserInputRequired
        | TaskCompletionStatus::ResourceLimitReached => {
            if let Some(milestone) = plan
                .milestones
                .iter_mut()
                .find(|milestone| milestone.status != PlanItemStatus::Completed)
            {
                milestone.status = PlanItemStatus::Blocked;
                if let Some(task) = milestone
                    .tasks
                    .iter_mut()
                    .find(|task| task.status == PlanItemStatus::InProgress)
                {
                    task.status = PlanItemStatus::Blocked;
                }
            }
            plan.status = PlanItemStatus::Blocked;
        }
        TaskCompletionStatus::Cancelled | TaskCompletionStatus::InProgress => {}
    }
    sync_current_milestone(task_run);
}

fn extract_plan_revision(text: &str) -> Option<(String, Vec<String>)> {
    let mut lines = text.lines();
    let revision = lines.find_map(|line| {
        let (heading, reason) = line.trim().split_once(':')?;
        heading
            .trim()
            .eq_ignore_ascii_case("plan revision")
            .then(|| reason.trim().to_owned())
    })?;
    if revision.is_empty() {
        return None;
    }
    let remaining = text
        .lines()
        .skip_while(|line| {
            !line
                .trim()
                .to_ascii_lowercase()
                .starts_with("plan revision:")
        })
        .skip(1)
        .collect::<Vec<_>>()
        .join("\n");
    let plan_start = remaining
        .lines()
        .position(|line| line.trim().eq_ignore_ascii_case("plan:"))?;
    let steps = extract_plan(
        &remaining
            .lines()
            .skip(plan_start)
            .collect::<Vec<_>>()
            .join("\n"),
    )?
    .into_iter()
    .take(32)
    .collect::<Vec<_>>();
    (!steps.is_empty()).then_some((bounded_summary(&revision), steps))
}

fn revise_execution_plan(task_run: &mut TaskRun, reason: &str, steps: &[String]) -> bool {
    if reason.trim().is_empty() || steps.is_empty() {
        return false;
    }
    let Some(existing) = task_run.execution_plan.as_mut() else {
        return false;
    };
    if existing.revision >= MAX_PLAN_REVISIONS.saturating_add(1) {
        return false;
    }
    let old_steps = existing
        .milestones
        .iter()
        .flat_map(|milestone| milestone.tasks.iter().map(|task| task.description.as_str()))
        .collect::<Vec<_>>();
    if old_steps
        .iter()
        .copied()
        .eq(steps.iter().map(String::as_str))
    {
        return false;
    }
    let completed = existing
        .milestones
        .iter()
        .flat_map(|milestone| milestone.tasks.iter())
        .filter(|task| task.status == PlanItemStatus::Completed)
        .map(|task| task.description.clone())
        .collect::<HashSet<_>>();
    let mut affected_architecture = Vec::new();
    let mut validation_commands = Vec::new();
    let mut completion_criteria = task_run.goal.acceptance_criteria.clone();
    for milestone in &existing.milestones {
        for item in &milestone.affected_architecture {
            push_unique_bounded(&mut affected_architecture, item.clone(), 32);
        }
        for command in &milestone.validation_commands {
            push_unique_bounded(&mut validation_commands, command.clone(), 32);
        }
        for criterion in &milestone.completion_criteria {
            push_unique_bounded(&mut completion_criteria, criterion.clone(), 32);
        }
    }
    let milestone = ExecutionMilestone {
        title: "Implementation".to_owned(),
        tasks: steps
            .iter()
            .map(|description| ExecutionTask {
                description: description.clone(),
                status: if completed.contains(description) {
                    PlanItemStatus::Completed
                } else {
                    PlanItemStatus::Pending
                },
            })
            .collect(),
        affected_architecture,
        validation_commands,
        completion_criteria,
        ..ExecutionMilestone::default()
    };
    existing.revision += 1;
    existing.milestones = vec![milestone];
    push_unique_bounded(&mut existing.decision_notes, bounded_summary(reason), 16);
    existing.status = PlanItemStatus::InProgress;
    advance_execution_plan(task_run);
    true
}

fn push_plan_item(plan: &mut ImplementationPlan, section: PlanSection, item: String) {
    let items = match section {
        PlanSection::Goal => return,
        PlanSection::Architecture => &mut plan.relevant_architecture,
        PlanSection::Files => &mut plan.files_likely_affected,
        PlanSection::Steps => &mut plan.implementation_steps,
        PlanSection::Validation => &mut plan.validation,
        PlanSection::Risks => &mut plan.risks_or_unknowns,
    };
    if !items.contains(&item) {
        items.push(item);
    }
}

fn extract_acceptance_criteria(goal: &str) -> Vec<String> {
    let lines = goal.lines().collect::<Vec<_>>();
    let mut criteria = Vec::new();
    let mut in_section = false;
    for line in lines {
        let trimmed = line.trim();
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("acceptance criteria") || lower == "criteria:" {
            in_section = true;
            continue;
        }
        if in_section && trimmed.is_empty() {
            continue;
        }
        if in_section && !is_list_item(trimmed) {
            in_section = false;
        }
        if (in_section || trimmed.starts_with("- [ ]") || trimmed.starts_with("- [x]"))
            && is_list_item(trimmed)
        {
            let item = trimmed
                .trim_start_matches(|character: char| {
                    matches!(character, '-' | '*' | ' ' | '[' | ']' | 'x' | 'X')
                })
                .trim();
            if !item.is_empty() {
                push_unique_bounded(&mut criteria, bounded_summary(item), 32);
            }
        }
    }
    criteria
}

fn is_list_item(line: &str) -> bool {
    line.starts_with("- ")
        || line.starts_with("* ")
        || line.starts_with("+ ")
        || line.starts_with("- [ ]")
        || line.starts_with("- [x]")
        || line.split_once('.').is_some_and(|(number, _)| {
            !number.is_empty() && number.chars().all(|c| c.is_ascii_digit())
        })
}

fn extract_plan(text: &str) -> Option<Vec<String>> {
    let mut found = false;
    let mut plan = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        if !found {
            if let Some((_, remainder)) = trimmed.split_once(':') {
                if trimmed[..trimmed.find(':').unwrap_or_default()].eq_ignore_ascii_case("plan") {
                    found = true;
                    if !remainder.trim().is_empty() {
                        plan.push(bounded_summary(remainder.trim()));
                    }
                    continue;
                }
            }
            continue;
        }
        if trimmed.is_empty() {
            break;
        }
        if trimmed.ends_with(':') && !is_list_item(trimmed) {
            break;
        }
        let item = trimmed.trim_start_matches(['-', '*', '+', ' ']).trim();
        if !item.is_empty() {
            push_unique_bounded(&mut plan, bounded_summary(item), 16);
        }
    }
    found.then_some(plan)
}

fn parse_completion_directive(text: &str) -> (Option<TaskCompletionStatus>, String) {
    let trimmed = text.trim();
    for (marker, status) in [
        (
            "[USER_INPUT_REQUIRED]",
            TaskCompletionStatus::UserInputRequired,
        ),
        ("[BLOCKED]", TaskCompletionStatus::Blocked),
    ] {
        if let Some(message) = trimmed.strip_prefix(marker) {
            return (Some(status), message.trim().to_owned());
        }
    }
    (None, text.to_owned())
}

fn parse_unrelated_verification_directives(text: &str) -> (String, Vec<(String, String)>) {
    const MARKER: &str = "[UNRELATED_VERIFICATION]";
    let mut message = Vec::new();
    let mut dispositions = Vec::new();
    for line in text.lines() {
        let trimmed = line.trim();
        let Some(value) = trimmed.strip_prefix(MARKER) else {
            message.push(line);
            continue;
        };
        let Some((command, reason)) = value.trim().split_once(" :: ") else {
            message.push(line);
            continue;
        };
        let command = command.trim();
        let reason = reason.trim();
        if !command.is_empty() && !reason.is_empty() {
            dispositions.push((command.to_owned(), reason.to_owned()));
        } else {
            message.push(line);
        }
    }
    (message.join("\n").trim().to_owned(), dispositions)
}

fn apply_unrelated_verification_dispositions(
    task_run: &mut TaskRun,
    unresolved_error_keys: &mut HashMap<String, String>,
    dispositions: &[(String, String)],
    validation_since_last_edit: &mut bool,
) {
    let mut applied = false;
    for (command, reason) in dispositions {
        let key = format!("verification:{command}");
        if unresolved_error_keys.remove(&key).is_none() {
            continue;
        }
        applied = true;
        if let Some(result) = task_run
            .verification_results
            .iter_mut()
            .rev()
            .find(|result| result.command == *command && !result.passed)
        {
            result.failure_origin = Some("unrelated".to_owned());
            result.summary = bounded_summary(&format!(
                "{} | agent classified as unrelated: {}",
                result.summary, reason
            ));
        }
    }
    if applied {
        unresolved_error_keys.remove("verification:after_last_edit");
        *validation_since_last_edit = true;
    }
    task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
}

fn tool_call_key(name: &str, arguments: &serde_json::Value) -> String {
    format!(
        "{name}:{}",
        serde_json::to_string(arguments).unwrap_or_else(|_| "<invalid-json>".to_owned())
    )
}

fn normalize_failure(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_ascii_lowercase()
}

fn collect_relevant_files(task_run: &mut TaskRun, arguments: &serde_json::Value) {
    fn visit(task_run: &mut TaskRun, value: &serde_json::Value) {
        match value {
            serde_json::Value::Object(object) => {
                for (key, value) in object {
                    if matches!(key.as_str(), "path" | "file" | "file_path" | "target_file") {
                        if let Some(path) = value.as_str().filter(|path| !path.trim().is_empty()) {
                            push_unique_bounded(
                                &mut task_run.relevant_files,
                                PathBuf::from(path),
                                200,
                            );
                        }
                    } else {
                        visit(task_run, value);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for item in items {
                    visit(task_run, item);
                }
            }
            _ => {}
        }
    }
    visit(task_run, arguments);
}

fn phase_for_tool(name: &str, arguments: &serde_json::Value) -> TaskPhase {
    let name = name.to_ascii_lowercase();
    if name.contains("diff") || name.contains("git_status") {
        return TaskPhase::InspectDiff;
    }
    if name.contains("write") || name.contains("patch") || name.contains("edit") {
        return TaskPhase::Edit;
    }
    if name == "shell" {
        let command = arguments
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_ascii_lowercase();
        if ["test", "check", "lint", "build", "fmt", "format"]
            .iter()
            .any(|word| command.contains(word))
        {
            return TaskPhase::Verify;
        }
        if command.contains("git diff") {
            return TaskPhase::InspectDiff;
        }
    }
    TaskPhase::SearchRead
}

fn is_validation_command(command: &str) -> bool {
    command
        .split(|character: char| !character.is_ascii_alphanumeric() && character != '_')
        .take(5)
        .map(str::to_ascii_lowercase)
        .any(|word| {
            matches!(
                word.as_str(),
                "test"
                    | "tests"
                    | "check"
                    | "lint"
                    | "build"
                    | "format"
                    | "fmt"
                    | "typecheck"
                    | "verify"
                    | "compile"
                    | "pytest"
                    | "vitest"
                    | "jest"
            )
        })
}

fn estimate_cost_microusd(usage: Option<&Usage>, pricing: Option<&ModelPricing>) -> Option<u64> {
    let usage = usage?;
    let pricing = pricing?;
    let input_tokens = u64::from(usage.input_tokens?);
    let output_tokens = u64::from(usage.output_tokens?);
    let input_rate = decimal_microusd(pricing.input_usd_per_million_tokens.as_deref()?)?;
    let output_rate = decimal_microusd(pricing.output_usd_per_million_tokens.as_deref()?)?;
    Some(
        input_rate
            .saturating_mul(input_tokens)
            .saturating_add(output_rate.saturating_mul(output_tokens))
            / 1_000_000,
    )
}

fn decimal_microusd(value: &str) -> Option<u64> {
    let (whole, fraction) = value.trim().split_once('.').unwrap_or((value.trim(), ""));
    let whole = whole.parse::<u64>().ok()?;
    let fraction = fraction.chars().take(6).collect::<String>();
    let fraction_value = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u64>().ok()? * 10_u64.pow(6_u32.saturating_sub(fraction.len() as u32))
    };
    whole.checked_mul(1_000_000)?.checked_add(fraction_value)
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct AgentTask {
    pub workspace_root: PathBuf,
    pub user_task: String,
    #[serde(default)]
    pub task_mode: TaskMode,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
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
    pub estimated_cost_microusd: Option<u64>,
    pub completion_status: TaskCompletionStatus,
    pub remaining_work: Vec<String>,
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
    checkpoints: Option<Arc<dyn CheckpointStore>>,
    event_bus: EventBus,
    reasoning: Option<ReasoningConfig>,
}

struct ToolExecutionContext<'a> {
    session_id: &'a SessionId,
    workspace_root: &'a std::path::Path,
    policy: &'a ApprovedPolicy,
    approved: &'a Arc<Mutex<HashSet<String>>>,
    collector: &'a Arc<Mutex<Vec<HarnessEvent>>>,
    cancellation: &'a CancellationToken,
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
            checkpoints: None,
            event_bus: EventBus::new(),
            reasoning: None,
        }
    }

    pub fn with_event_bus(mut self, event_bus: EventBus) -> Self {
        self.event_bus = event_bus;
        self
    }

    pub fn with_reasoning_config(mut self, reasoning: Option<ReasoningConfig>) -> Self {
        self.reasoning = reasoning;
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

    pub fn with_checkpoints(mut self, checkpoints: Arc<dyn CheckpointStore>) -> Self {
        self.checkpoints = Some(checkpoints);
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
        // Desktop and other RPC clients may omit a plan. Discover the project
        // here so verification remains runtime-owned and works consistently
        // for every client.
        let discovered_workspace = discover_workspace(&task.workspace_root).ok();
        let verification_plan = task
            .verification_plan
            .clone()
            .or_else(|| discovered_workspace.as_ref().map(VerificationPlan::all));
        let git_available = discovered_workspace.as_ref().is_some_and(|workspace| {
            workspace.git.available && workspace.repository_root.is_some()
        });
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
        let previous_task_run = previous_state.task_run.clone();
        // A completed PLAN is still useful when the user explicitly resumes it
        // in CODE mode. Keep its discoveries and structured plan as context.
        let approved_plan_continuation = task.task_mode == TaskMode::Code
            && task.resume_session.is_some()
            && previous_task_run.as_ref().is_some_and(|run| {
                run.task_mode == TaskMode::Plan && run.structured_plan.is_some()
            });
        let mut task_run = previous_task_run
            .filter(|run| {
                run.completion_status != TaskCompletionStatus::Done || approved_plan_continuation
            })
            .unwrap_or_else(|| TaskRun::new(task.user_task.clone()));
        let resumed_task_state = task.resume_session.as_ref().map(|_| task_run.clone());
        if task_run.original_goal.is_empty() {
            task_run.original_goal.clone_from(&task.user_task);
        }
        if task_run.goal.objective.trim().is_empty() {
            task_run.goal =
                parse_goal_details(&task_run.original_goal, &task_run.acceptance_criteria);
        }
        if !task.acceptance_criteria.is_empty() {
            task_run.acceptance_criteria = task.acceptance_criteria.clone();
        } else if task_run.acceptance_criteria.is_empty() {
            task_run.acceptance_criteria = extract_acceptance_criteria(&task.user_task);
        }
        task_run.goal.objective = extract_goal_objective(&task_run.original_goal);
        task_run.goal.acceptance_criteria = task_run.acceptance_criteria.clone();
        let parsed_goal =
            parse_goal_details(&task_run.original_goal, &task_run.acceptance_criteria);
        if task_run.goal.constraints.is_empty() {
            task_run.goal.constraints = parsed_goal.constraints;
        }
        if task_run.goal.non_goals.is_empty() {
            task_run.goal.non_goals = parsed_goal.non_goals;
        }
        if task_run.goal.completion_condition.trim().is_empty()
            || parsed_goal.completion_condition != Goal::new("").completion_condition
        {
            task_run.goal.completion_condition = parsed_goal.completion_condition;
        }
        if task_run.execution_plan.is_none() {
            if let Some(plan) = &task_run.structured_plan {
                task_run.execution_plan = Some(execution_plan_from_implementation(
                    plan,
                    &task_run.acceptance_criteria,
                ));
            } else if !task_run.current_plan.is_empty() {
                let legacy_plan = ImplementationPlan {
                    goal: task_run.original_goal.clone(),
                    implementation_steps: task_run.current_plan.clone(),
                    ..ImplementationPlan::default()
                };
                task_run.execution_plan = Some(execution_plan_from_implementation(
                    &legacy_plan,
                    &task_run.acceptance_criteria,
                ));
            }
        }
        task_run.task_mode = task.task_mode;
        advance_execution_plan(&mut task_run);
        sync_current_milestone(&mut task_run);
        task_run.completion_status = TaskCompletionStatus::InProgress;
        task_run.current_phase = TaskPhase::Understand;
        task_run.remaining_work.clear();
        if task.resume_session.is_some() {
            task_run
                .unresolved_errors
                .retain(|error| !error.starts_with("runtime:guard:"));
        }
        let mut system_instructions = task_mode_instructions(
            coding_agent_instructions(&task.system_instructions),
            task.task_mode,
        );
        append_acceptance_criteria(&mut system_instructions, &task_run.acceptance_criteria);
        if let Some(previous) = &resumed_task_state {
            append_prior_task_state(&mut system_instructions, previous);
        }
        let base_system_instructions = system_instructions.clone();
        let checkpoint_id = if let Some(checkpoints) = &self.checkpoints {
            Some(
                checkpoints
                    .create(&session_id, &task.workspace_root)
                    .map_err(|error| AgentError::Core(error.to_string()))?
                    .id,
            )
        } else {
            None
        };
        let mut workspace_metadata = task.workspace.clone();
        if workspace_metadata.root.is_none() {
            workspace_metadata.root = Some(task.workspace_root.clone());
        }
        if let Some(root) = &workspace_metadata.root {
            if !workspace_metadata.details.contains_key("repository_map") {
                if let Ok(repository_map) = self.tools.repository_map(root) {
                    workspace_metadata
                        .details
                        .insert("repository_map".to_owned(), repository_map);
                }
            }
        }
        let collector = Arc::new(Mutex::new(Vec::new()));
        let collected = Arc::clone(&collector);
        let collected_ids = Arc::new(Mutex::new(HashSet::new()));
        let collected_event_ids = Arc::clone(&collected_ids);
        let collected_session = session_id.clone();
        let _subscription = self
            .event_bus
            .subscribe(Arc::new(move |event: &HarnessEvent| {
                if event.session_id == collected_session
                    && collected_event_ids
                        .lock()
                        .expect("agent event ID lock poisoned")
                        .insert(event.event_id.clone())
                {
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
        let mut instructions = task.instructions.clone();
        if let Some(discovered) = &discovered_workspace {
            for instruction in &discovered.instructions {
                if !instructions
                    .iter()
                    .any(|existing| existing.path == instruction.path)
                {
                    instructions.push(instruction.clone());
                }
            }
        }
        let mut context_input = ContextInput {
            system_instructions: base_system_instructions.clone(),
            workspace: workspace_metadata,
            instructions,
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
        self.update_task_run(&session_id, &collector, &task_run)?;
        let mut turns = 0;
        let mut tool_calls = 0;
        let mut model_tokens = 0;
        let mut estimated_cost_microusd = Some(0_u64);
        let mut repeated_tool_calls = HashMap::<String, u32>::new();
        let mut repeated_failures = HashMap::<String, u32>::new();
        let mut unresolved_error_keys = task_run
            .unresolved_errors
            .iter()
            .filter_map(|error| error.split_once(" :: "))
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect::<HashMap<_, _>>();
        let mut no_progress_final_responses = 0_u32;
        let mut validation_since_last_edit =
            task.task_mode != TaskMode::Code || task_run.changed_files.is_empty();
        // Keep the latest canonical call/result batch for native model
        // protocols that require explicit tool-result messages. The rebuilt
        // context carries older observations without retaining an unbounded
        // raw tool transcript. Provider-private signatures stay in adapters.
        let mut model_history = Vec::<Message>::new();
        loop {
            self.check_limits(
                started_at,
                turns,
                tool_calls,
                model_tokens,
                cancellation,
                &session_id,
                &collector,
                &mut task_run,
                estimated_cost_microusd,
            )?;
            turns += 1;
            task_run.current_phase = if task_run.current_plan.is_empty() {
                TaskPhase::Understand
            } else {
                TaskPhase::Plan
            };
            self.update_task_run(&session_id, &collector, &task_run)?;
            context_input.system_instructions = base_system_instructions.clone();
            append_live_task_state(&mut context_input.system_instructions, &task_run);
            let mut assembly = match self.context_builder.build(&context_input) {
                Ok(assembly) => assembly,
                Err(error) => {
                    return self.fail(session_id, collector, AgentError::Core(error.to_string()))
                }
            };
            if self.compaction_config.threshold_tokens > 0
                && assembly.estimated_tokens >= self.compaction_config.threshold_tokens
            {
                self.compact_context(&session_id, &mut context_input, &collector, &task_run)?;
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
            let context_message = Message::user_text(assembly.prompt);
            let mut model_messages = Vec::with_capacity(model_history.len() + 1);
            model_messages.push(context_message);
            model_messages.extend(model_history.iter().cloned());
            let model_request = ModelRequest {
                model: self.model.clone(),
                messages: model_messages,
                tools: self
                    .tools
                    .specs()
                    .into_iter()
                    .filter(|spec| {
                        task_mode_allows_tool(task.task_mode, self.tools.operation_for(&spec.name))
                    })
                    .map(|spec| harness_models::ToolDefinition {
                        name: spec.name,
                        description: spec.description,
                        input_schema: spec.arguments_schema,
                    })
                    .collect(),
                max_output_tokens: None,
                temperature: None,
                metadata: Default::default(),
                reasoning: self.reasoning.clone(),
            };
            let descriptor = self.provider.descriptor();
            self.emit(
                &session_id,
                EventPayload::ModelRequested {
                    provider: descriptor.provider.clone(),
                    model: self.model.clone(),
                    prompt_tokens: None,
                },
                &collector,
            )?;
            let response = match self.provider.generate_cancellable(
                &model_request,
                &mut |event| {
                    if cancellation.is_cancelled() {
                        return Err(ProviderError::StreamConsumer);
                    }
                    if let ModelStreamEvent::TextDelta { text } = event {
                        self.event_bus.publish(&HarnessEvent::new(
                            session_id.clone(),
                            EventPayload::AssistantDelta { text },
                            None,
                            None,
                        ));
                    }
                    Ok(())
                },
                &|| cancellation.is_cancelled(),
            ) {
                Ok(response) => response,
                Err(_) if cancellation.is_cancelled() => {
                    task_run.completion_status = TaskCompletionStatus::Cancelled;
                    task_run.current_phase = TaskPhase::Finish;
                    return self.abort_with_task_run(
                        session_id,
                        collector,
                        &task_run,
                        AgentError::Cancelled,
                    );
                }
                Err(error) => {
                    let error = AgentError::Model(error);
                    task_run.unresolved_errors.push(format!(
                        "runtime:provider :: {}",
                        bounded_summary(&error.to_string())
                    ));
                    task_run.completion_status = TaskCompletionStatus::Blocked;
                    task_run.current_phase = TaskPhase::Finish;
                    return self.abort_with_task_run(session_id, collector, &task_run, error);
                }
            };
            self.flush(&session_id, &collector)?;
            if unresolved_error_keys.remove("runtime:provider").is_some() {
                task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
            }
            let plan_revision = extract_plan_revision(&response.text());
            if let Some((reason, steps)) = &plan_revision {
                if revise_execution_plan(&mut task_run, reason, steps) {
                    task_run.current_plan = steps.clone();
                    task_run.current_phase = TaskPhase::Plan;
                    self.update_task_run(&session_id, &collector, &task_run)?;
                }
            } else if task_run.execution_plan.is_none() {
                if let Some(plan_steps) =
                    extract_plan(&response.text()).filter(|plan| !plan.is_empty())
                {
                    task_run.current_plan = plan_steps.clone();
                    let mut parsed = parse_implementation_plan(
                        &task_run.original_goal,
                        &response.text(),
                        &task_run.relevant_files,
                    );
                    parsed.implementation_steps = plan_steps;
                    task_run.execution_plan = Some(execution_plan_from_implementation(
                        &parsed,
                        &task_run.acceptance_criteria,
                    ));
                    advance_execution_plan(&mut task_run);
                    task_run.current_phase = TaskPhase::Plan;
                    self.update_task_run(&session_id, &collector, &task_run)?;
                }
            }
            model_tokens += usage_tokens(response.usage.as_ref());
            let response_cost = estimate_cost_microusd(
                response.usage.as_ref(),
                descriptor.metadata.pricing.as_ref(),
            );
            estimated_cost_microusd = match (estimated_cost_microusd, response_cost) {
                (Some(total), Some(cost)) => Some(total.saturating_add(cost)),
                _ => None,
            };
            self.emit(
                &session_id,
                EventPayload::ModelResponse {
                    provider: descriptor.provider,
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
                context_input.conversation.push(SessionConversationMessage {
                    role: MessageRole::Assistant,
                    text: response.text(),
                });
            }
            if !response.text().is_empty() && !response.tool_calls.is_empty() {
                self.emit(
                    &session_id,
                    EventPayload::AssistantMessage {
                        text: response.text(),
                    },
                    &collector,
                )?;
            }
            self.check_limits(
                started_at,
                turns,
                tool_calls,
                model_tokens,
                cancellation,
                &session_id,
                &collector,
                &mut task_run,
                estimated_cost_microusd,
            )?;
            if !response.tool_calls.is_empty() && turns >= self.limits.max_turns {
                task_run.completion_status = TaskCompletionStatus::ResourceLimitReached;
                task_run.current_phase = TaskPhase::Finish;
                return self.abort_with_task_run(
                    session_id,
                    collector,
                    &task_run,
                    AgentError::LimitExceeded {
                        limit: "max_turns".to_owned(),
                    },
                );
            }
            if response.tool_calls.is_empty() {
                let (directive, completion_text) = parse_completion_directive(&response.text());
                let (final_message, unrelated_verification) =
                    parse_unrelated_verification_directives(&completion_text);
                if task_run.execution_plan.is_none() {
                    if let Some(plan_steps) =
                        extract_plan(&response.text()).filter(|plan| !plan.is_empty())
                    {
                        task_run.current_plan = plan_steps.clone();
                        let mut parsed = parse_implementation_plan(
                            &task_run.original_goal,
                            &response.text(),
                            &task_run.relevant_files,
                        );
                        parsed.implementation_steps = plan_steps;
                        task_run.execution_plan = Some(execution_plan_from_implementation(
                            &parsed,
                            &task_run.acceptance_criteria,
                        ));
                        advance_execution_plan(&mut task_run);
                    }
                }
                if directive == Some(TaskCompletionStatus::UserInputRequired) {
                    task_run.current_phase = TaskPhase::Finish;
                    task_run.completion_status = TaskCompletionStatus::UserInputRequired;
                    task_run.remaining_work = vec![final_message.clone()];
                    self.emit(
                        &session_id,
                        EventPayload::AssistantMessage {
                            text: final_message.clone(),
                        },
                        &collector,
                    )?;
                    return self.finish_task_run(
                        session_id,
                        collector,
                        task_run,
                        final_message,
                        turns,
                        tool_calls,
                        model_tokens,
                        estimated_cost_microusd,
                    );
                }
                if directive == Some(TaskCompletionStatus::Blocked) {
                    let reason = if final_message.is_empty() {
                        "The task cannot be completed with the available information.".to_owned()
                    } else {
                        final_message.clone()
                    };
                    task_run.current_phase = TaskPhase::Finish;
                    return self.finish_blocked_task(
                        session_id,
                        collector,
                        task_run,
                        reason,
                        turns,
                        tool_calls,
                        model_tokens,
                        estimated_cost_microusd,
                    );
                }
                if task.task_mode == TaskMode::Plan {
                    let plan = parse_implementation_plan(
                        &task_run.original_goal,
                        &response.text(),
                        &task_run.relevant_files,
                    );
                    if task_run.structured_plan.is_none() {
                        task_run.current_plan = plan.implementation_steps.clone();
                        task_run.execution_plan = Some(execution_plan_from_implementation(
                            &plan,
                            &task_run.acceptance_criteria,
                        ));
                        advance_execution_plan(&mut task_run);
                        task_run.structured_plan = Some(plan);
                    }
                    task_run.current_phase = TaskPhase::Finish;
                    self.update_task_run(&session_id, &collector, &task_run)?;
                }
                apply_unrelated_verification_dispositions(
                    &mut task_run,
                    &mut unresolved_error_keys,
                    &unrelated_verification,
                    &mut validation_since_last_edit,
                );
                if task.task_mode == TaskMode::Code
                    && git_available
                    && self.verifier.is_some()
                    && !task_run.changed_files.is_empty()
                    && !task_run.final_diff_inspected
                {
                    let diff_plan = VerificationPlan {
                        steps: vec![VerificationStep {
                            category: VerificationCategory::GitDiff,
                            command: CommandSpec::new(
                                "git",
                                ["diff", "--no-ext-diff", "--no-color"],
                            ),
                            source: "required-final-diff-inspection".to_owned(),
                        }],
                    };
                    let changed_files = task_run.changed_files.clone();
                    let before = task_run.verification_results.len();
                    self.run_verification_if_needed(
                        &session_id,
                        &task.workspace_root,
                        Some(&diff_plan),
                        self.verifier.as_ref(),
                        &changed_files,
                        &mut context_input,
                        &collector,
                        cancellation,
                        &mut task_run,
                        &mut unresolved_error_keys,
                        &mut repeated_failures,
                        self.limits.max_repeated_failures,
                    )?;
                    task_run.final_diff_inspected = task_run.verification_results[before..]
                        .iter()
                        .any(|result| result.category == "GitDiff" && result.passed);
                    self.update_task_run(&session_id, &collector, &task_run)?;
                    if task_run.final_diff_inspected {
                        // The next model turn receives the final diff output
                        // before it can declare the task complete.
                        continue;
                    }
                }
                if task.task_mode == TaskMode::Code
                    && !task_run.changed_files.is_empty()
                    && !task_run.final_diff_inspected
                {
                    let key = "verification:final_diff_inspection".to_owned();
                    unresolved_error_keys.entry(key.clone()).or_insert_with(|| {
                        format!(
                            "{key} :: A final diff could not be assembled for the changed files. Inspect the latest file edits before finishing."
                        )
                    });
                    task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
                }
                if task.task_mode == TaskMode::Code
                    && !task_run.changed_files.is_empty()
                    && !validation_since_last_edit
                    && !task_run.final_diff_inspected
                {
                    let key = "verification:after_last_edit".to_owned();
                    unresolved_error_keys.entry(key.clone()).or_insert_with(|| {
                        format!(
                            "{key} :: No successful relevant verification has run since the most recent edit. Run the appropriate test, build, lint, format, or typecheck command before finishing."
                        )
                    });
                    task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
                }
                if !task_run.unresolved_errors.is_empty() {
                    no_progress_final_responses = no_progress_final_responses.saturating_add(1);
                    task_run.current_phase = TaskPhase::Repair;
                    task_run.remaining_work = task_run.unresolved_errors.clone();
                    if no_progress_final_responses >= self.limits.max_repeated_failures.max(1) {
                        let reason = format!(
                            "The model stopped making progress with unresolved verification or tool errors: {}",
                            task_run.unresolved_errors.join("; ")
                        );
                        return self.finish_blocked_task(
                            session_id,
                            collector,
                            task_run,
                            reason,
                            turns,
                            tool_calls,
                            model_tokens,
                            estimated_cost_microusd,
                        );
                    }
                    context_input.tool_results.push(ToolContextResult {
                        name: "task_completion_check".to_owned(),
                        result: tool_error_result(
                            "task_completion_check".to_owned(),
                            format!(
                                "Do not report completion. Resolve or explain these outstanding issues, revise your approach, and continue with tools when possible: {}. If the task is impossible, return [BLOCKED] followed by the reason; if a user decision is needed, return [USER_INPUT_REQUIRED] followed by one concise question.",
                                task_run.unresolved_errors.join("; ")
                            ),
                        ),
                        is_shell: false,
                    });
                    self.update_task_run(&session_id, &collector, &task_run)?;
                    continue;
                }
                task_run.current_phase = TaskPhase::Finish;
                task_run.completion_status = TaskCompletionStatus::Done;
                task_run.remaining_work.clear();
                self.emit(
                    &session_id,
                    EventPayload::AssistantMessage {
                        text: final_message.clone(),
                    },
                    &collector,
                )?;
                return self.finish_task_run(
                    session_id,
                    collector,
                    task_run,
                    final_message,
                    turns,
                    tool_calls,
                    model_tokens,
                    estimated_cost_microusd,
                );
            }
            let mut next_model_history = Vec::with_capacity(response.tool_calls.len() + 1);
            next_model_history.push(Message {
                role: Role::Assistant,
                content: response.content.clone(),
                name: None,
                tool_call_id: None,
                tool_calls: response.tool_calls.clone(),
                is_error: false,
            });
            for tool_call in response.tool_calls {
                if cancellation.is_cancelled() {
                    task_run.completion_status = TaskCompletionStatus::Cancelled;
                    task_run.current_phase = TaskPhase::Finish;
                    return self.abort_with_task_run(
                        session_id,
                        collector,
                        &task_run,
                        AgentError::Cancelled,
                    );
                }
                if tool_calls >= self.limits.max_tool_calls {
                    task_run.completion_status = TaskCompletionStatus::ResourceLimitReached;
                    task_run.current_phase = TaskPhase::Finish;
                    return self.abort_with_task_run(
                        session_id,
                        collector,
                        &task_run,
                        AgentError::LimitExceeded {
                            limit: "max_tool_calls".to_owned(),
                        },
                    );
                }
                tool_calls += 1;
                let is_shell = tool_call.name == "shell";
                let tool_error_key = format!("tool:{}", tool_call.name);
                let call_key = tool_call_key(&tool_call.name, &tool_call.arguments);
                let repeated = repeated_tool_calls.entry(call_key.clone()).or_default();
                *repeated = repeated.saturating_add(1);
                if *repeated > self.limits.max_repeated_tool_calls.max(1) {
                    let message = format!(
                        "Stopped a repeated identical tool call after {} attempts: {}",
                        repeated.saturating_sub(1),
                        tool_call.name
                    );
                    unresolved_error_keys.insert(
                        "runtime:guard:tool_call".to_owned(),
                        format!("runtime:guard:tool_call :: {message}"),
                    );
                    task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
                    task_run.remaining_work = task_run.unresolved_errors.clone();
                    task_run.completion_status = TaskCompletionStatus::Blocked;
                    task_run.current_phase = TaskPhase::Finish;
                    return self.finish_blocked_task(
                        session_id,
                        collector,
                        task_run,
                        message,
                        turns,
                        tool_calls,
                        model_tokens,
                        estimated_cost_microusd,
                    );
                }
                task_run.current_phase = phase_for_tool(&tool_call.name, &tool_call.arguments);
                collect_relevant_files(&mut task_run, &tool_call.arguments);
                if is_shell {
                    if let Some(command) =
                        tool_call.arguments.get("command").and_then(|v| v.as_str())
                    {
                        push_bounded(
                            &mut task_run.commands_executed,
                            bounded_summary(command),
                            200,
                        );
                    }
                }
                self.update_task_run(&session_id, &collector, &task_run)?;
                let request = ToolRequest::new(tool_call.name.clone(), tool_call.arguments.clone());
                let execution = ToolExecutionContext {
                    session_id: &session_id,
                    workspace_root: &task.workspace_root,
                    policy: &approval_policy,
                    approved: &approved,
                    collector: &collector,
                    cancellation,
                };
                let operation = self.tools.operation_for(&tool_call.name);
                let task_mode_denial = (!task_mode_allows_tool(task.task_mode, operation)).then(|| {
                    format!(
                        "Task mode {:?} is read-only and only allows workspace read/search tools. No command or file change was made.",
                        task.task_mode
                    )
                });
                let mut result = if let Some(reason) = &task_mode_denial {
                    self.emit(
                        &session_id,
                        EventPayload::ToolDenied {
                            tool: tool_call.name.clone(),
                            reason: reason.clone(),
                        },
                        &collector,
                    )?;
                    let mut result = ToolResult::new(reason.clone());
                    result.is_error = true;
                    result
                } else {
                    match self.execute_tool(&execution, request) {
                        Ok(result) => result,
                        Err(error) => {
                            unresolved_error_keys.insert(
                                tool_error_key.clone(),
                                format!(
                                    "{tool_error_key} :: {}",
                                    bounded_summary(&error.to_string())
                                ),
                            );
                            task_run.unresolved_errors =
                                unresolved_error_keys.values().cloned().collect();
                            task_run.completion_status = TaskCompletionStatus::Blocked;
                            task_run.current_phase = TaskPhase::Finish;
                            return self
                                .abort_with_task_run(session_id, collector, &task_run, error);
                        }
                    }
                };
                let (bounded_output, output_truncated) =
                    bound_tool_result_output(&result.output, result.truncated);
                result.output = bounded_output;
                result.truncated = output_truncated;
                let changed_files = result.changed_files.clone();
                let mut meaningful_plan_progress = !result.is_error && task_mode_denial.is_none();
                let quick_diagnostic = quick_diagnostic_result(&result, &changed_files);
                let final_edit_diff = (!git_available || self.verifier.is_none())
                    .then(|| complete_edit_diff(&result, &changed_files))
                    .flatten();
                if result.is_error && task_mode_denial.is_none() {
                    let error = format!("{tool_error_key} :: {}", bounded_summary(&result.output));
                    unresolved_error_keys.insert(tool_error_key, error);
                    let signature = format!("{}:{}", call_key, normalize_failure(&result.output));
                    let failures = repeated_failures.entry(signature).or_default();
                    *failures = failures.saturating_add(1);
                    task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
                    task_run.current_phase = TaskPhase::Repair;
                    if *failures >= self.limits.max_repeated_failures.max(1) {
                        let reason = format!(
                            "The same tool failure repeated {} times: {}",
                            failures,
                            bounded_summary(&result.output)
                        );
                        task_run.remaining_work = task_run.unresolved_errors.clone();
                        return self.finish_blocked_task(
                            session_id,
                            collector,
                            task_run,
                            reason,
                            turns,
                            tool_calls,
                            model_tokens,
                            estimated_cost_microusd,
                        );
                    }
                } else if task_mode_denial.is_none() {
                    unresolved_error_keys.remove(&tool_error_key);
                    task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
                    no_progress_final_responses = 0;
                    if !changed_files.is_empty() {
                        task_run.final_diff_inspected = false;
                        task_run.current_phase = TaskPhase::Verify;
                    }
                }
                if is_shell {
                    if let Some(command) =
                        tool_call.arguments.get("command").and_then(|v| v.as_str())
                    {
                        if is_validation_command(command) {
                            let passed = !result.is_error;
                            let exit_code = result
                                .metadata
                                .get("exit_code")
                                .and_then(serde_json::Value::as_i64)
                                .and_then(|code| i32::try_from(code).ok());
                            push_bounded(
                                &mut task_run.verification_results,
                                TaskVerificationResult {
                                    command: bounded_summary(command),
                                    category: "agent_command".to_owned(),
                                    passed,
                                    exit_code,
                                    summary: bounded_summary(&result.output),
                                    affected_files: Vec::new(),
                                    failure_origin: (!passed).then(|| "unknown".to_owned()),
                                    relevant_output: bounded_context(&result.output, 8 * 1024),
                                },
                                200,
                            );
                            let key = format!("verification:{command}");
                            if passed {
                                meaningful_plan_progress = true;
                                unresolved_error_keys.remove(&key);
                                unresolved_error_keys.remove("verification:after_last_edit");
                                validation_since_last_edit = true;
                                if command.to_ascii_lowercase().contains("git diff") {
                                    task_run.final_diff_inspected = true;
                                }
                            } else {
                                unresolved_error_keys.insert(
                                    key.clone(),
                                    format!("{key} :: {}", bounded_summary(&result.output)),
                                );
                                validation_since_last_edit = false;
                            }
                        }
                    }
                }
                for path in &changed_files {
                    push_unique_bounded(&mut task_run.changed_files, path.clone(), 200);
                }
                if !changed_files.is_empty() {
                    validation_since_last_edit = false;
                }
                if let Some((command, passed, affected_files, output)) = quick_diagnostic {
                    let key = format!("verification:{command}");
                    let output = bounded_context(&output, 8 * 1024);
                    push_bounded(&mut task_run.commands_executed, command.clone(), 200);
                    push_bounded(
                        &mut task_run.verification_results,
                        TaskVerificationResult {
                            command: command.clone(),
                            category: "diagnostics".to_owned(),
                            passed,
                            exit_code: None,
                            summary: bounded_summary(&output),
                            affected_files: affected_files.clone(),
                            failure_origin: (!passed).then(|| "unknown".to_owned()),
                            relevant_output: output.clone(),
                        },
                        200,
                    );
                    self.emit(
                        &session_id,
                        EventPayload::VerificationResult {
                            command: command.clone(),
                            category: "diagnostics".to_owned(),
                            duration_ms: 0,
                            passed,
                            exit_code: None,
                            output: output.clone(),
                            diagnostics: output.lines().map(str::to_owned).take(100).collect(),
                            affected_files,
                            failure_origin: (!passed).then(|| "unknown".to_owned()),
                            relevant_output: output.clone(),
                        },
                        &collector,
                    )?;
                    context_input.tool_results.push(ToolContextResult {
                        name: "verification".to_owned(),
                        result: ToolResult::new(format!(
                            "verification category=diagnostics command={command} passed={passed} affected_files={:?}\n{output}",
                            changed_files
                        )),
                        is_shell: false,
                    });
                    if passed {
                        meaningful_plan_progress = true;
                        unresolved_error_keys.remove(&key);
                        unresolved_error_keys.remove("verification:after_last_edit");
                        validation_since_last_edit = true;
                    } else {
                        validation_since_last_edit = false;
                        unresolved_error_keys.insert(
                            key.clone(),
                            format!("{key} :: {}", bounded_summary(&output)),
                        );
                    }
                }
                if let (Some(checkpoints), Some(checkpoint_id)) =
                    (&self.checkpoints, &checkpoint_id)
                {
                    for path in &changed_files {
                        checkpoints
                            .record_harness_change(checkpoint_id, path)
                            .map_err(|error| AgentError::Core(error.to_string()))?;
                    }
                }
                next_model_history.push(Message::tool_result(harness_models::ToolResult {
                    tool_call_id: tool_call.id,
                    content: result.output.clone(),
                    is_error: result.is_error,
                }));
                context_input.tool_results.push(ToolContextResult {
                    name: tool_call.name,
                    result,
                    is_shell,
                });
                if let Some(diff) = final_edit_diff {
                    let command = "inspect final edit diff".to_owned();
                    let relevant_output = bounded_context(&diff, 8 * 1024);
                    task_run.final_diff_inspected = true;
                    push_bounded(
                        &mut task_run.verification_results,
                        TaskVerificationResult {
                            command: command.clone(),
                            category: "GitDiff".to_owned(),
                            passed: true,
                            exit_code: None,
                            summary: "Complete edit-tool diff was provided to the model."
                                .to_owned(),
                            affected_files: changed_files.clone(),
                            failure_origin: None,
                            relevant_output: relevant_output.clone(),
                        },
                        200,
                    );
                    self.emit(
                        &session_id,
                        EventPayload::VerificationResult {
                            command,
                            category: "GitDiff".to_owned(),
                            duration_ms: 0,
                            passed: true,
                            exit_code: None,
                            output: relevant_output.clone(),
                            diagnostics: Vec::new(),
                            affected_files: changed_files.clone(),
                            failure_origin: None,
                            relevant_output,
                        },
                        &collector,
                    )?;
                    unresolved_error_keys.remove("verification:final_diff_inspection");
                    let has_automated_checks = self.verifier.is_some()
                        && verification_plan.as_ref().is_some_and(|plan| {
                            plan.steps
                                .iter()
                                .any(|step| step.category != VerificationCategory::GitDiff)
                        });
                    if !has_automated_checks {
                        // The complete patch was shown to the model and there
                        // are no repository checks to run after this review.
                        validation_since_last_edit = true;
                        unresolved_error_keys.remove("verification:after_last_edit");
                    }
                }
                let verification_results_before = task_run.verification_results.len();
                let verification_repeated_failure = self.run_verification_if_needed(
                    &session_id,
                    &task.workspace_root,
                    verification_plan.as_ref(),
                    self.verifier.as_ref(),
                    &changed_files,
                    &mut context_input,
                    &collector,
                    cancellation,
                    &mut task_run,
                    &mut unresolved_error_keys,
                    &mut repeated_failures,
                    self.limits.max_repeated_failures,
                )?;
                if task_run.verification_results.len() > verification_results_before {
                    meaningful_plan_progress |= task_run.verification_results
                        [verification_results_before..]
                        .iter()
                        .any(|result| result.passed);
                    validation_since_last_edit = task_run.verification_results
                        [verification_results_before..]
                        .iter()
                        .all(|result| result.passed);
                    if validation_since_last_edit {
                        unresolved_error_keys.remove("verification:after_last_edit");
                        no_progress_final_responses = 0;
                    }
                } else if task_run.final_diff_inspected {
                    // No automated command was available to run, but the
                    // complete edit diff was reviewed and no check failed.
                    validation_since_last_edit = true;
                    unresolved_error_keys.remove("verification:after_last_edit");
                }
                if meaningful_plan_progress {
                    record_plan_progress(&mut task_run);
                }
                task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
                task_run.remaining_work = task_run.unresolved_errors.clone();
                self.update_task_run(&session_id, &collector, &task_run)?;
                if verification_repeated_failure {
                    let reason = format!(
                        "The same verification failure repeated {} times.",
                        self.limits.max_repeated_failures.max(1)
                    );
                    task_run.completion_status = TaskCompletionStatus::Blocked;
                    task_run.current_phase = TaskPhase::Finish;
                    return self.finish_blocked_task(
                        session_id,
                        collector,
                        task_run,
                        reason,
                        turns,
                        tool_calls,
                        model_tokens,
                        estimated_cost_microusd,
                    );
                }
            }
            model_history = next_model_history;
        }
    }

    fn update_task_run(
        &self,
        session_id: &SessionId,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
        task_run: &TaskRun,
    ) -> Result<(), AgentError> {
        self.emit(
            session_id,
            EventPayload::TaskRunUpdated {
                task_run: task_run.clone(),
            },
            collector,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_task_run(
        &self,
        session_id: SessionId,
        collector: Arc<Mutex<Vec<HarnessEvent>>>,
        mut task_run: TaskRun,
        final_message: String,
        turns: u32,
        tool_calls: u32,
        model_tokens: u64,
        estimated_cost_microusd: Option<u64>,
    ) -> Result<AgentOutcome, AgentError> {
        finish_plan_state(&mut task_run);
        self.update_task_run(&session_id, &collector, &task_run)?;
        let reason = match task_run.completion_status {
            TaskCompletionStatus::Done => None,
            TaskCompletionStatus::Blocked => Some("blocked".to_owned()),
            TaskCompletionStatus::UserInputRequired => Some("user_input_required".to_owned()),
            TaskCompletionStatus::ResourceLimitReached => Some("resource_limit_reached".to_owned()),
            TaskCompletionStatus::Cancelled => Some("cancelled".to_owned()),
            TaskCompletionStatus::InProgress => Some("incomplete".to_owned()),
        };
        self.emit(
            &session_id,
            EventPayload::SessionCompleted { reason },
            &collector,
        )?;
        Ok(AgentOutcome {
            session_id,
            final_message,
            turns,
            tool_calls,
            model_tokens,
            estimated_cost_microusd,
            completion_status: task_run.completion_status,
            remaining_work: task_run.remaining_work,
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_blocked_task(
        &self,
        session_id: SessionId,
        collector: Arc<Mutex<Vec<HarnessEvent>>>,
        mut task_run: TaskRun,
        reason: String,
        turns: u32,
        tool_calls: u32,
        model_tokens: u64,
        estimated_cost_microusd: Option<u64>,
    ) -> Result<AgentOutcome, AgentError> {
        task_run.current_phase = TaskPhase::Finish;
        task_run.completion_status = TaskCompletionStatus::Blocked;
        task_run.remaining_work = if task_run.unresolved_errors.is_empty() {
            vec![bounded_summary(&reason)]
        } else {
            task_run.unresolved_errors.clone()
        };
        let final_message = format!("Blocked: {}", bounded_summary(&reason));
        self.emit(
            &session_id,
            EventPayload::AssistantMessage {
                text: final_message.clone(),
            },
            &collector,
        )?;
        self.finish_task_run(
            session_id,
            collector,
            task_run,
            final_message,
            turns,
            tool_calls,
            model_tokens,
            estimated_cost_microusd,
        )
    }

    fn abort_with_task_run<T>(
        &self,
        session_id: SessionId,
        collector: Arc<Mutex<Vec<HarnessEvent>>>,
        task_run: &TaskRun,
        error: AgentError,
    ) -> Result<T, AgentError> {
        let mut task_run = task_run.clone();
        finish_plan_state(&mut task_run);
        let _ = self.update_task_run(&session_id, &collector, &task_run);
        self.fail(session_id, collector, error)
    }

    fn compact_context(
        &self,
        session_id: &SessionId,
        context_input: &mut ContextInput,
        collector: &Arc<Mutex<Vec<HarnessEvent>>>,
        task_run: &TaskRun,
    ) -> Result<(), AgentError> {
        let request = CompactionRequest {
            task: &context_input.user_request,
            goal: Some(&task_run.goal),
            execution_plan: task_run.execution_plan.as_ref(),
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
        cancellation: &CancellationToken,
        task_run: &mut TaskRun,
        unresolved_error_keys: &mut HashMap<String, String>,
        repeated_failures: &mut HashMap<String, u32>,
        repeated_failure_limit: u32,
    ) -> Result<bool, AgentError> {
        if changed_files.is_empty() {
            return Ok(false);
        }
        let (Some(plan), Some(verifier)) = (plan, verifier) else {
            return Ok(false);
        };
        let plan = VerificationPlanner::after_changes(plan, workspace_root, changed_files);
        if plan.steps.is_empty() {
            return Ok(false);
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
            changed_files: changed_files.to_vec(),
            max_output_bytes: 64 * 1024,
        };
        match verifier.verify_cancellable(&request, cancellation) {
            Ok(reports) => {
                for report in reports {
                    let failure_key = format!("verification:{}", report.command);
                    let failure = report.failure.as_ref();
                    let git_diff_output = (report.category == VerificationCategory::GitDiff
                        && report.passed)
                        .then(|| task_final_diff(workspace_root, changed_files))
                        .flatten();
                    let relevant_output = failure
                        .map(|failure| failure.relevant_output.as_str())
                        .filter(|output| !output.is_empty())
                        .or(git_diff_output.as_deref())
                        .unwrap_or(report.output.as_str());
                    let context_output = bounded_context(relevant_output, 8 * 1024);
                    let affected_files = failure
                        .map(|failure| failure.affected_files.clone())
                        .unwrap_or_default();
                    let failure_origin = failure.map(|failure| {
                        match failure.origin {
                            FailureOrigin::Introduced => "introduced",
                            FailureOrigin::Unrelated => "unrelated",
                            FailureOrigin::Unknown => "unknown",
                        }
                        .to_owned()
                    });
                    push_bounded(
                        &mut task_run.commands_executed,
                        bounded_summary(&report.command),
                        200,
                    );
                    if report.category == VerificationCategory::GitDiff && report.passed {
                        task_run.final_diff_inspected = true;
                    }
                    push_bounded(
                        &mut task_run.verification_results,
                        TaskVerificationResult {
                            command: report.command.clone(),
                            category: format!("{:?}", report.category),
                            passed: report.passed,
                            exit_code: report.exit_code,
                            summary: bounded_summary(&format!(
                                "{} {}",
                                report.diagnostics.join("; "),
                                context_output
                            )),
                            affected_files: affected_files.clone(),
                            failure_origin: failure_origin.clone(),
                            relevant_output: context_output.clone(),
                        },
                        200,
                    );
                    self.emit(
                        session_id,
                        EventPayload::VerificationResult {
                            command: report.command.clone(),
                            category: format!("{:?}", report.category),
                            duration_ms: report.duration_ms,
                            passed: report.passed,
                            exit_code: report.exit_code,
                            output: context_output.clone(),
                            diagnostics: report.diagnostics.clone(),
                            affected_files: affected_files.clone(),
                            failure_origin: failure_origin.clone(),
                            relevant_output: context_output.clone(),
                        },
                        collector,
                    )?;
                    let attribution = failure_origin
                        .as_deref()
                        .map(|origin| format!(" likely_origin={origin}"))
                        .unwrap_or_default();
                    let summary = bounded_context(
                        &format!(
                            "verification category={:?} command={} passed={} exit_code={:?} affected_files={:?}{} diagnostics={:?}\n{}",
                            report.category,
                            report.command,
                            report.passed,
                            report.exit_code,
                            affected_files,
                            attribution,
                            report.diagnostics,
                            context_output
                        ),
                        MAX_MODEL_TOOL_RESULT_BYTES,
                    );
                    context_input.tool_results.push(ToolContextResult {
                        name: "verification".to_owned(),
                        result: ToolResult::new(summary),
                        is_shell: false,
                    });
                    if report.passed {
                        unresolved_error_keys.remove(&failure_key);
                        if matches!(
                            report.category,
                            VerificationCategory::Typecheck | VerificationCategory::Build
                        ) {
                            unresolved_error_keys.retain(|key, _| {
                                !key.starts_with("verification:language-server diagnostics ")
                            });
                        }
                    } else {
                        let output = bounded_summary(&format!(
                            "{} {}",
                            report.diagnostics.join("; "),
                            context_output
                        ));
                        unresolved_error_keys
                            .insert(failure_key.clone(), format!("{failure_key} :: {output}"));
                        let signature = format!(
                            "{failure_key}:{}:{}",
                            report.exit_code.unwrap_or(-1),
                            normalize_failure(&context_output)
                        );
                        let failures = repeated_failures.entry(signature).or_default();
                        *failures = failures.saturating_add(1);
                        if *failures >= repeated_failure_limit.max(1) {
                            task_run.unresolved_errors =
                                unresolved_error_keys.values().cloned().collect();
                            return Ok(true);
                        }
                    }
                }
            }
            Err(error) => {
                let message = error.to_string();
                let failure_key = "verification:runner".to_owned();
                let failure = format!("{failure_key} :: {}", bounded_summary(&message));
                unresolved_error_keys.insert(failure_key.clone(), failure);
                push_bounded(
                    &mut task_run.verification_results,
                    TaskVerificationResult {
                        command: "verification".to_owned(),
                        category: "runner".to_owned(),
                        passed: false,
                        exit_code: None,
                        summary: bounded_summary(&message),
                        affected_files: Vec::new(),
                        failure_origin: Some("unknown".to_owned()),
                        relevant_output: bounded_context(&message, 8 * 1024),
                    },
                    200,
                );
                let signature = format!("{failure_key}:{}", normalize_failure(&message));
                let failures = repeated_failures.entry(signature).or_default();
                *failures = failures.saturating_add(1);
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
                        affected_files: Vec::new(),
                        failure_origin: Some("unknown".to_owned()),
                        relevant_output: bounded_context(&message, 8 * 1024),
                    },
                    collector,
                )?;
                context_input.tool_results.push(ToolContextResult {
                    name: "verification".to_owned(),
                    result: ToolResult::new(message),
                    is_shell: false,
                });
                if *failures >= repeated_failure_limit.max(1) {
                    task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
                    return Ok(true);
                }
            }
        }
        task_run.unresolved_errors = unresolved_error_keys.values().cloned().collect();
        Ok(false)
    }

    fn execute_tool(
        &self,
        execution: &ToolExecutionContext<'_>,
        request: ToolRequest,
    ) -> Result<ToolResult, AgentError> {
        let context = ToolContext {
            policy: execution.policy,
            working_directory: execution.workspace_root,
            cancellation: Some(execution.cancellation),
            event_bus: Some(&self.event_bus),
            session_id: Some(execution.session_id),
            correlation_id: None,
        };
        let result = self.tools.execute(&context, request.clone());
        self.flush(execution.session_id, execution.collector)?;
        match result {
            Ok(result) => Ok(result),
            Err(Error::PermissionRequired { .. }) => {
                let approved_by_user = self.approval_handler.request(&request)?;
                if !approved_by_user {
                    return Err(AgentError::ApprovalDenied { tool: request.name });
                }
                execution
                    .approved
                    .lock()
                    .expect("agent approval lock poisoned")
                    .insert(approval_key(&request));
                let result = self.tools.execute(&context, request);
                self.flush(execution.session_id, execution.collector)?;
                match result {
                    Ok(result) => Ok(result),
                    Err(Error::Tool { tool, message }) => Ok(tool_error_result(tool, message)),
                    Err(error) => Err(AgentError::Tool(error.to_string())),
                }
            }
            Err(Error::Tool { tool, message }) => Ok(tool_error_result(tool, message)),
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
        task_run: &mut TaskRun,
        estimated_cost_microusd: Option<u64>,
    ) -> Result<(), AgentError> {
        if cancellation.is_cancelled() {
            task_run.completion_status = TaskCompletionStatus::Cancelled;
            task_run.current_phase = TaskPhase::Finish;
            return self.abort_with_task_run(
                session_id.clone(),
                collector.clone(),
                task_run,
                AgentError::Cancelled,
            );
        }
        let reason = if turns > self.limits.max_turns {
            Some("max_turns")
        } else if tool_calls > self.limits.max_tool_calls {
            Some("max_tool_calls")
        } else if self.limits.max_model_tokens > 0 && model_tokens > self.limits.max_model_tokens {
            Some("max_model_tokens")
        } else if self.limits.max_runtime != Duration::ZERO
            && started_at.elapsed() > self.limits.max_runtime
        {
            Some("max_runtime")
        } else if self
            .limits
            .max_estimated_cost_microusd
            .is_some_and(|limit| estimated_cost_microusd.is_some_and(|cost| cost > limit))
        {
            Some("max_estimated_cost")
        } else {
            None
        };
        if let Some(limit) = reason {
            task_run.completion_status = TaskCompletionStatus::ResourceLimitReached;
            task_run.current_phase = TaskPhase::Finish;
            task_run.remaining_work = vec![format!("Task stopped at configured limit: {limit}")];
            return self.abort_with_task_run(
                session_id.clone(),
                collector.clone(),
                task_run,
                AgentError::LimitExceeded {
                    limit: limit.to_owned(),
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

const MAX_MODEL_TOOL_RESULT_BYTES: usize = 16 * 1024;
const TOOL_RESULT_TRUNCATION_MARKER: &str = "\n[truncated]";

fn tool_error_result(tool: String, message: String) -> ToolResult {
    let mut result = ToolResult::new(format!("Tool {tool} failed: {message}"));
    result.is_error = true;
    result
}

fn bound_tool_result_output(output: &str, already_truncated: bool) -> (String, bool) {
    let truncated = already_truncated || output.len() > MAX_MODEL_TOOL_RESULT_BYTES;
    if !truncated {
        return (output.to_owned(), false);
    }

    let content_limit =
        MAX_MODEL_TOOL_RESULT_BYTES.saturating_sub(TOOL_RESULT_TRUNCATION_MARKER.len());
    let mut end = output.len().min(content_limit);
    while !output.is_char_boundary(end) {
        end -= 1;
    }
    let mut bounded = output[..end].to_owned();
    if !bounded.ends_with(TOOL_RESULT_TRUNCATION_MARKER) {
        bounded.push_str(TOOL_RESULT_TRUNCATION_MARKER);
    }
    (bounded, true)
}

fn quick_diagnostic_result(
    result: &ToolResult,
    changed_files: &[PathBuf],
) -> Option<(String, bool, Vec<PathBuf>, String)> {
    let diagnostics = result.metadata.get("diagnostics")?;
    if diagnostics.get("status")?.as_str()? != "best_effort" {
        return None;
    }
    let detail = diagnostics.get("detail")?.as_str()?.trim();
    let normalized_detail = detail.to_ascii_lowercase();
    if detail.is_empty()
        || normalized_detail.contains("unavailable")
        || normalized_detail.contains("failed:")
        || normalized_detail.contains("timed out")
        || normalized_detail.contains("returned no diagnostic details")
    {
        return None;
    }
    let path = changed_files.iter().find(|path| {
        matches!(
            path.extension().and_then(|extension| extension.to_str()),
            Some("rs" | "ts" | "tsx" | "js" | "jsx" | "py")
        )
    })?;
    let path_display = path.to_string_lossy().replace('\\', "/");
    let command = format!("language-server diagnostics {path_display}");
    let failed = detail
        .lines()
        .any(|line| line.trim_start().to_ascii_lowercase().starts_with("error "));
    Some((command, !failed, vec![path.clone()], detail.to_owned()))
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

#[cfg(test)]
mod conformance_hardening_tests {
    use super::{
        advance_execution_plan, apply_unrelated_verification_dispositions,
        bound_tool_result_output, complete_edit_diff, decimal_microusd, estimate_cost_microusd,
        execution_plan_from_implementation, extract_plan_revision, finish_plan_state,
        parse_unrelated_verification_directives, quick_diagnostic_result, record_plan_progress,
        revise_execution_plan, task_final_diff, tool_error_result,
    };
    use harness_models::{ModelPricing, Usage};
    use harness_session::{
        ExecutionPlan, ImplementationPlan, PlanItemStatus, TaskCompletionStatus, TaskRun,
        TaskVerificationResult,
    };
    use std::collections::HashMap;
    use std::process::Command;
    use tempfile::tempdir;

    #[test]
    fn plans_advance_on_progress_and_only_revise_for_a_reason() {
        let mut task_run = TaskRun::new("implement a durable goal");
        let initial = ImplementationPlan {
            goal: task_run.original_goal.clone(),
            implementation_steps: vec![
                "Inspect existing session state".to_owned(),
                "Persist execution plan".to_owned(),
            ],
            relevant_architecture: vec!["harness-session owns durable events".to_owned()],
            validation: vec!["cargo test -p harness-session".to_owned()],
            ..ImplementationPlan::default()
        };
        task_run.execution_plan = Some(execution_plan_from_implementation(&initial, &[]));
        advance_execution_plan(&mut task_run);
        record_plan_progress(&mut task_run);
        assert_eq!(
            task_run.execution_plan.as_ref().unwrap().milestones[0].tasks[0].status,
            PlanItemStatus::Completed
        );

        let revised_steps = vec![
            "Inspect existing session state".to_owned(),
            "Persist execution plan".to_owned(),
            "Verify after compaction".to_owned(),
        ];
        assert!(!revise_execution_plan(&mut task_run, "", &revised_steps));
        assert!(!revise_execution_plan(
            &mut task_run,
            "the saved plan still applies",
            &revised_steps[..2]
        ));
        assert!(revise_execution_plan(
            &mut task_run,
            "compaction testing revealed a missing resume step",
            &revised_steps
        ));
        let plan = task_run.execution_plan.as_ref().unwrap();
        assert_eq!(plan.revision, 2);
        assert_eq!(
            plan.decision_notes,
            ["compaction testing revealed a missing resume step"]
        );
        assert_eq!(
            plan.milestones[0].tasks[0].status,
            PlanItemStatus::Completed
        );
        assert_eq!(
            plan.milestones[0].tasks[1].status,
            PlanItemStatus::InProgress
        );
        assert_eq!(
            plan.milestones[0].affected_architecture,
            ["harness-session owns durable events"]
        );
        assert_eq!(
            plan.milestones[0].validation_commands,
            ["cargo test -p harness-session"]
        );

        let ordinary_plan_text = "Plan:\n- replace the existing approach";
        assert!(extract_plan_revision(ordinary_plan_text).is_none());
        let revision_text = "Plan revision: a fixture exposed a missing validation step\nPlan:\n- inspect\n- validate";
        assert_eq!(
            extract_plan_revision(revision_text).unwrap().0,
            "a fixture exposed a missing validation step"
        );
        assert_eq!(task_run.execution_plan.as_ref().unwrap().revision, 2);
    }

    #[test]
    fn failed_milestone_is_persisted_as_blocked() {
        let mut task_run = TaskRun::new("finish a task");
        task_run.execution_plan = Some(ExecutionPlan {
            revision: 1,
            status: PlanItemStatus::InProgress,
            milestones: vec![harness_session::ExecutionMilestone {
                title: "Validate".to_owned(),
                tasks: vec![harness_session::ExecutionTask {
                    description: "Run the required check".to_owned(),
                    status: PlanItemStatus::InProgress,
                }],
                status: PlanItemStatus::InProgress,
                ..harness_session::ExecutionMilestone::default()
            }],
            ..ExecutionPlan::default()
        });
        task_run.completion_status = TaskCompletionStatus::Blocked;

        finish_plan_state(&mut task_run);

        let plan = task_run.execution_plan.unwrap();
        assert_eq!(plan.status, PlanItemStatus::Blocked);
        assert_eq!(plan.milestones[0].status, PlanItemStatus::Blocked);
        assert_eq!(plan.milestones[0].tasks[0].status, PlanItemStatus::Blocked);
        assert_eq!(task_run.goal.current_milestone.as_deref(), Some("Validate"));
    }

    #[test]
    fn tool_errors_are_marked_and_large_utf8_results_are_bounded() {
        let error = tool_error_result("read_file".to_owned(), "missing file".to_owned());
        assert!(error.is_error);
        assert!(error.output.contains("missing file"));

        let long = "🙂".repeat(20_000);
        let (bounded, truncated) = bound_tool_result_output(&long, false);
        assert!(truncated);
        assert!(bounded.len() <= 16 * 1024);
        assert!(bounded.ends_with("[truncated]"));
    }

    #[test]
    fn quick_language_server_diagnostics_are_structured_and_ignore_unavailable_data() {
        let path = std::path::PathBuf::from("src/lib.rs");
        let mut result = harness_tools::ToolResult::new("edited");
        result.metadata.insert(
            "diagnostics".to_owned(),
            serde_json::json!({
                "status": "best_effort",
                "detail": "error 4:2: expected `;`"
            }),
        );
        let failure = quick_diagnostic_result(&result, std::slice::from_ref(&path))
            .expect("reported LSP errors should be recorded");
        assert!(!failure.1);
        assert_eq!(failure.2, vec![path.clone()]);
        assert!(failure.0.contains("src/lib.rs"));

        result.metadata.insert(
            "diagnostics".to_owned(),
            serde_json::json!({
                "status": "best_effort",
                "detail": "No diagnostics reported by the language server."
            }),
        );
        let passed = quick_diagnostic_result(&result, std::slice::from_ref(&path))
            .expect("empty diagnostic list is a successful quick check");
        assert!(passed.1);

        result.metadata.insert(
            "diagnostics".to_owned(),
            serde_json::json!({
                "status": "best_effort",
                "detail": "Quick diagnostics unavailable: language server is not running"
            }),
        );
        assert!(quick_diagnostic_result(&result, &[path]).is_none());

        result.metadata.insert(
            "diagnostics".to_owned(),
            serde_json::json!({
                "status": "best_effort",
                "detail": "rust-analyzer diagnostics failed: language server timed out"
            }),
        );
        assert!(
            quick_diagnostic_result(&result, &[std::path::PathBuf::from("src/lib.rs")]).is_none()
        );
    }

    #[test]
    fn complete_edit_diff_is_accepted_when_the_tool_result_is_not_truncated() {
        let path = std::path::PathBuf::from("src/lib.rs");
        let mut result = harness_tools::ToolResult::new("patched");
        result.changed_files.push(path.clone());
        result.metadata.insert(
            "diff".to_owned(),
            serde_json::json!("--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new"),
        );
        let diff = complete_edit_diff(&result, std::slice::from_ref(&path))
            .expect("edit diff is available");
        assert!(diff.contains("+new"));

        result
            .metadata
            .insert("diff_truncated".to_owned(), serde_json::json!(true));
        assert!(complete_edit_diff(&result, &[path]).is_none());
    }

    #[test]
    fn estimated_cost_requires_complete_usage_and_trusted_pricing() {
        let pricing = ModelPricing {
            input_usd_per_million_tokens: Some("2.00".to_owned()),
            output_usd_per_million_tokens: Some("4".to_owned()),
        };
        assert_eq!(
            estimate_cost_microusd(Some(&Usage::new(1_000_000, 500_000)), Some(&pricing)),
            Some(4_000_000)
        );
        assert_eq!(decimal_microusd("0.25"), Some(250_000));
        assert_eq!(estimate_cost_microusd(None, Some(&pricing)), None);
        assert_eq!(estimate_cost_microusd(Some(&Usage::new(1, 1)), None), None);
    }

    #[test]
    fn unrelated_verification_requires_an_exact_failed_command_and_keeps_audit_trail() {
        let text = "The failure is from an unchanged legacy fixture.\n[UNRELATED_VERIFICATION] cargo test -p legacy :: its failing assertion is in tests/legacy.rs, outside the edited package";
        let (message, dispositions) = parse_unrelated_verification_directives(text);
        let mut task_run = TaskRun::new("fix the changed package");
        task_run.verification_results.push(TaskVerificationResult {
            command: "cargo test -p legacy".to_owned(),
            category: "GeneralTest".to_owned(),
            passed: false,
            exit_code: Some(1),
            summary: "assertion failed".to_owned(),
            affected_files: Vec::new(),
            failure_origin: Some("unrelated".to_owned()),
            relevant_output: "tests/legacy.rs assertion failed".to_owned(),
        });
        let mut unresolved = HashMap::from([(
            "verification:cargo test -p legacy".to_owned(),
            "legacy test failed".to_owned(),
        )]);

        let mut validated = false;
        apply_unrelated_verification_dispositions(
            &mut task_run,
            &mut unresolved,
            &dispositions,
            &mut validated,
        );

        assert!(message.contains("legacy fixture"));
        assert!(unresolved.is_empty());
        assert!(task_run.unresolved_errors.is_empty());
        assert!(validated);
        assert!(task_run.verification_results[0]
            .summary
            .contains("classified as unrelated"));
        assert!(task_run.verification_results[0]
            .summary
            .contains("outside the edited package"));
    }

    #[test]
    fn final_task_diff_includes_new_untracked_files() {
        let temporary = tempdir().unwrap();
        let root = temporary.path();
        let run_git = |arguments: &[&str]| {
            let output = Command::new("git")
                .current_dir(root)
                .args(arguments)
                .output()
                .unwrap();
            assert!(output.status.success(), "git command failed: {arguments:?}");
        };
        run_git(&["init", "--quiet"]);
        run_git(&["config", "user.email", "verify@example.invalid"]);
        run_git(&["config", "user.name", "Verification"]);
        std::fs::write(root.join("tracked.txt"), "before\n").unwrap();
        run_git(&["add", "tracked.txt"]);
        run_git(&["commit", "--quiet", "-m", "baseline"]);
        std::fs::write(root.join("new.rs"), "pub fn added() {}\n").unwrap();
        let git = super::GitClient::open(root).unwrap();
        let direct_change = git.file_change("new.rs").unwrap();
        assert!(!direct_change.patch.is_empty());

        let diff = task_final_diff(root, &[std::path::PathBuf::from("new.rs")])
            .expect("Git diff should be available");

        assert!(diff.contains("new.rs"));
        assert!(diff.contains("+pub fn added()"));
    }
}
