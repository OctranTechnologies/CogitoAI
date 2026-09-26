//! Human-operated pseudo-terminal sessions.
//!
//! # Security boundary
//!
//! A [`PtyManager`] spawns a *human* terminal: a person at the desktop shell is
//! typing into it directly. That is deliberately **not** the same thing as an
//! agent running a command, and the two must never be conflated.
//!
//! * **Agent-controlled command execution** goes through
//!   `harness_tools::ToolRegistry`, which evaluates every call against the
//!   policy engine (deny / ask / allow) and runs it through a captured,
//!   non-interactive `ProcessRunner`. It is bounded by workspace path rules,
//!   approval prompts, timeouts, and output caps.
//!
//! * **Human-controlled terminal sessions** are the interactive PTYs in this
//!   crate. A human is present and directly responsible for everything typed, so
//!   the agent policy engine is intentionally **not** consulted: prompting a
//!   person to approve their own keystrokes would be noise, not safety.
//!
//! The safety of a human PTY rests on three properties, all enforced here:
//!
//! 1. **The agent cannot reach it.** This crate exposes no
//!    `harness_tools::Tool` implementation and is never registered in a
//!    `ToolRegistry`, so no model-driven tool call can open, write to, or resize
//!    a PTY. Agent shell commands cannot be routed through this path.
//! 2. **It is workspace-confined.** A session's working directory must resolve
//!    inside the runtime's open workspace.
//! 3. **Attribution is explicit.** Callers must state [`SessionOrigin::Human`],
//!    so the human path is a visible decision rather than an inferred one.
//!
//! In short: agent commands are policy-governed and non-interactive; human
//! terminals are interactive and ungoverned, and never substitute for one
//! another.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{SystemTime, UNIX_EPOCH};

use harness_core::Id;
use portable_pty::{CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};

/// Identifier for a live PTY session.
pub type PtyId = Id;

/// Default PTY geometry used when a client does not supply one.
pub const DEFAULT_COLS: u16 = 80;
/// Default PTY geometry used when a client does not supply one.
pub const DEFAULT_ROWS: u16 = 24;

/// Maximum bytes read from a PTY in a single read.
const READ_BUFFER_BYTES: usize = 8_192;
/// Upper bound on a single chunk forwarded to clients.
const MAX_CHUNK_BYTES: usize = 64 * 1024;
/// How often the reaper thread checks whether the shell has exited.
const REAP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(20);

/// Shared, late-bindable notification sink.
///
/// The sink is held behind a shared cell so reader and reaper threads can reach
/// it without holding the manager, and so it may be installed after sessions
/// already exist.
type SinkCell = Arc<Mutex<Option<Arc<dyn TerminalSink>>>>;

/// Who is operating a terminal session.
///
/// Only [`SessionOrigin::Human`] is accepted. The single-variant enum makes the
/// human path explicit at every call site instead of implied by a missing
/// parameter, so agent use would be a visible, deliberate change.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionOrigin {
    /// A person is typing into this terminal from the desktop shell.
    Human,
}

impl SessionOrigin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Human => "human",
        }
    }
}

/// Why a terminal session ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    /// The shell or its foreground child exited on its own.
    Exited,
    /// A human asked to close the session.
    Closed,
}

#[derive(Debug, thiserror::Error)]
pub enum PtyError {
    #[error("terminal origin must be 'human'")]
    InvalidOrigin,
    #[error("working directory is outside the open workspace: {path}")]
    OutsideWorkspace { path: PathBuf },
    #[error("could not start pseudo-terminal: {message}")]
    Spawn { message: String },
    #[error("could not access pseudo-terminal: {message}")]
    Io { message: String },
    #[error("terminal size must be greater than zero")]
    InvalidSize,
    #[error("no such terminal session: {id}")]
    NotFound { id: String },
    #[error("terminal session already closed: {id}")]
    Closed { id: String },
}

impl PtyError {
    /// Stable machine-readable code for RPC clients.
    pub fn code(&self) -> &'static str {
        match self {
            Self::InvalidOrigin => "invalid_origin",
            Self::OutsideWorkspace { .. } => "outside_workspace",
            Self::Spawn { .. } => "spawn_failed",
            Self::Io { .. } => "io_error",
            Self::InvalidSize => "invalid_size",
            Self::NotFound { .. } => "terminal_not_found",
            Self::Closed { .. } => "terminal_closed",
        }
    }
}

/// A PTY notification produced by the runtime.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TerminalEvent {
    /// A chunk of terminal output. stdout and stderr are interleaved by the PTY
    /// exactly as a real terminal presents them.
    Output { terminal_id: String, data: String },
    /// The session ended.
    Exited {
        terminal_id: String,
        exit_code: Option<u32>,
        reason: ExitReason,
    },
}

/// Sink for terminal notifications, wired to RPC broadcast by the server.
pub trait TerminalSink: Send + Sync {
    fn publish(&self, event: TerminalEvent);
}

impl<F> TerminalSink for F
where
    F: Fn(TerminalEvent) + Send + Sync,
{
    fn publish(&self, event: TerminalEvent) {
        self(event);
    }
}

fn publish(sink: &SinkCell, event: TerminalEvent) {
    // A poisoned lock must not take down a terminal; losing a notification is
    // preferable to killing the reader thread mid-stream.
    if let Ok(guard) = sink.lock() {
        if let Some(sink) = guard.as_ref() {
            sink.publish(event);
        }
    }
}

/// Description of a terminal to open.
#[derive(Clone, Debug)]
pub struct PtyRequest {
    pub origin: SessionOrigin,
    /// Shell program to run. Defaults to the platform shell when `None`.
    pub program: Option<String>,
    /// Arguments for the shell program.
    pub args: Vec<String>,
    /// Working directory, which must be inside the workspace.
    pub working_directory: PathBuf,
    pub cols: u16,
    pub rows: u16,
}

impl PtyRequest {
    /// A request for the default interactive shell at `working_directory`.
    pub fn human_shell(working_directory: impl Into<PathBuf>) -> Self {
        Self {
            origin: SessionOrigin::Human,
            program: None,
            args: Vec::new(),
            working_directory: working_directory.into(),
            cols: DEFAULT_COLS,
            rows: DEFAULT_ROWS,
        }
    }
}

/// Public state of a terminal session.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PtyInfo {
    pub id: String,
    pub program: String,
    pub working_directory: String,
    pub origin: String,
    pub cols: u16,
    pub rows: u16,
    pub pid: Option<u32>,
}

/// A live human terminal session.
struct PtySession {
    info: Mutex<PtyInfo>,
    writer: Mutex<Box<dyn Write + Send>>,
    child: Mutex<Box<dyn portable_pty::Child + Send + Sync>>,
    closed: Arc<AtomicBool>,
    /// Retained for the session's lifetime: dropping the master early would send
    /// a hangup before the shell has been asked to exit.
    master: Mutex<Box<dyn MasterPty + Send>>,
}

impl PtySession {
    fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    fn info(&self) -> PtyInfo {
        self.info
            .lock()
            .map(|info| info.clone())
            .unwrap_or_else(|_| PtyInfo {
                id: String::new(),
                program: String::new(),
                working_directory: String::new(),
                origin: String::new(),
                cols: 0,
                rows: 0,
                pid: None,
            })
    }
}

/// Owns every live human terminal for one runtime.
pub struct PtyManager {
    workspace_root: PathBuf,
    sessions: Mutex<HashMap<String, Arc<PtySession>>>,
    next_id: AtomicU64,
    sink: SinkCell,
}

impl PtyManager {
    /// Creates a manager confined to `workspace_root`.
    pub fn new(workspace_root: impl Into<PathBuf>) -> Self {
        Self {
            workspace_root: workspace_root.into(),
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            sink: Arc::new(Mutex::new(None)),
        }
    }

    /// Registers the notification sink for output and exit events.
    pub fn set_sink(&self, sink: Arc<dyn TerminalSink>) {
        *self.sink.lock().expect("PTY sink lock poisoned") = Some(sink);
    }

    /// Opens a human terminal session.
    ///
    /// This is the only way to create a PTY, and it always requires
    /// [`SessionOrigin::Human`].
    pub fn open(&self, request: PtyRequest) -> Result<PtyInfo, PtyError> {
        if request.origin != SessionOrigin::Human {
            return Err(PtyError::InvalidOrigin);
        }
        if request.cols == 0 || request.rows == 0 {
            return Err(PtyError::InvalidSize);
        }
        let working_directory = self.confine(&request.working_directory)?;

        let pty_system = portable_pty::native_pty_system();
        let pair = pty_system
            .openpty(PtySize {
                rows: request.rows,
                cols: request.cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| PtyError::Spawn {
                message: error.to_string(),
            })?;

        let (program, args) = resolve_program(request.program.as_deref(), &request.args);
        let mut command = CommandBuilder::new(&program);
        for arg in &args {
            command.arg(arg);
        }
        command.cwd(&working_directory);
        apply_platform_environment(&mut command);

        let child = pair
            .slave
            .spawn_command(command)
            .map_err(|error| PtyError::Spawn {
                message: error.to_string(),
            })?;
        // The slave handle is not needed once the child is running; dropping it
        // here is what lets the PTY report EOF when the child exits.
        drop(pair.slave);

        let pid = child.process_id();
        let writer = pair.master.take_writer().map_err(|error| PtyError::Io {
            message: error.to_string(),
        })?;
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| PtyError::Io {
                message: error.to_string(),
            })?;

        let sequence = self.next_id.fetch_add(1, Ordering::Relaxed);
        let id = PtyId::new(format!(
            "pty-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |duration| duration.as_millis()),
            sequence
        ))
        .map_err(|error| PtyError::Spawn {
            message: error.to_string(),
        })?;

        let info = PtyInfo {
            id: id.to_string(),
            program,
            working_directory: working_directory.to_string_lossy().into_owned(),
            origin: request.origin.as_str().to_owned(),
            cols: request.cols,
            rows: request.rows,
            pid,
        };

        let session = Arc::new(PtySession {
            info: Mutex::new(info.clone()),
            writer: Mutex::new(writer),
            child: Mutex::new(child),
            closed: Arc::new(AtomicBool::new(false)),
            master: Mutex::new(pair.master),
        });
        self.sessions
            .lock()
            .expect("PTY session lock poisoned")
            .insert(id.to_string(), Arc::clone(&session));

        self.spawn_reader(id.to_string(), reader);
        self.spawn_reaper(id.to_string(), session);
        Ok(info)
    }

    /// Streams input to a session's stdin.
    ///
    /// Control characters such as `\u{3}` (Ctrl+C) are forwarded verbatim, so
    /// the shell's own line discipline decides what they mean.
    pub fn write(&self, id: &str, data: &str) -> Result<(), PtyError> {
        let session = self.session(id)?;
        if session.is_closed() {
            return Err(PtyError::Closed { id: id.to_owned() });
        }
        let mut writer = session.writer.lock().map_err(|_| PtyError::Io {
            message: "terminal writer unavailable".to_owned(),
        })?;
        writer
            .write_all(data.as_bytes())
            .and_then(|()| writer.flush())
            .map_err(|error| PtyError::Io {
                message: error.to_string(),
            })
    }

    /// Resizes a session so full-screen programs redraw at the new size.
    pub fn resize(&self, id: &str, cols: u16, rows: u16) -> Result<PtyInfo, PtyError> {
        if cols == 0 || rows == 0 {
            return Err(PtyError::InvalidSize);
        }
        let session = self.session(id)?;
        if session.is_closed() {
            return Err(PtyError::Closed { id: id.to_owned() });
        }
        session
            .master
            .lock()
            .map_err(|_| PtyError::Io {
                message: "terminal master unavailable".to_owned(),
            })?
            .resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| PtyError::Io {
                message: error.to_string(),
            })?;
        // Keep the reported geometry in step with the real PTY.
        let mut info = session.info.lock().map_err(|_| PtyError::Io {
            message: "terminal state unavailable".to_owned(),
        })?;
        info.cols = cols;
        info.rows = rows;
        Ok(info.clone())
    }

    /// Returns the current public state of a session.
    pub fn info(&self, id: &str) -> Result<PtyInfo, PtyError> {
        Ok(self.session(id)?.info())
    }

    /// Lists live sessions.
    pub fn list(&self) -> Vec<PtyInfo> {
        let sessions = self.sessions.lock().expect("PTY session lock poisoned");
        let mut listed: Vec<PtyInfo> = sessions.values().map(|session| session.info()).collect();
        listed.sort_by(|left, right| left.id.cmp(&right.id));
        listed
    }

    /// Terminates a session at a human's request.
    pub fn close(&self, id: &str) -> Result<(), PtyError> {
        let session = self.session(id)?;
        if !session.closed.swap(true, Ordering::AcqRel) {
            if let Ok(mut child) = session.child.lock() {
                let _ = child.kill();
            }
        }
        self.sessions
            .lock()
            .expect("PTY session lock poisoned")
            .remove(id);
        let sink = self.sink.lock().expect("PTY sink lock poisoned");
        if let Some(sink) = sink.as_ref() {
            sink.publish(TerminalEvent::Exited {
                terminal_id: id.to_owned(),
                exit_code: None,
                reason: ExitReason::Closed,
            });
        }
        Ok(())
    }

    /// Terminates every live session.
    ///
    /// Called when the runtime shuts down and when the desktop window closes, so
    /// no PTY outlives the process that owns it.
    pub fn close_all(&self) {
        let ids: Vec<String> = self
            .sessions
            .lock()
            .expect("PTY session lock poisoned")
            .keys()
            .cloned()
            .collect();
        for id in ids {
            let _ = self.close(&id);
        }
    }

    fn session(&self, id: &str) -> Result<Arc<PtySession>, PtyError> {
        self.sessions
            .lock()
            .expect("PTY session lock poisoned")
            .get(id)
            .cloned()
            .ok_or_else(|| PtyError::NotFound { id: id.to_owned() })
    }

    /// Rejects any working directory outside the open workspace.
    fn confine(&self, path: &Path) -> Result<PathBuf, PtyError> {
        let root = strip_verbatim_prefix(
            std::fs::canonicalize(&self.workspace_root)
                .unwrap_or_else(|_| self.workspace_root.clone()),
        );
        let requested = strip_verbatim_prefix(
            std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()),
        );
        if !requested.starts_with(&root) {
            return Err(PtyError::OutsideWorkspace { path: requested });
        }
        Ok(requested)
    }

    /// Reads the PTY on its own thread, decoding UTF-8 incrementally.
    ///
    /// A PTY is a byte stream, so one multi-byte character can be split across
    /// reads. Buffering the trailing partial sequence keeps Unicode output
    /// intact instead of emitting replacement characters at chunk boundaries.
    fn spawn_reader(&self, terminal_id: String, mut reader: Box<dyn Read + Send>) {
        let sink = Arc::clone(&self.sink);
        thread::spawn(move || {
            let mut buffer = [0_u8; READ_BUFFER_BYTES];
            let mut pending: Vec<u8> = Vec::new();
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(count) => count,
                };
                pending.extend_from_slice(&buffer[..count]);
                let text = take_complete_utf8(&mut pending);
                for chunk in split_on_char_boundaries(&text, MAX_CHUNK_BYTES) {
                    publish(
                        &sink,
                        TerminalEvent::Output {
                            terminal_id: terminal_id.clone(),
                            data: chunk,
                        },
                    );
                }
            }
            // Flush a trailing partial sequence so output is never silently lost.
            if !pending.is_empty() {
                publish(
                    &sink,
                    TerminalEvent::Output {
                        terminal_id,
                        data: String::from_utf8_lossy(&pending).into_owned(),
                    },
                );
            }
            // Deliberately does not mark the session closed: the reaper thread is
            // the single authority for "the shell exited", and setting the flag
            // here would suppress the exit notification it is responsible for.
        });
    }

    /// Watches for the child to exit and reports the exit exactly once.
    ///
    /// This polls `try_wait` instead of blocking in `wait`, because a blocking
    /// `wait` would hold the child lock for the process's whole lifetime and
    /// deadlock `close`, which needs that same lock to kill the shell.
    fn spawn_reaper(&self, terminal_id: String, session: Arc<PtySession>) {
        let sink = Arc::clone(&self.sink);
        thread::spawn(move || loop {
            let status = {
                let Ok(mut child) = session.child.lock() else {
                    return;
                };
                match child.try_wait() {
                    Ok(Some(status)) => Some(status),
                    Ok(None) => None,
                    Err(_) => return,
                }
            };
            if let Some(status) = status {
                // `close` may have already reported the exit; report once only.
                if !session.closed.swap(true, Ordering::AcqRel) {
                    publish(
                        &sink,
                        TerminalEvent::Exited {
                            terminal_id,
                            exit_code: Some(status.exit_code()),
                            reason: ExitReason::Exited,
                        },
                    );
                }
                return;
            }
            thread::sleep(REAP_POLL_INTERVAL);
        });
    }
}

/// Strips the Windows verbatim (`\\?\`) prefix from a canonicalized path.
///
/// `std::fs::canonicalize` returns extended-length paths on Windows, which many
/// programs reject (`cmd.exe` treats them as UNC paths and falls back to the
/// Windows directory). Shells are handed a normal path instead.
#[cfg(windows)]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    let text = path.to_string_lossy().into_owned();
    if let Some(stripped) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{stripped}"));
    }
    if let Some(stripped) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(stripped);
    }
    path
}

#[cfg(not(windows))]
fn strip_verbatim_prefix(path: PathBuf) -> PathBuf {
    path
}

/// Chooses the shell program, defaulting to the platform shell.
fn resolve_program(program: Option<&str>, args: &[String]) -> (String, Vec<String>) {
    match program {
        Some(program) if !program.trim().is_empty() => (program.to_owned(), args.to_vec()),
        _ => (default_shell().to_owned(), args.to_vec()),
    }
}

/// Interactive shell used when the client does not name one.
pub fn default_shell() -> &'static str {
    if cfg!(windows) {
        "powershell.exe"
    } else {
        "/bin/sh"
    }
}

#[cfg(windows)]
fn apply_platform_environment(command: &mut CommandBuilder) {
    // ConPTY does not inherit a usable console code page, so ask the shell for
    // UTF-8 output instead of the local ANSI code page.
    command.env("TERM", "xterm-256color");
    command.env("ConEmuANSI", "ON");
    command.env("WT_SESSION", "cogitoai");
}

#[cfg(not(windows))]
fn apply_platform_environment(command: &mut CommandBuilder) {
    command.env("TERM", "xterm-256color");
    command.env("LANG", "C.UTF-8");
}

/// Splits text into chunks of at most `max_bytes`, never cutting a character in
/// half so multi-byte output stays intact.
fn split_on_char_boundaries(text: &str, max_bytes: usize) -> Vec<String> {
    if text.len() <= max_bytes {
        return vec![text.to_owned()];
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    for (index, _) in text.char_indices() {
        if index - start >= max_bytes {
            chunks.push(text[start..index].to_owned());
            start = index;
        }
    }
    if start < text.len() {
        chunks.push(text[start..].to_owned());
    }
    chunks
}

/// Splits a pending byte buffer into complete UTF-8 text, leaving any trailing
/// partial sequence buffered for the next read.
fn take_complete_utf8(pending: &mut Vec<u8>) -> String {
    match std::str::from_utf8(pending) {
        Ok(text) => {
            let owned = text.to_owned();
            pending.clear();
            owned
        }
        Err(error) => {
            let valid = error.valid_up_to();
            let text = String::from_utf8_lossy(&pending[..valid]).into_owned();
            pending.drain(..valid);
            text
        }
    }
}
