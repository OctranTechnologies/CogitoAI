use std::collections::{BTreeMap, VecDeque};
use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};

use super::INTERACTIVE_COMMANDS;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    self, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use crossterm::{cursor::Show, style::ResetColor};
use harness_models::{ModelConfig, ProviderKind};
use harness_session::{EventPayload, TaskMode};
use harness_tools::CancellationToken;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

const ACTIVITY_LIMIT: usize = 200;
const HISTORY_LIMIT: usize = 100;
type BackendTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Debug)]
pub struct StartupInfo {
    pub model: String,
    pub provider: String,
    pub version: String,
    pub workspace: String,
    pub notice: Option<String>,
    pub execution_mode: String,
    pub branch: Option<String>,
    pub workspace_dirty: Option<bool>,
}

enum UiMessage {
    Activity(String),
    RuntimeEvent(Box<EventPayload>),
    WorkspaceContext {
        execution_mode: String,
        branch: Option<String>,
        workspace_dirty: Option<bool>,
    },
    RuntimeStatus(Option<String>),
    Approval {
        tool: String,
        response: SyncSender<bool>,
    },
    RunFinished(Result<(), String>),
    SessionId(String),
}

#[derive(Clone)]
pub struct TuiSender {
    sender: Sender<UiMessage>,
}

impl TuiSender {
    pub fn activity(&self, message: impl Into<String>) {
        let _ = self.sender.send(UiMessage::Activity(message.into()));
    }

    pub fn runtime_event(&self, event: &EventPayload) {
        let _ = self
            .sender
            .send(UiMessage::RuntimeEvent(Box::new(event.clone())));
    }

    pub fn workspace_context(
        &self,
        execution_mode: &str,
        branch: Option<String>,
        workspace_dirty: Option<bool>,
    ) {
        let _ = self.sender.send(UiMessage::WorkspaceContext {
            execution_mode: execution_mode.to_owned(),
            branch,
            workspace_dirty,
        });
    }

    pub fn runtime_status(&self, status: Option<&str>) {
        let _ = self
            .sender
            .send(UiMessage::RuntimeStatus(status.map(str::to_owned)));
    }

    pub fn request_approval(&self, tool: &str) -> bool {
        let (sender, receiver) = mpsc::sync_channel(1);
        if self
            .sender
            .send(UiMessage::Approval {
                tool: tool.to_owned(),
                response: sender,
            })
            .is_err()
        {
            return false;
        }
        receiver.recv().unwrap_or(false)
    }

    pub fn run_finished(&self, result: Result<(), String>) {
        let _ = self.sender.send(UiMessage::RunFinished(result));
    }

    pub fn active_session(&self, session_id: &str) {
        let _ = self
            .sender
            .send(UiMessage::SessionId(session_id.to_owned()));
    }
}

#[derive(Debug, Eq, PartialEq)]
enum InputAction {
    None,
    Submit(String),
    Quit,
}

struct AppState {
    startup: StartupInfo,
    task_mode: TaskMode,
    execution_mode: String,
    branch: Option<String>,
    workspace_dirty: Option<bool>,
    token_usage: Option<(u64, u64)>,
    token_usage_complete: bool,
    run_started: Option<Instant>,
    last_run_duration: Option<Duration>,
    current_action: Option<String>,
    active_process: Option<String>,
    background_processes: BTreeMap<String, String>,
    activity: VecDeque<String>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_scratch: String,
    scroll_from_bottom: usize,
    status: String,
    runtime_status: Option<String>,
    running: bool,
    cancellation: Option<CancellationToken>,
    pending_approval: Option<(String, SyncSender<bool>)>,
    quit: bool,
    colors: bool,
    active_session_id: Option<String>,
    approved_plan_available: bool,
}

impl AppState {
    fn new(startup: StartupInfo, colors: bool) -> Self {
        let mut state = Self {
            task_mode: TaskMode::Code,
            execution_mode: startup.execution_mode.clone(),
            branch: startup.branch.clone(),
            workspace_dirty: startup.workspace_dirty,
            token_usage: None,
            token_usage_complete: true,
            run_started: None,
            last_run_duration: None,
            current_action: None,
            active_process: None,
            background_processes: BTreeMap::new(),
            startup,
            activity: VecDeque::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            history_scratch: String::new(),
            scroll_from_bottom: 0,
            status: "Ready  |  Enter a task or type /help".to_owned(),
            runtime_status: None,
            running: false,
            cancellation: None,
            pending_approval: None,
            quit: false,
            colors,
            active_session_id: None,
            approved_plan_available: false,
        };
        state.push_activity("Ready. Enter a task or type /help.".to_owned());
        state
    }

    fn push_activity(&mut self, message: String) {
        let message = sanitize(&message);
        if self.activity.len() == ACTIVITY_LIMIT {
            self.activity.pop_front();
            self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(1);
        }
        self.activity.push_back(message);
    }

    fn approved_plan_session_for_code(&self) -> Option<&str> {
        if self.task_mode == TaskMode::Code && self.approved_plan_available {
            self.active_session_id.as_deref()
        } else {
            None
        }
    }

    fn apply_runtime_event(&mut self, event: EventPayload) {
        match event {
            EventPayload::ModelRequested { .. } => {
                self.current_action = Some("Thinking".to_owned());
            }
            EventPayload::ModelResponse {
                provider,
                input_tokens,
                output_tokens,
                ..
            } => {
                if provider.to_ascii_lowercase().contains("mock") {
                    self.token_usage = None;
                    self.token_usage_complete = false;
                } else {
                    self.record_token_usage(input_tokens, output_tokens);
                }
                self.current_action = Some("Reviewing response".to_owned());
            }
            EventPayload::ToolRequested { tool, arguments } => {
                let action = match tool.as_str() {
                    "read_file" => "Reading file",
                    "list_directory" => "Reading directory",
                    "glob" => "Searching files",
                    "grep" => "Searching symbol",
                    "write_file" | "apply_patch" => "Editing file",
                    "shell" | "run_command" => "Running command",
                    "start_background_command" => "Starting background process",
                    "read_process_output" => "Reading process logs",
                    "list_processes" => "Inspecting processes",
                    "stop_process" => "Stopping process",
                    "wait_for_process_output" => "Waiting for process readiness",
                    "delegate_subagents" => "Delegating to read-only agents",
                    _ => "Using tool",
                };
                let target = arguments
                    .get("path")
                    .or_else(|| arguments.get("pattern"))
                    .or_else(|| arguments.get("query"))
                    .or_else(|| arguments.get("command"));
                self.current_action = Some(target.map_or_else(
                    || {
                        if tool == "delegate_subagents" {
                            action.to_owned()
                        } else {
                            format!("{action} · {tool}")
                        }
                    },
                    |target| format!("{action} · {}", compact(target, 72)),
                ));
            }
            EventPayload::ToolStarted { tool } if self.current_action.is_none() => {
                self.current_action = Some(format!("Running {tool}"));
            }
            EventPayload::ToolStarted { .. } => {}
            EventPayload::ProcessStarted { command, .. } => {
                self.active_process = Some(compact(&command, 72));
                self.current_action = Some("Running command".to_owned());
            }
            EventPayload::ProcessExited { .. } => {
                self.active_process = None;
                self.current_action = Some("Checking command result".to_owned());
            }
            EventPayload::BackgroundProcessStarted {
                process_id,
                command,
                ..
            } => {
                self.background_processes
                    .insert(process_id, compact(&command, 36));
                self.current_action = Some("Background process running".to_owned());
            }
            EventPayload::BackgroundProcessStatus {
                process_id, status, ..
            } => {
                if status != "running" && status != "stopping" {
                    self.background_processes.remove(&process_id);
                }
                self.current_action = Some(format!("Background process {status}"));
            }
            EventPayload::ToolCompleted { .. } => {
                self.current_action = Some("Thinking".to_owned());
            }
            EventPayload::ToolFailed { .. } => {
                self.current_action = Some("Recovering from tool error".to_owned());
            }
            EventPayload::VerificationStarted { .. } => {
                self.current_action = Some("Running checks".to_owned());
            }
            EventPayload::VerificationResult {
                category, passed, ..
            } => {
                let check = if category.to_ascii_lowercase().contains("test") {
                    "Tests"
                } else {
                    "Checks"
                };
                self.current_action = Some(format!(
                    "{check} {}",
                    if passed { "passed" } else { "failed" }
                ));
            }
            EventPayload::TaskRunUpdated { task_run } => {
                self.approved_plan_available = task_run.task_mode == TaskMode::Plan
                    && task_run.structured_plan.is_some()
                    && task_run.completion_status == harness_session::TaskCompletionStatus::Done;
            }
            EventPayload::FileChanged { .. } => {
                self.workspace_dirty = Some(true);
                self.current_action = Some("Updating workspace".to_owned());
            }
            EventPayload::SessionResumed { .. } => {
                self.current_action = Some("Resuming session".to_owned());
            }
            EventPayload::SessionCompleted { .. } => {
                self.active_process = None;
                self.current_action = Some("Task completed".to_owned());
            }
            EventPayload::SessionFailed { .. } => {
                self.active_process = None;
                self.current_action = Some("Task failed".to_owned());
            }
            _ => {}
        }
    }

    fn record_token_usage(&mut self, input: Option<u32>, output: Option<u32>) {
        if !self.token_usage_complete {
            return;
        }
        match (input, output) {
            (Some(input), Some(output)) => {
                let total = self.token_usage.get_or_insert((0, 0));
                total.0 += u64::from(input);
                total.1 += u64::from(output);
            }
            _ => {
                self.token_usage = None;
                self.token_usage_complete = false;
            }
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> InputAction {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        if control && matches!(key.code, KeyCode::Char('c' | 'C')) {
            if self.running {
                if let Some((tool, response)) = self.pending_approval.take() {
                    let _ = response.send(false);
                    self.push_activity(format!("Approval denied · {tool}"));
                }
                if let Some(cancellation) = &self.cancellation {
                    cancellation.cancel();
                }
                self.current_action = Some("Cancelling".to_owned());
                self.status =
                    "Cancellation requested  |  Waiting for the current operation to stop"
                        .to_owned();
            } else if !self.input.is_empty() {
                self.input.clear();
                self.cursor = 0;
                self.reset_history_navigation();
                self.status = "Input cleared".to_owned();
            } else {
                return InputAction::Quit;
            }
            return InputAction::None;
        }

        if control && matches!(key.code, KeyCode::Char('d' | 'D')) {
            if !self.running && self.input.is_empty() {
                return InputAction::Quit;
            }
            if !self.running {
                self.delete_next_char();
            }
            return InputAction::None;
        }

        if self.running {
            if let Some((tool, response)) = self.pending_approval.take() {
                match key.code {
                    KeyCode::Char('y' | 'Y') => {
                        let _ = response.send(true);
                        self.status = format!("Approved · {tool}");
                        self.current_action = Some("Continuing".to_owned());
                        self.push_activity(format!("Approval granted · {tool}"));
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        let _ = response.send(false);
                        self.status = format!("Denied · {tool}");
                        self.current_action = Some("Continuing".to_owned());
                        self.push_activity(format!("Approval denied · {tool}"));
                    }
                    _ => self.pending_approval = Some((tool, response)),
                }
            }
            self.handle_activity_navigation(key.code);
            return InputAction::None;
        }

        match key.code {
            KeyCode::Enter if alt || shift => {
                self.insert_text("\n");
            }
            KeyCode::Enter => {
                let submitted = self.input.trim().to_owned();
                if submitted.is_empty() {
                    return InputAction::None;
                }
                if self.history.last() != Some(&submitted) {
                    if self.history.len() == HISTORY_LIMIT {
                        self.history.remove(0);
                    }
                    self.history.push(submitted.clone());
                }
                self.input.clear();
                self.cursor = 0;
                self.reset_history_navigation();
                return InputAction::Submit(submitted);
            }
            KeyCode::Char('j') if control => self.insert_text("\n"),
            KeyCode::Char(character) if control && matches!(character, 'a' | 'A') => {
                self.cursor = 0;
            }
            KeyCode::Char(character) if control && matches!(character, 'e' | 'E') => {
                self.cursor = self.input.len();
            }
            KeyCode::Char(character) if control && matches!(character, 'w' | 'W') => {
                self.delete_previous_word();
            }
            KeyCode::Char(character) if !control && !alt => {
                self.insert_char(character);
            }
            KeyCode::Left => self.move_left(),
            KeyCode::Right => self.move_right(),
            KeyCode::Up => self.move_up_or_history(),
            KeyCode::Down => self.move_down_or_history(),
            KeyCode::Home => self.move_line_start(),
            KeyCode::End => self.move_line_end(),
            KeyCode::Backspace => self.delete_previous_char(),
            KeyCode::Delete => self.delete_next_char(),
            KeyCode::Tab => self.complete_command(),
            KeyCode::PageUp => self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(8),
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(8)
            }
            KeyCode::Esc => {
                self.input.clear();
                self.cursor = 0;
                self.reset_history_navigation();
            }
            _ => {}
        }
        InputAction::None
    }

    fn handle_activity_navigation(&mut self, code: KeyCode) {
        match code {
            KeyCode::PageUp => self.scroll_from_bottom = self.scroll_from_bottom.saturating_add(8),
            KeyCode::PageDown => {
                self.scroll_from_bottom = self.scroll_from_bottom.saturating_sub(8)
            }
            _ => {}
        }
    }

    fn insert_char(&mut self, character: char) {
        self.input.insert(self.cursor, character);
        self.cursor += character.len_utf8();
        self.reset_history_navigation();
    }

    fn insert_text(&mut self, text: &str) {
        self.input.insert_str(self.cursor, text);
        self.cursor += text.len();
        self.reset_history_navigation();
    }

    fn move_left(&mut self) {
        if let Some((index, _)) = self.input[..self.cursor].char_indices().last() {
            self.cursor = index;
        }
    }

    fn move_right(&mut self) {
        if let Some(character) = self.input[self.cursor..].chars().next() {
            self.cursor += character.len_utf8();
        }
    }

    fn move_line_start(&mut self) {
        self.cursor = self.input[..self.cursor]
            .rfind('\n')
            .map_or(0, |newline| newline + 1);
    }

    fn move_line_end(&mut self) {
        self.cursor = self.input[self.cursor..]
            .find('\n')
            .map_or(self.input.len(), |offset| self.cursor + offset);
    }

    fn move_up_or_history(&mut self) {
        if self.input.contains('\n') {
            self.move_vertical(-1);
        } else if !self.history.is_empty() {
            let next = self
                .history_index
                .map_or(self.history.len() - 1, |index| index.saturating_sub(1));
            self.set_history_index(Some(next));
        }
    }

    fn move_down_or_history(&mut self) {
        if self.input.contains('\n') {
            self.move_vertical(1);
        } else if let Some(index) = self.history_index {
            if index + 1 < self.history.len() {
                self.set_history_index(Some(index + 1));
            } else {
                self.set_history_index(None);
            }
        }
    }

    fn move_vertical(&mut self, direction: isize) {
        let before = &self.input[..self.cursor];
        let current_line_start = before.rfind('\n').map_or(0, |index| index + 1);
        let column = before[current_line_start..].chars().count();
        let line_start = if direction < 0 {
            if current_line_start == 0 {
                return;
            }
            self.input[..current_line_start - 1]
                .rfind('\n')
                .map_or(0, |index| index + 1)
        } else {
            let Some(next_start) = self.input[self.cursor..].find('\n') else {
                return;
            };
            self.cursor + next_start + 1
        };
        let line_end = self.input[line_start..]
            .find('\n')
            .map_or(self.input.len(), |offset| line_start + offset);
        self.cursor = self.input[line_start..line_end]
            .char_indices()
            .nth(column)
            .map_or(line_end, |(offset, _)| line_start + offset);
    }

    fn delete_previous_char(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let previous = self.input[..self.cursor]
            .char_indices()
            .last()
            .map_or(self.cursor, |(index, _)| index);
        self.input.replace_range(previous..self.cursor, "");
        self.cursor = previous;
        self.reset_history_navigation();
    }

    fn delete_next_char(&mut self) {
        if let Some(character) = self.input[self.cursor..].chars().next() {
            let end = self.cursor + character.len_utf8();
            self.input.replace_range(self.cursor..end, "");
            self.reset_history_navigation();
        }
    }

    fn delete_previous_word(&mut self) {
        let prefix = &self.input[..self.cursor];
        let trimmed_end = prefix.trim_end_matches(char::is_whitespace).len();
        let start = prefix[..trimmed_end]
            .rfind(char::is_whitespace)
            .map_or(0, |index| index + 1);
        self.input.replace_range(start..self.cursor, "");
        self.cursor = start;
        self.reset_history_navigation();
    }

    fn set_history_index(&mut self, index: Option<usize>) {
        if self.history_index.is_none() {
            self.history_scratch = self.input.clone();
        }
        self.history_index = index;
        self.input = index.map_or_else(
            || self.history_scratch.clone(),
            |value| self.history[value].clone(),
        );
        self.cursor = self.input.len();
    }

    fn reset_history_navigation(&mut self) {
        self.history_index = None;
        self.history_scratch.clear();
    }

    fn complete_command(&mut self) {
        if !self.input.starts_with('/') || self.input.contains(char::is_whitespace) {
            return;
        }
        let prefix = self.input.as_str();
        let matches = INTERACTIVE_COMMANDS
            .iter()
            .map(|definition| definition.name)
            .filter(|command| command.starts_with(prefix))
            .collect::<Vec<_>>();
        if matches.len() == 1 {
            self.input = format!("{} ", matches[0]);
            self.cursor = self.input.len();
            self.reset_history_navigation();
        } else if matches.len() > 1 {
            self.status = format!("Commands: {}", matches.join("  "));
        }
    }
}

pub struct Tui {
    terminal: BackendTerminal,
    receiver: Receiver<UiMessage>,
    sender: TuiSender,
    state: AppState,
    suspended: bool,
    dirty: bool,
    model_config: ModelConfig,
}

impl Tui {
    pub fn new(startup: StartupInfo, model_config: ModelConfig) -> io::Result<Self> {
        let (sender, receiver) = mpsc::channel();
        let colors = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map_or(true, |term| term != "dumb");
        enable_raw_mode()?;
        let mut terminal = match Terminal::new(CrosstermBackend::new(io::stdout())) {
            Ok(terminal) => terminal,
            Err(error) => {
                let _ = terminal::disable_raw_mode();
                return Err(error);
            }
        };
        if let Err(error) = execute!(
            terminal.backend_mut(),
            EnterAlternateScreen,
            crossterm::cursor::Hide,
            SetTitle("CogitoAI")
        ) {
            restore_terminal(&mut terminal);
            return Err(error);
        }
        if let Err(error) = terminal.clear() {
            restore_terminal(&mut terminal);
            return Err(error);
        }
        Ok(Self {
            terminal,
            receiver,
            sender: TuiSender { sender },
            state: AppState::new(startup, colors),
            suspended: false,
            dirty: true,
            model_config,
        })
    }

    pub fn sender(&self) -> TuiSender {
        self.sender.clone()
    }

    pub fn model_config(&self) -> ModelConfig {
        self.model_config.clone()
    }

    pub fn task_mode(&self) -> TaskMode {
        self.state.task_mode
    }

    pub fn set_task_mode(&mut self, mode: TaskMode) {
        self.state.task_mode = mode;
        self.state.status = format!("Task mode · {}", task_mode_label(mode));
        self.dirty = true;
    }

    pub fn approved_plan_session_for_code(&self) -> Option<String> {
        self.state
            .approved_plan_session_for_code()
            .map(str::to_owned)
    }

    pub fn set_model_config(&mut self, model: ModelConfig) {
        self.state.startup.model = model.model.clone();
        self.state.startup.provider = match model.provider {
            ProviderKind::Mock => "mock",
            ProviderKind::OpenAi => "openai",
            ProviderKind::Anthropic => "anthropic",
            ProviderKind::Gemini => "gemini",
            ProviderKind::OpenCodeZen => "opencode-zen",
            ProviderKind::OpenCodeGo => "opencode-go",
        }
        .to_owned();
        self.model_config = model;
        self.dirty = true;
    }

    pub fn active_session_id(&self) -> Option<String> {
        self.state.active_session_id.clone()
    }

    pub fn set_active_session_id(&mut self, session_id: Option<String>) {
        if session_id != self.state.active_session_id {
            self.state.approved_plan_available = false;
        }
        self.state.active_session_id = session_id;
    }

    pub fn show_runtime_status(&mut self, status: &str) -> io::Result<()> {
        self.state.runtime_status = Some(status.to_owned());
        self.dirty = true;
        self.draw()
    }

    pub fn set_runtime_status(&mut self, status: Option<&str>) {
        self.state.runtime_status = status.map(str::to_owned);
        self.dirty = true;
    }

    pub fn start_run(&mut self, label: &str, cancellation: CancellationToken) -> bool {
        if self.state.running {
            return false;
        }
        self.state.running = true;
        self.state.cancellation = Some(cancellation);
        self.state.run_started = Some(Instant::now());
        self.state.last_run_duration = None;
        self.state.token_usage = None;
        self.state.token_usage_complete = true;
        self.state.current_action = Some("Starting agent".to_owned());
        self.state.active_process = None;
        self.state.status = "Running  |  Ctrl+C cancels the current task".to_owned();
        self.state
            .push_activity(format!("Task started · {}", compact(label, 120)));
        self.dirty = true;
        true
    }

    pub fn request_exit(&mut self) {
        if self.state.running {
            self.state.status = "A task is running; press Ctrl+C to cancel it first".to_owned();
        } else {
            self.state.quit = true;
        }
        self.dirty = true;
    }

    pub fn cancel_run(&mut self) {
        if let Some(cancellation) = &self.state.cancellation {
            cancellation.cancel();
            self.state.current_action = Some("Cancelling".to_owned());
            self.state.status = "Cancellation requested".to_owned();
            self.dirty = true;
        }
    }

    pub fn add_activity(&mut self, message: impl Into<String>) {
        self.state.push_activity(message.into());
        self.dirty = true;
    }

    pub fn clear_activity(&mut self) {
        self.state.activity.clear();
        self.state.scroll_from_bottom = 0;
        self.dirty = true;
    }

    pub fn suspend(&mut self) -> io::Result<()> {
        if self.suspended {
            return Ok(());
        }
        terminal::disable_raw_mode()?;
        execute!(
            self.terminal.backend_mut(),
            LeaveAlternateScreen,
            Show,
            ResetColor
        )?;
        self.suspended = true;
        Ok(())
    }

    pub fn resume(&mut self) -> io::Result<()> {
        if !self.suspended {
            return Ok(());
        }
        enable_raw_mode()?;
        if let Err(error) = execute!(
            self.terminal.backend_mut(),
            EnterAlternateScreen,
            crossterm::cursor::Hide
        ) {
            restore_terminal(&mut self.terminal);
            self.suspended = true;
            return Err(error);
        }
        if let Err(error) = self.terminal.clear() {
            restore_terminal(&mut self.terminal);
            self.suspended = true;
            return Err(error);
        }
        self.suspended = false;
        self.dirty = true;
        Ok(())
    }

    pub fn run<F>(&mut self, mut dispatch: F) -> io::Result<()>
    where
        F: FnMut(String, &mut Self) -> Result<(), String>,
    {
        let mut next_runtime_refresh = Instant::now() + Duration::from_secs(1);
        loop {
            self.drain_messages();
            if self.state.running && Instant::now() >= next_runtime_refresh {
                self.dirty = true;
                next_runtime_refresh = Instant::now() + Duration::from_secs(1);
            }
            if self.dirty && !self.suspended {
                self.draw()?;
            }
            if self.state.quit {
                break;
            }
            if self.suspended {
                return Err(io::Error::other("interactive terminal was left suspended"));
            }
            if event::poll(Duration::from_millis(60))? {
                match event::read()? {
                    Event::Key(key)
                        if matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat) =>
                    {
                        match self.state.handle_key(key) {
                            InputAction::None => self.dirty = true,
                            InputAction::Quit => self.request_exit(),
                            InputAction::Submit(line) => {
                                self.dirty = true;
                                if let Err(error) = dispatch(line, self) {
                                    self.add_activity(format!("Command failed · {error}"));
                                }
                            }
                        }
                    }
                    Event::Paste(text) if !self.state.running => {
                        self.state.insert_text(&sanitize(&text));
                        self.dirty = true;
                    }
                    Event::Resize(_, _) => {
                        self.dirty = true;
                    }
                    _ => {}
                }
            }
        }
        self.suspend()?;
        self.print_transcript();
        Ok(())
    }

    fn drain_messages(&mut self) {
        while let Ok(message) = self.receiver.try_recv() {
            match message {
                UiMessage::Activity(message) => self.state.push_activity(message),
                UiMessage::RuntimeEvent(event) => self.state.apply_runtime_event(*event),
                UiMessage::WorkspaceContext {
                    execution_mode,
                    branch,
                    workspace_dirty,
                } => {
                    self.state.execution_mode = execution_mode;
                    self.state.branch = branch;
                    self.state.workspace_dirty = workspace_dirty;
                }
                UiMessage::RuntimeStatus(status) => self.state.runtime_status = status,
                UiMessage::Approval { tool, response } => {
                    self.state.pending_approval = Some((tool.clone(), response));
                    self.state.current_action = Some("Waiting for approval".to_owned());
                    self.state.status =
                        format!("Approval request · {tool}  |  press Y to approve, N to deny");
                    self.state
                        .push_activity(format!("Approval request · {tool}"));
                }
                UiMessage::RunFinished(result) => {
                    self.state.running = false;
                    self.state.cancellation = None;
                    if let Some(started) = self.state.run_started.take() {
                        self.state.last_run_duration = Some(started.elapsed());
                    }
                    self.state.current_action = None;
                    self.state.active_process = None;
                    if let Some((tool, response)) = self.state.pending_approval.take() {
                        let _ = response.send(false);
                        self.state
                            .push_activity(format!("Approval denied · {tool}"));
                    }
                    match result {
                        Ok(()) => self.state.status = "Ready for the next task".to_owned(),
                        Err(error) => {
                            self.state.status = "Task failed · see activity".to_owned();
                            self.state.push_activity(format!("Task failed · {error}"));
                        }
                    }
                }
                UiMessage::SessionId(session_id) => {
                    if self.state.active_session_id.as_deref() != Some(&session_id) {
                        self.state.approved_plan_available = false;
                    }
                    self.state.active_session_id = Some(session_id);
                }
            }
            self.dirty = true;
        }
    }

    fn draw(&mut self) -> io::Result<()> {
        let colors = self.state.colors;
        self.terminal
            .draw(|frame| draw_ui(frame, &self.state, colors))?;
        self.dirty = false;
        Ok(())
    }

    fn print_transcript(&self) {
        if self.state.activity.is_empty() {
            return;
        }
        println!("CogitoAI activity");
        for item in self.state.activity.iter().rev().take(12).rev() {
            println!("  {item}");
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        if !self.suspended {
            restore_terminal(&mut self.terminal);
            self.suspended = true;
        }
    }
}

fn restore_terminal(terminal: &mut BackendTerminal) {
    let _ = terminal::disable_raw_mode();
    let _ = execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        Show,
        ResetColor
    );
}

fn draw_ui(frame: &mut Frame<'_>, state: &AppState, colors: bool) {
    let area = frame.area();
    let input_rows = state.input.lines().count().clamp(1, 5) as u16;
    let footer_height = 3.min(area.height);
    let notice = state
        .startup
        .notice
        .as_deref()
        .map(str::trim)
        .filter(|notice| !notice.is_empty());
    let preferred_header_height = 3 + u16::from(notice.is_some());
    let header_height = preferred_header_height
        .min(area.height.saturating_sub(footer_height + 4))
        .max(1);
    let input_space = area
        .height
        .saturating_sub(header_height + footer_height + 1)
        .max(1);
    let input_height = (input_rows + 2).min(7).min(input_space);
    let constraints = [
        Constraint::Length(header_height),
        Constraint::Min(1),
        Constraint::Length(input_height),
        Constraint::Length(footer_height),
    ];
    let areas = Layout::vertical(constraints).split(area);

    let accent = if colors { Color::Cyan } else { Color::Reset };
    let muted = if colors {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let header_width = usize::from(areas[0].width);
    let model_label = truncate_status(
        &sanitize(&state.startup.model),
        header_width.saturating_sub(18).saturating_mul(2) / 3,
    );
    let provider_label = sanitize(&state.startup.provider);
    let workspace_label = truncate_path(
        &sanitize(&state.startup.workspace),
        header_width.saturating_sub(" workspace: ".chars().count()),
    );
    let mut header_lines = vec![
        Line::from(vec![
            Span::styled(
                " [<>] COGITOAI",
                Style::default().fg(accent).add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("  harness v{}", state.startup.version),
                Style::default().fg(muted),
            ),
        ]),
        Line::from(format!(" model: {model_label} · {provider_label}")),
        Line::from(format!(" workspace: {workspace_label}")),
    ];
    if let Some(notice) = notice {
        header_lines.push(Line::from(Span::styled(
            format!(
                " {}",
                truncate_status(&sanitize(notice), header_width.saturating_sub(1))
            ),
            Style::default().fg(if colors { Color::Yellow } else { Color::Reset }),
        )));
    }
    let header = Paragraph::new(header_lines);
    frame.render_widget(header, areas[0]);

    let activity_block = Block::default()
        .title(" ACTIVITY ")
        .borders(Borders::TOP)
        .border_style(Style::default().fg(muted));
    let inner_height = activity_block.inner(areas[1]).height as usize;
    let end = state
        .activity
        .len()
        .saturating_sub(state.scroll_from_bottom);
    let start = end.saturating_sub(inner_height);
    let items = state
        .activity
        .iter()
        .skip(start)
        .take(end.saturating_sub(start))
        .map(|message| {
            let color = if !colors {
                Color::Reset
            } else if message.contains("failed") || message.contains("denied") {
                Color::Red
            } else if message.contains("passed")
                || message.contains("completed")
                || message.contains("granted")
            {
                Color::Green
            } else if message.contains("Approval request") || message.contains("Running command") {
                Color::Yellow
            } else {
                Color::Reset
            };
            ListItem::new(Line::from(Span::styled(
                format!("  {message}"),
                Style::default().fg(color),
            )))
        })
        .collect::<Vec<_>>();
    frame.render_widget(List::new(items).block(activity_block), areas[1]);

    let input_block = Block::default()
        .title(format!(
            " INPUT · {}  Enter runs  |  Alt+Enter adds a line ",
            task_mode_label(state.task_mode)
        ))
        .borders(Borders::TOP)
        .border_style(Style::default().fg(muted));
    let input_inner = input_block.inner(areas[2]);
    let prompt = "> ";
    let input = Paragraph::new(format!("{prompt}{}", state.input))
        .wrap(Wrap { trim: false })
        .block(input_block);
    frame.render_widget(input, areas[2]);
    let (cursor_row, cursor_col) = cursor_position(&state.input, state.cursor);
    let cursor_x = input_inner
        .x
        .saturating_add(prompt.len() as u16)
        .saturating_add(cursor_col as u16);
    let cursor_y = input_inner.y.saturating_add(cursor_row as u16);
    if cursor_x < input_inner.right() && cursor_y < input_inner.bottom() {
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }

    let footer_block = Block::default()
        .borders(Borders::TOP)
        .border_style(Style::default().fg(muted));
    let footer_inner = footer_block.inner(areas[3]);
    let detail_style = if colors && state.pending_approval.is_some() {
        Style::default().fg(Color::Yellow)
    } else if colors && state.running {
        Style::default().fg(accent)
    } else {
        Style::default().fg(muted)
    };
    let status = Paragraph::new(vec![
        status_line(state, footer_inner.width, colors),
        Line::from(Span::styled(
            format!(
                " {}",
                truncate_status(
                    &status_detail(state),
                    footer_inner.width.saturating_sub(1) as usize
                )
            ),
            detail_style,
        )),
    ])
    .block(footer_block);
    frame.render_widget(status, areas[3]);
}

fn task_mode_label(mode: TaskMode) -> &'static str {
    match mode {
        TaskMode::Explore => "EXPLORE",
        TaskMode::Plan => "PLAN",
        TaskMode::Code => "CODE",
    }
}

fn status_line(state: &AppState, width: u16, colors: bool) -> Line<'static> {
    let available = usize::from(width.saturating_sub(2));
    let compact_mode = if available < 26 {
        match state.execution_mode.as_str() {
            "read-only" => "ro",
            "normal" => "norm",
            mode => mode,
        }
    } else {
        &state.execution_mode
    };
    let mode = compact_mode.to_owned();
    let runtime = state
        .run_started
        .map(|started| started.elapsed())
        .or(state.last_run_duration)
        .map(format_runtime);

    let mut middle: Vec<(String, Color)> = Vec::new();
    let tokens = state.token_usage.map(|(input, output)| {
        (
            format!("tok {}", format_token_count(input.saturating_add(output))),
            Color::Cyan,
        )
    });
    let branch = workspace_marker(state).map(|branch| (branch, Color::DarkGray));
    let minimum_model_width = if available >= 32 { 10 } else { 1 };
    for candidate in [tokens, branch].into_iter().flatten() {
        let mut candidate_middle = middle.clone();
        candidate_middle.push(candidate.clone());
        let required = status_fixed_width(&mode, &candidate_middle, runtime.as_deref());
        if required.saturating_add(minimum_model_width) <= available {
            middle.push(candidate);
        }
    }

    let fixed_width = status_fixed_width(&mode, &middle, runtime.as_deref());
    let model_width = available.saturating_sub(fixed_width);
    let model = truncate_status(&compact(&status_model(&state.startup), 240), model_width);
    let has_middle = !middle.is_empty();
    let total_width = fixed_width + model.chars().count();
    let extra = available.saturating_sub(total_width);
    let (left_gap, right_gap) = if has_middle && runtime.is_some() {
        (2 + extra / 2, 2 + extra - extra / 2)
    } else if has_middle {
        (2, 0)
    } else if runtime.is_some() {
        (2 + extra, 0)
    } else {
        (0, 0)
    };

    let accent = if colors { Color::Cyan } else { Color::Reset };
    let mode_color = if colors {
        match state.execution_mode.as_str() {
            "read-only" => Color::Blue,
            "safe" => Color::Yellow,
            "normal" => Color::Green,
            "auto" => Color::Magenta,
            _ => Color::Reset,
        }
    } else {
        Color::Reset
    };
    let muted = if colors {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let mut spans = vec![
        Span::raw(" "),
        Span::styled(model, Style::default().fg(accent)),
        Span::styled(" │ ", Style::default().fg(muted)),
        Span::styled(mode, Style::default().fg(mode_color)),
    ];
    if has_middle {
        spans.push(Span::raw(" ".repeat(left_gap)));
        for (index, (text, color)) in middle.into_iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", Style::default().fg(muted)));
            }
            spans.push(Span::styled(text, Style::default().fg(color)));
        }
    }
    if let Some(runtime) = runtime {
        spans.push(Span::raw(" ".repeat(if has_middle {
            right_gap
        } else {
            left_gap
        })));
        spans.push(Span::styled(runtime, Style::default().fg(muted)));
    }
    Line::from(spans)
}

fn status_model(startup: &StartupInfo) -> String {
    let model = startup
        .model
        .strip_prefix(&format!("{}/", startup.provider))
        .unwrap_or(&startup.model);
    format!("{}/{}", startup.provider, model)
}

fn status_fixed_width(mode: &str, middle: &[(String, Color)], runtime: Option<&str>) -> usize {
    let middle_width = middle
        .iter()
        .map(|(text, _)| text.chars().count())
        .sum::<usize>()
        + middle.len().saturating_sub(1) * 3;
    mode.chars().count()
        + 3
        + middle_width
        + runtime.map_or(0, |text| text.chars().count())
        + if middle.is_empty() {
            usize::from(runtime.is_some()) * 2
        } else {
            2 + usize::from(runtime.is_some()) * 2
        }
}

fn workspace_marker(state: &AppState) -> Option<String> {
    let branch = state.branch.as_deref().map(|branch| compact(branch, 28));
    match (branch, state.workspace_dirty) {
        (Some(branch), Some(true)) => Some(format!("{branch}*")),
        (Some(branch), _) => Some(branch),
        (None, Some(true)) => Some("dirty".to_owned()),
        (None, Some(false)) => Some("clean".to_owned()),
        (None, None) => None,
    }
}

fn format_runtime(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds >= 3_600 {
        format!(
            "run {}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("run {}:{:02}", seconds / 60, seconds % 60)
    }
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 10_000 {
        format!("{}k", tokens / 1_000)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

fn truncate_status(value: &str, max_chars: usize) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= max_chars {
        value.to_owned()
    } else if max_chars == 0 {
        String::new()
    } else if max_chars == 1 {
        "…".to_owned()
    } else {
        format!(
            "{}…",
            characters[..max_chars - 1].iter().collect::<String>()
        )
    }
}

fn status_detail(state: &AppState) -> String {
    if let Some((tool, _)) = &state.pending_approval {
        return format!("Approval: {tool} · Y approve · N deny · Ctrl+C cancel");
    }
    if let Some(runtime_status) = &state.runtime_status {
        if runtime_status != "Connected" {
            return runtime_status.clone();
        }
    }
    if state.running {
        let action = if let Some(process) = &state.active_process {
            format!("Process · {process}")
        } else {
            state
                .current_action
                .as_deref()
                .unwrap_or("Agent working")
                .to_owned()
        };
        let processes = background_process_summary(state);
        return format!(
            "{action}{} · Ctrl+C cancel · PgUp/PgDn scroll",
            processes.map_or_else(String::new, |summary| format!(" · {summary}"))
        );
    }
    if let Some(processes) = background_process_summary(state) {
        return format!("{processes} · Enter run · ↑/↓ history · /help");
    }
    if state.status != "Ready  |  Enter a task or type /help" {
        return state.status.clone();
    }
    if state.runtime_status.as_deref() == Some("Connected") {
        return "Connected · Enter run · ↑/↓ history · /help".to_owned();
    }
    "Enter run · ↑/↓ history · /help · Ctrl+C exit".to_owned()
}

fn background_process_summary(state: &AppState) -> Option<String> {
    let (id, command) = state.background_processes.iter().next()?;
    Some(format!(
        "{} bg · {} {}",
        state.background_processes.len(),
        &id[..id.len().min(12)],
        command
    ))
}

fn truncate_path(value: &str, max_chars: usize) -> String {
    let characters = value.chars().collect::<Vec<_>>();
    if characters.len() <= max_chars {
        value.to_owned()
    } else if max_chars == 0 {
        String::new()
    } else if max_chars == 1 {
        "…".to_owned()
    } else {
        format!(
            "…{}",
            characters[characters.len() - max_chars + 1..]
                .iter()
                .collect::<String>()
        )
    }
}

fn cursor_position(input: &str, cursor: usize) -> (usize, usize) {
    let before = &input[..cursor.min(input.len())];
    let row = before.matches('\n').count();
    let col = before
        .rsplit('\n')
        .next()
        .map_or(0, |line| line.chars().count());
    (row, col)
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() && character != '\n' {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn compact(value: &str, max: usize) -> String {
    let value = sanitize(value)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let chars = value.chars().collect::<Vec<_>>();
    if chars.len() <= max {
        value
    } else {
        format!(
            "{}...",
            chars[..max.saturating_sub(3)].iter().collect::<String>()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn startup() -> StartupInfo {
        StartupInfo {
            model: "mock".to_owned(),
            provider: "Mock".to_owned(),
            version: "0.1.0".to_owned(),
            workspace: "C:/work/project".to_owned(),
            notice: Some(
                "Workspace is not a Git repository; checkpoints are unavailable".to_owned(),
            ),
            execution_mode: "normal".to_owned(),
            branch: Some("main".to_owned()),
            workspace_dirty: Some(false),
        }
    }

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn editor_supports_multiline_input_and_submission_history() {
        let mut state = AppState::new(startup(), false);
        state.insert_text("fix the sidebar");
        state.handle_key(key(KeyCode::Enter, KeyModifiers::ALT));
        state.insert_text("keep json output");

        assert_eq!(
            state.handle_key(key(KeyCode::Enter, KeyModifiers::NONE)),
            InputAction::Submit("fix the sidebar\nkeep json output".to_owned())
        );
        assert!(state.input.is_empty());
        assert_eq!(state.history, ["fix the sidebar\nkeep json output"]);

        state.handle_key(key(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(state.input, "fix the sidebar\nkeep json output");
    }

    #[test]
    fn ctrl_c_cancels_a_run_and_default_denies_a_pending_approval() {
        let mut state = AppState::new(startup(), false);
        let cancellation = CancellationToken::new();
        state.running = true;
        state.cancellation = Some(cancellation.clone());
        let (response, answer) = mpsc::sync_channel(1);
        state.pending_approval = Some(("shell".to_owned(), response));

        state.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL));

        assert!(cancellation.is_cancelled());
        assert!(!answer.try_recv().unwrap());
    }

    #[test]
    fn ctrl_c_and_ctrl_d_exit_only_from_an_empty_idle_prompt() {
        let mut state = AppState::new(startup(), false);
        state.insert_text("draft");
        assert_eq!(
            state.handle_key(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            InputAction::None
        );
        assert!(state.input.is_empty());
        assert_eq!(
            state.handle_key(key(KeyCode::Char('d'), KeyModifiers::CONTROL)),
            InputAction::Quit
        );
    }

    #[test]
    fn activity_scroll_stays_put_while_new_events_arrive() {
        let mut state = AppState::new(startup(), false);
        state.scroll_from_bottom = 8;

        state.push_activity("Running command cargo test".to_owned());

        assert_eq!(state.scroll_from_bottom, 8);
    }

    #[test]
    fn render_adapts_to_resize_and_contains_startup_activity_and_prompt() {
        let mut state = AppState::new(startup(), false);
        state.push_activity("Reading file src/main.rs".to_owned());
        state.insert_text("inspect workspace");
        let mut terminal = Terminal::new(TestBackend::new(72, 20)).unwrap();
        terminal
            .draw(|frame| draw_ui(frame, &state, false))
            .unwrap();
        let initial = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(initial.contains("COGITOAI"));
        assert!(initial.contains("Reading file src/main.rs"));
        assert!(initial.contains("inspect workspace"));

        terminal.backend_mut().resize(36, 12);
        terminal
            .draw(|frame| draw_ui(frame, &state, false))
            .unwrap();
        let resized = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(resized.contains("COGITOAI"));
        assert!(resized.contains("inspect workspace"));
        assert_eq!(
            terminal.backend().buffer().area,
            ratatui::layout::Rect::new(0, 0, 36, 12)
        );
    }

    #[test]
    fn startup_snapshot_stays_compact_without_an_empty_notice_row() {
        let mut state = AppState::new(startup(), false);
        state.startup.notice = None;
        let mut terminal = Terminal::new(TestBackend::new(72, 16)).unwrap();
        terminal
            .draw(|frame| draw_ui(frame, &state, false))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let startup_rows = (0..3)
            .map(|row| {
                (0..buffer.area.width)
                    .map(|column| buffer[(column, row)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>();
        assert_eq!(
            startup_rows,
            [
                " [<>] COGITOAI  harness v0.1.0",
                " model: mock · Mock",
                " workspace: C:/work/project",
            ]
        );

        let activity_heading = (0..buffer.area.width)
            .map(|column| buffer[(column, 3)].symbol())
            .collect::<String>();
        assert!(activity_heading.contains("ACTIVITY"));
        assert!(!activity_heading.contains('┌'));
    }

    #[test]
    fn task_behavior_mode_is_visible_separately_from_execution_permission() {
        for (mode, label) in [
            (TaskMode::Explore, "EXPLORE"),
            (TaskMode::Plan, "PLAN"),
            (TaskMode::Code, "CODE"),
        ] {
            let mut state = AppState::new(startup(), false);
            state.task_mode = mode;
            let mut terminal = Terminal::new(TestBackend::new(80, 16)).unwrap();
            terminal
                .draw(|frame| draw_ui(frame, &state, false))
                .unwrap();
            let rendered = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect::<String>();

            assert!(rendered.contains(&format!("INPUT · {label}")));
            assert!(
                rendered.contains("normal"),
                "execution permission remains visible"
            );
        }
    }

    #[test]
    fn switching_a_completed_plan_to_code_reuses_its_session_once() {
        let mut state = AppState::new(startup(), false);
        state.task_mode = TaskMode::Plan;
        state.active_session_id = Some("plan-session".to_owned());
        let mut task_run = harness_session::TaskRun::new("implement the requested change");
        task_run.task_mode = TaskMode::Plan;
        task_run.structured_plan = Some(harness_session::ImplementationPlan {
            goal: task_run.original_goal.clone(),
            ..harness_session::ImplementationPlan::default()
        });
        task_run.completion_status = harness_session::TaskCompletionStatus::Done;
        state.apply_runtime_event(EventPayload::TaskRunUpdated {
            task_run: task_run.clone(),
        });

        assert_eq!(state.approved_plan_session_for_code(), None);
        state.task_mode = TaskMode::Code;
        assert_eq!(state.approved_plan_session_for_code(), Some("plan-session"));

        task_run.task_mode = TaskMode::Code;
        task_run.completion_status = harness_session::TaskCompletionStatus::InProgress;
        state.apply_runtime_event(EventPayload::TaskRunUpdated { task_run });
        assert_eq!(state.approved_plan_session_for_code(), None);
    }

    #[test]
    fn startup_and_status_truncate_long_context_without_losing_workspace_tail() {
        let mut state = AppState::new(startup(), false);
        state.startup.workspace =
            "C:/Users/Developer/Documents/GitHub/OctranTechnologies/CogitoAI".to_owned();
        state.startup.model = "anthropic/claude-sonnet-4.5-with-a-long-model-name".to_owned();
        state.startup.notice = None;
        let mut terminal = Terminal::new(TestBackend::new(60, 16)).unwrap();
        terminal
            .draw(|frame| draw_ui(frame, &state, false))
            .unwrap();

        let buffer = terminal.backend().buffer();
        let model_row = (0..buffer.area.width)
            .map(|column| buffer[(column, 1)].symbol())
            .collect::<String>();
        let workspace_row = (0..buffer.area.width)
            .map(|column| buffer[(column, 2)].symbol())
            .collect::<String>();
        assert!(model_row.contains("anthropic/claude"));
        assert!(model_row.chars().count() <= usize::from(buffer.area.width));
        assert!(workspace_row.contains("…"));
        assert!(workspace_row.contains("OctranTechnologies/CogitoAI"));
        assert!(workspace_row.chars().count() <= usize::from(buffer.area.width));
    }

    #[test]
    fn idle_footer_surfaces_recent_feedback_before_shortcut_hints() {
        let mut state = AppState::new(startup(), false);
        state.status = "Input cleared".to_owned();

        assert_eq!(status_detail(&state), "Input cleared");
    }

    #[test]
    fn footer_prioritizes_runtime_connection_state_until_connected() {
        let mut state = AppState::new(startup(), false);
        for (connection, expected) in [
            ("Connecting...", "Connecting..."),
            ("Starting runtime...", "Starting runtime..."),
            ("Reconnecting...", "Reconnecting..."),
            ("Runtime unavailable", "Runtime unavailable"),
        ] {
            state.runtime_status = Some(connection.to_owned());
            assert_eq!(status_detail(&state), expected);
        }
        state.runtime_status = Some("Connected".to_owned());
        assert_eq!(
            status_detail(&state),
            "Connected · Enter run · ↑/↓ history · /help"
        );
        state.status = "Task failed · see activity".to_owned();
        assert_eq!(status_detail(&state), "Task failed · see activity");
    }

    fn status_text(state: &AppState, width: u16) -> String {
        status_line(state, width, false)
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    fn status_fixture() -> AppState {
        let mut state = AppState::new(startup(), false);
        state.startup.provider = "anthropic".to_owned();
        state.startup.model = "anthropic/claude-sonnet-4.5-super-long-model-name".to_owned();
        state.execution_mode = "normal".to_owned();
        state.branch = Some("main".to_owned());
        state.workspace_dirty = Some(true);
        state.token_usage = Some((12_345, 678));
        state.last_run_duration = Some(Duration::from_secs(65));
        state
    }

    #[test]
    fn status_line_fits_requested_widths_and_shortens_long_model_names() {
        let state = status_fixture();
        let snapshots = [
            (
                60,
                " anthropic/claude-sonn… │ normal  tok 13k · main*  run 1:05",
            ),
            (
                80,
                " anthropic/claude-sonnet-4.5-super-long-mo… │ normal  tok 13k · main*  run 1:05",
            ),
            (
                120,
                " anthropic/claude-sonnet-4.5-super-long-model-name │ normal                  tok 13k · main*                   run 1:05",
            ),
            (
                180,
                " anthropic/claude-sonnet-4.5-super-long-model-name │ normal                                                tok 13k · main*                                                 run 1:05",
            ),
        ];
        for (width, expected) in snapshots {
            let rendered = status_text(&state, width);
            assert_eq!(rendered, expected, "{width} column snapshot");
            assert!(
                rendered.chars().count() < usize::from(width),
                "status overflowed {width} columns: {rendered:?}"
            );
            assert!(rendered.contains("normal"), "{rendered:?}");
            assert!(rendered.contains("tok 13k"), "{rendered:?}");
            assert!(rendered.contains("main*"), "{rendered:?}");
            assert!(rendered.contains("run 1:05"), "{rendered:?}");
        }
        assert!(status_text(&state, 60).contains("…"));
        assert!(status_text(&state, 180).contains(&state.startup.model));
    }

    #[test]
    fn unavailable_token_context_and_cost_metrics_are_hidden() {
        let mut state = status_fixture();
        state.token_usage = None;
        let rendered = status_text(&state, 120);

        assert!(!rendered.contains("tok"));
        assert!(!rendered.contains("ctx"));
        assert!(!rendered.contains('$'));

        state.record_token_usage(Some(42), None);
        state.record_token_usage(Some(10), Some(5));
        assert!(state.token_usage.is_none());
        assert!(!status_text(&state, 120).contains("tok"));

        let mut mock_state = AppState::new(startup(), false);
        mock_state.apply_runtime_event(EventPayload::ModelResponse {
            provider: "scripted-mock".to_owned(),
            model: "mock".to_owned(),
            text: String::new(),
            input_tokens: Some(100),
            output_tokens: Some(20),
        });
        assert!(mock_state.token_usage.is_none());
        assert!(!status_text(&mock_state, 120).contains("tok"));
    }

    #[test]
    fn background_processes_stay_visible_after_the_task_finishes_and_clear_on_exit() {
        let mut state = AppState::new(startup(), false);
        state.runtime_status = Some("Connected".to_owned());
        state.apply_runtime_event(EventPayload::BackgroundProcessStarted {
            process_id: "proc-123-0".to_owned(),
            command: "npm run dev".to_owned(),
            working_directory: "C:/repo".into(),
            pid: 123,
            started_at_unix_ms: 1,
        });
        assert!(status_detail(&state).contains("npm run dev"));

        state.apply_runtime_event(EventPayload::BackgroundProcessStatus {
            process_id: "proc-123-0".to_owned(),
            pid: 123,
            status: "exited".to_owned(),
            exit_code: Some(0),
            timed_out: false,
        });
        assert_eq!(
            status_detail(&state),
            "Connected · Enter run · ↑/↓ history · /help"
        );
    }

    #[test]
    fn execution_modes_have_distinct_restrained_colors() {
        for (mode, color) in [
            ("read-only", Color::Blue),
            ("safe", Color::Yellow),
            ("normal", Color::Green),
            ("auto", Color::Magenta),
        ] {
            let mut state = status_fixture();
            state.execution_mode = mode.to_owned();
            let line = status_line(&state, 120, true);
            let mode_span = line
                .spans
                .iter()
                .find(|span| span.content.as_ref() == mode)
                .unwrap();
            assert_eq!(mode_span.style.fg, Some(color));
        }
    }

    #[test]
    fn resize_during_a_run_preserves_status_action_and_input() {
        let mut state = status_fixture();
        state.running = true;
        state.run_started = Some(Instant::now());
        state.active_process = Some("cargo test".to_owned());
        state.current_action = Some("Running command".to_owned());
        let mut terminal = Terminal::new(TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| draw_ui(frame, &state, false))
            .unwrap();

        terminal.backend_mut().resize(60, 12);
        terminal
            .draw(|frame| draw_ui(frame, &state, false))
            .unwrap();
        let resized = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();

        assert!(resized.contains("normal"));
        assert!(resized.contains("Process · cargo test"));
        assert!(resized.contains("Ctrl+C cancel"));
        assert!(resized.contains("> "));
    }
}
