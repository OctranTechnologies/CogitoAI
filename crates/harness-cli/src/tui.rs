use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    self, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen, SetTitle,
};
use crossterm::{cursor::Show, style::ResetColor};
use harness_tools::CancellationToken;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, Paragraph, Wrap};
use ratatui::{Frame, Terminal};

const ACTIVITY_LIMIT: usize = 200;
const HISTORY_LIMIT: usize = 100;
const SLASH_COMMANDS: &[&str] = &[
    "/cancel",
    "/clear",
    "/config",
    "/diff",
    "/exit",
    "/help",
    "/inspect",
    "/model",
    "/resume",
    "/run",
    "/sessions",
    "/status",
    "/undo",
];

type BackendTerminal = Terminal<CrosstermBackend<Stdout>>;

#[derive(Clone, Debug)]
pub struct StartupInfo {
    pub model: String,
    pub provider: String,
    pub version: String,
    pub workspace: String,
    pub notice: Option<String>,
}

enum UiMessage {
    Activity(String),
    Approval {
        tool: String,
        response: SyncSender<bool>,
    },
    RunFinished(Result<(), String>),
}

#[derive(Clone)]
pub struct TuiSender {
    sender: Sender<UiMessage>,
}

impl TuiSender {
    pub fn activity(&self, message: impl Into<String>) {
        let _ = self.sender.send(UiMessage::Activity(message.into()));
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
}

#[derive(Debug, Eq, PartialEq)]
enum InputAction {
    None,
    Submit(String),
    Quit,
}

struct AppState {
    startup: StartupInfo,
    activity: VecDeque<String>,
    input: String,
    cursor: usize,
    history: Vec<String>,
    history_index: Option<usize>,
    history_scratch: String,
    scroll_from_bottom: usize,
    status: String,
    running: bool,
    cancellation: Option<CancellationToken>,
    pending_approval: Option<(String, SyncSender<bool>)>,
    quit: bool,
    colors: bool,
}

impl AppState {
    fn new(startup: StartupInfo, colors: bool) -> Self {
        let mut state = Self {
            startup,
            activity: VecDeque::new(),
            input: String::new(),
            cursor: 0,
            history: Vec::new(),
            history_index: None,
            history_scratch: String::new(),
            scroll_from_bottom: 0,
            status: "Ready  |  Enter a task or type /help".to_owned(),
            running: false,
            cancellation: None,
            pending_approval: None,
            quit: false,
            colors,
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
                        self.push_activity(format!("Approval granted · {tool}"));
                    }
                    KeyCode::Char('n' | 'N') | KeyCode::Esc => {
                        let _ = response.send(false);
                        self.status = format!("Denied · {tool}");
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
        let matches = SLASH_COMMANDS
            .iter()
            .copied()
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
}

impl Tui {
    pub fn new(startup: StartupInfo) -> io::Result<Self> {
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
        })
    }

    pub fn sender(&self) -> TuiSender {
        self.sender.clone()
    }

    pub fn start_run(&mut self, label: &str, cancellation: CancellationToken) -> bool {
        if self.state.running {
            return false;
        }
        self.state.running = true;
        self.state.cancellation = Some(cancellation);
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
        loop {
            self.drain_messages();
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
                UiMessage::Approval { tool, response } => {
                    self.state.pending_approval = Some((tool.clone(), response));
                    self.state.status =
                        format!("Approval request · {tool}  |  press Y to approve, N to deny");
                    self.state
                        .push_activity(format!("Approval request · {tool}"));
                }
                UiMessage::RunFinished(result) => {
                    self.state.running = false;
                    self.state.cancellation = None;
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
    let input_height = (input_rows + 2).min(area.height.saturating_sub(2).max(2));
    let constraints = [
        Constraint::Length(4),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
    ];
    let areas = Layout::vertical(constraints).split(area);

    let accent = if colors { Color::Cyan } else { Color::Reset };
    let muted = if colors {
        Color::DarkGray
    } else {
        Color::Reset
    };
    let header = Paragraph::new(vec![
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
        Line::from(format!(
            " Model: {}  |  Provider: {}",
            sanitize(&state.startup.model),
            sanitize(&state.startup.provider)
        )),
        Line::from(format!(
            " Workspace: {}",
            sanitize(&state.startup.workspace)
        )),
        Line::from(
            state
                .startup
                .notice
                .as_deref()
                .map(sanitize)
                .unwrap_or_default(),
        ),
    ]);
    frame.render_widget(header, areas[0]);

    let activity_block = Block::default()
        .title(" ACTIVITY ")
        .borders(Borders::ALL)
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

    let status_style = if colors {
        Style::default().fg(accent)
    } else {
        Style::default()
    };
    frame.render_widget(
        Paragraph::new(format!(" {}", sanitize(&state.status))).style(status_style),
        areas[2],
    );

    let input_block = Block::default()
        .title(" INPUT  Enter runs  |  Alt+Enter adds a line ")
        .borders(Borders::TOP)
        .border_style(Style::default().fg(muted));
    let input_inner = input_block.inner(areas[3]);
    let prompt = "> ";
    let input = Paragraph::new(format!("{prompt}{}", state.input))
        .wrap(Wrap { trim: false })
        .block(input_block);
    frame.render_widget(input, areas[3]);
    let (cursor_row, cursor_col) = cursor_position(&state.input, state.cursor);
    let cursor_x = input_inner
        .x
        .saturating_add(prompt.len() as u16)
        .saturating_add(cursor_col as u16);
    let cursor_y = input_inner.y.saturating_add(cursor_row as u16);
    if cursor_x < input_inner.right() && cursor_y < input_inner.bottom() {
        frame.set_cursor_position(Position::new(cursor_x, cursor_y));
    }

    let footer = if state.running {
        " Ctrl+C cancels  |  PageUp/PageDown scroll activity  |  approvals: Y / N"
    } else {
        " Ctrl+C / Ctrl+D exit  |  Up/Down history  |  Tab completes commands  |  /help"
    };
    frame.render_widget(
        Paragraph::new(footer).style(Style::default().fg(muted)),
        areas[4],
    );
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
}
