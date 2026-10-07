use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

const READ_BUFFER_BYTES: usize = 8_192;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;
const MAX_BACKGROUND_PROCESSES: usize = 16;
const MAX_BACKGROUND_RECORDS: usize = 64;
const MAX_BACKGROUND_LOG_BYTES: usize = 256 * 1024;
const PROCESS_MONITOR_INTERVAL: Duration = Duration::from_millis(40);

#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
    parents: Vec<Arc<AtomicBool>>,
}

impl Default for CancellationToken {
    fn default() -> Self {
        Self::new()
    }
}

impl CancellationToken {
    pub fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            parents: Vec::new(),
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
            || self
                .parents
                .iter()
                .any(|parent| parent.load(Ordering::Acquire))
    }

    /// Creates a cancellation scope that inherits parent cancellation while
    /// allowing an individual child to be stopped without cancelling siblings.
    pub fn child_token(&self) -> Self {
        let mut parents = self.parents.clone();
        parents.push(Arc::clone(&self.cancelled));
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
            parents,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessRequest {
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub timeout: Duration,
    pub max_output_bytes: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessResult {
    pub exit_code: Option<i32>,
    pub success: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProcessEvent {
    Started { pid: Option<u32> },
    Stdout { chunk: String },
    Stderr { chunk: String },
    Exited { result: ProcessResult },
}

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("could not start process: {message}")]
    Spawn { message: String },
    #[error("could not capture process output: {message}")]
    Pipe { message: String },
    #[error("process output consumer failed")]
    Consumer,
}

pub trait ProcessRunner: Send + Sync {
    fn execute(
        &self,
        request: ProcessRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProcessEvent) -> Result<(), ProcessError>,
    ) -> Result<ProcessResult, ProcessError>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ReaderEvent {
    Stdout(String),
    Stderr(String),
}

pub struct LocalProcessRunner;

impl ProcessRunner for LocalProcessRunner {
    fn execute(
        &self,
        request: ProcessRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProcessEvent) -> Result<(), ProcessError>,
    ) -> Result<ProcessResult, ProcessError> {
        let started_at = Instant::now();
        let mut command = Command::new(&request.program);
        command
            .args(&request.args)
            .current_dir(&request.working_directory)
            .env_clear()
            .envs(crate::environment::filtered_process_environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn().map_err(|error| ProcessError::Spawn {
            message: error.to_string(),
        })?;
        let pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| ProcessError::Pipe {
            message: "stdout unavailable".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ProcessError::Pipe {
            message: "stderr unavailable".to_owned(),
        })?;
        let capture_limit = request.max_output_bytes.min(MAX_CAPTURE_BYTES);
        let captured = Arc::new(AtomicUsize::new(0));
        let (sender, receiver) = mpsc::sync_channel(32);
        let stdout_sender = sender.clone();
        let stdout_counter = Arc::clone(&captured);
        let stdout_thread = std::thread::spawn(move || {
            read_pipe(stdout, stdout_sender, stdout_counter, capture_limit, true)
        });
        let stderr_sender = sender;
        let stderr_counter = Arc::clone(&captured);
        let stderr_thread = std::thread::spawn(move || {
            read_pipe(stderr, stderr_sender, stderr_counter, capture_limit, false)
        });
        let child = Arc::new(Mutex::new(child));
        let mut stdout_output = String::new();
        let mut stderr_output = String::new();
        let mut exit_status = None;
        let mut timed_out = false;
        let mut cancelled = false;
        let mut consumer_failed = false;
        let mut terminate = false;
        if let Err(error) = on_event(ProcessEvent::Started { pid: Some(pid) }) {
            consumer_failed = true;
            terminate = true;
            let _ = error;
        }
        while !consumer_failed {
            match receiver.recv_timeout(Duration::from_millis(10)) {
                Ok(ReaderEvent::Stdout(chunk)) => {
                    stdout_output.push_str(&chunk);
                    if on_event(ProcessEvent::Stdout { chunk }).is_err() {
                        consumer_failed = true;
                        terminate = true;
                        break;
                    }
                }
                Ok(ReaderEvent::Stderr(chunk)) => {
                    stderr_output.push_str(&chunk);
                    if on_event(ProcessEvent::Stderr { chunk }).is_err() {
                        consumer_failed = true;
                        terminate = true;
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            if exit_status.is_none() {
                if let Ok(Some(status)) =
                    child.lock().map_err(|_| ProcessError::Consumer)?.try_wait()
                {
                    exit_status = Some(status);
                }
            }
            if cancellation.is_cancelled() {
                cancelled = true;
                terminate = true;
                break;
            }
            if started_at.elapsed() >= request.timeout {
                timed_out = true;
                terminate = true;
                break;
            }
        }
        if terminate {
            terminate_process_tree(&child);
        }
        if exit_status.is_none() {
            exit_status = child
                .lock()
                .map_err(|_| ProcessError::Consumer)?
                .wait()
                .ok();
        }
        let _ = stdout_thread.join();
        let _ = stderr_thread.join();
        while let Ok(event) = receiver.try_recv() {
            match event {
                ReaderEvent::Stdout(chunk) => stdout_output.push_str(&chunk),
                ReaderEvent::Stderr(chunk) => stderr_output.push_str(&chunk),
            }
        }
        if consumer_failed {
            return Err(ProcessError::Consumer);
        }
        let exit_code = exit_status.and_then(|status| status.code());
        let result = ProcessResult {
            exit_code,
            success: exit_code == Some(0) && !timed_out && !cancelled,
            timed_out,
            cancelled,
            stdout: stdout_output,
            stderr: stderr_output,
            duration_ms: started_at
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        };
        on_event(ProcessEvent::Exited {
            result: result.clone(),
        })?;
        Ok(result)
    }
}

/// Lifecycle state of a persistent local command.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundProcessStatus {
    Running,
    Stopping,
    Exited,
    Failed,
    Stopped,
}

impl BackgroundProcessStatus {
    pub fn is_running(self) -> bool {
        matches!(self, Self::Running | Self::Stopping)
    }
}

impl std::fmt::Display for BackgroundProcessStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Exited => "exited",
            Self::Failed => "failed",
            Self::Stopped => "stopped",
        })
    }
}

/// Safe, user-visible metadata for one managed command.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ManagedProcessInfo {
    pub id: String,
    pub pid: u32,
    pub command: String,
    pub working_directory: String,
    pub started_at_unix_ms: u64,
    pub status: BackgroundProcessStatus,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ProcessLogBatch {
    pub text: String,
    pub next_cursor: u64,
    pub truncated: bool,
    pub status: BackgroundProcessStatus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProcessWaitResult {
    pub process: ManagedProcessInfo,
    pub ready: bool,
    pub cancelled: bool,
}

#[derive(Clone, Debug)]
struct LogRecord {
    cursor: u64,
    stream: &'static str,
    text: String,
    bytes: usize,
}

#[derive(Debug)]
struct BoundedProcessLogs {
    records: VecDeque<LogRecord>,
    bytes: usize,
    next_cursor: u64,
    truncated: bool,
}

impl Default for BoundedProcessLogs {
    fn default() -> Self {
        Self {
            records: VecDeque::new(),
            bytes: 0,
            next_cursor: 1,
            truncated: false,
        }
    }
}

impl BoundedProcessLogs {
    fn push(&mut self, stream: &'static str, text: String) {
        if text.is_empty() {
            return;
        }
        let mut text = text;
        while self.bytes.saturating_add(text.len()) > MAX_BACKGROUND_LOG_BYTES
            || self.records.len() >= MAX_BACKGROUND_RECORDS
        {
            let Some(removed) = self.records.pop_front() else {
                break;
            };
            self.bytes = self.bytes.saturating_sub(removed.bytes);
            self.truncated = true;
        }
        if text.len() > MAX_BACKGROUND_LOG_BYTES {
            let mut start = text.len() - MAX_BACKGROUND_LOG_BYTES;
            while !text.is_char_boundary(start) {
                start += 1;
            }
            text = text[start..].to_owned();
            self.records.clear();
            self.bytes = 0;
            self.truncated = true;
        }
        let bytes = text.len();
        self.records.push_back(LogRecord {
            cursor: self.next_cursor,
            stream,
            text,
            bytes,
        });
        self.next_cursor = self.next_cursor.saturating_add(1);
        self.bytes += bytes;
    }

    fn read(
        &self,
        after_cursor: u64,
        max_bytes: usize,
        status: BackgroundProcessStatus,
    ) -> ProcessLogBatch {
        let first = self.records.front().map(|record| record.cursor);
        let mut truncated = self.truncated && first.is_some_and(|cursor| after_cursor < cursor);
        let mut text = String::new();
        let mut next_cursor = after_cursor;
        for record in self
            .records
            .iter()
            .filter(|record| record.cursor > after_cursor)
        {
            let row = format!("[{}] {}", record.stream, record.text);
            let remaining = max_bytes.saturating_sub(text.len());
            if remaining == 0 {
                truncated = true;
                break;
            }
            if row.len() > remaining {
                let mut end = remaining;
                while !row.is_char_boundary(end) {
                    end -= 1;
                }
                text.push_str(&row[..end]);
                next_cursor = record.cursor;
                truncated = true;
                break;
            }
            text.push_str(&row);
            if !row.ends_with('\n') {
                text.push('\n');
            }
            next_cursor = record.cursor;
        }
        ProcessLogBatch {
            text,
            next_cursor,
            truncated,
            status,
        }
    }
}

struct ManagedProcess {
    info: Mutex<ManagedProcessInfo>,
    child: Arc<Mutex<Child>>,
    logs: Mutex<BoundedProcessLogs>,
    stop_requested: AtomicBool,
    status_sink: Option<ProcessStatusSink>,
}

pub type ProcessStatusSink = Arc<dyn Fn(ManagedProcessInfo) + Send + Sync>;

pub struct ProcessStartRequest {
    pub display_command: String,
    pub program: String,
    pub args: Vec<String>,
    pub working_directory: PathBuf,
    pub timeout: Option<Duration>,
    pub cancellation: CancellationToken,
    pub status_sink: Option<ProcessStatusSink>,
}

struct ProcessManagerInner {
    processes: Mutex<HashMap<String, Arc<ManagedProcess>>>,
    next_id: AtomicUsize,
}

impl Drop for ProcessManagerInner {
    fn drop(&mut self) {
        let processes = self
            .processes
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        for process in processes.values() {
            let running = process
                .info
                .lock()
                .map(|info| info.status.is_running())
                .unwrap_or(true);
            if running {
                process.stop_requested.store(true, Ordering::Release);
                terminate_process_tree(&process.child);
                if let Ok(mut child) = process.child.lock() {
                    let status = child.wait().ok();
                    if let Ok(mut info) = process.info.lock() {
                        info.status = BackgroundProcessStatus::Stopped;
                        info.exit_code = status.and_then(|status| status.code());
                        if let Some(sink) = &process.status_sink {
                            sink(info.clone());
                        }
                    }
                }
            }
        }
    }
}

/// Runtime-owned manager for persistent background processes. Process count
/// and captured logs are bounded; dropping the manager terminates live trees.
#[derive(Clone)]
pub struct ProcessManager {
    inner: Arc<ProcessManagerInner>,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self {
            inner: Arc::new(ProcessManagerInner {
                processes: Mutex::new(HashMap::new()),
                next_id: AtomicUsize::new(0),
            }),
        }
    }
}

impl ProcessManager {
    pub fn start(&self, request: ProcessStartRequest) -> Result<ManagedProcessInfo, ProcessError> {
        let ProcessStartRequest {
            display_command,
            program,
            args,
            working_directory,
            timeout,
            cancellation,
            status_sink,
        } = request;
        let timeout = timeout.map(|value| value.min(Duration::from_secs(24 * 60 * 60)));
        let mut processes = self
            .inner
            .processes
            .lock()
            .map_err(|_| ProcessError::Consumer)?;
        let active = processes
            .values()
            .filter(|process| {
                process
                    .info
                    .lock()
                    .is_ok_and(|info| info.status.is_running())
            })
            .count();
        if active >= MAX_BACKGROUND_PROCESSES {
            return Err(ProcessError::Spawn {
                message: format!("background process limit ({MAX_BACKGROUND_PROCESSES}) reached"),
            });
        }
        if processes.len() >= MAX_BACKGROUND_RECORDS {
            let oldest_finished = processes
                .iter()
                .filter_map(|(id, process)| {
                    process
                        .info
                        .lock()
                        .ok()
                        .filter(|info| !info.status.is_running())
                        .map(|info| (id.clone(), info.started_at_unix_ms))
                })
                .min_by_key(|(_, started)| *started)
                .map(|(id, _)| id);
            if let Some(id) = oldest_finished {
                processes.remove(&id);
            }
        }
        let mut command = Command::new(program);
        command
            .args(&args)
            .current_dir(&working_directory)
            .env_clear()
            .envs(crate::environment::filtered_process_environment())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_process_group(&mut command);
        let mut child = command.spawn().map_err(|error| ProcessError::Spawn {
            message: error.to_string(),
        })?;
        let pid = child.id();
        let stdout = child.stdout.take().ok_or_else(|| ProcessError::Pipe {
            message: "background stdout unavailable".to_owned(),
        })?;
        let stderr = child.stderr.take().ok_or_else(|| ProcessError::Pipe {
            message: "background stderr unavailable".to_owned(),
        })?;
        let child = Arc::new(Mutex::new(child));
        let id = format!(
            "proc-{pid}-{}",
            self.inner.next_id.fetch_add(1, Ordering::Relaxed)
        );
        let started_at_unix_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis()
            .min(u64::MAX as u128) as u64;
        let info = ManagedProcessInfo {
            id: id.clone(),
            pid,
            command: display_command,
            working_directory: working_directory.to_string_lossy().into_owned(),
            started_at_unix_ms,
            status: BackgroundProcessStatus::Running,
            exit_code: None,
            timed_out: false,
        };
        let process = Arc::new(ManagedProcess {
            info: Mutex::new(info.clone()),
            child: Arc::clone(&child),
            logs: Mutex::new(BoundedProcessLogs::default()),
            stop_requested: AtomicBool::new(false),
            status_sink: status_sink.clone(),
        });
        processes.insert(id, Arc::clone(&process));
        drop(processes);

        if let Some(sink) = &status_sink {
            sink(info.clone());
        }

        let stdout_process = Arc::clone(&process);
        if let Err(error) = thread::Builder::new()
            .name(format!("harness-bg-{pid}-stdout"))
            .spawn(move || read_background_pipe(stdout, stdout_process, "stdout"))
        {
            terminate_process_tree(&child);
            return Err(ProcessError::Pipe {
                message: error.to_string(),
            });
        }
        let stderr_process = Arc::clone(&process);
        if let Err(error) = thread::Builder::new()
            .name(format!("harness-bg-{pid}-stderr"))
            .spawn(move || read_background_pipe(stderr, stderr_process, "stderr"))
        {
            terminate_process_tree(&child);
            return Err(ProcessError::Pipe {
                message: error.to_string(),
            });
        }

        let watched = Arc::clone(&process);
        if let Err(error) = thread::Builder::new()
            .name(format!("harness-bg-{pid}-monitor"))
            .spawn(move || {
                let started = Instant::now();
                loop {
                    let cancelled = cancellation.is_cancelled();
                    let timed_out = timeout.is_some_and(|limit| started.elapsed() >= limit);
                    if cancelled || timed_out {
                        watched.stop_requested.store(true, Ordering::Release);
                        terminate_process_tree(&watched.child);
                    }
                    let status = watched
                        .child
                        .lock()
                        .ok()
                        .and_then(|mut child| child.try_wait().ok().flatten());
                    if let Some(status) = status {
                        let updated = if let Ok(mut info) = watched.info.lock() {
                            info.exit_code = status.code();
                            info.timed_out = timed_out;
                            info.status =
                                if cancelled || watched.stop_requested.load(Ordering::Acquire) {
                                    BackgroundProcessStatus::Stopped
                                } else if status.success() {
                                    BackgroundProcessStatus::Exited
                                } else {
                                    BackgroundProcessStatus::Failed
                                };
                            Some(info.clone())
                        } else {
                            None
                        };
                        if let (Some(sink), Some(info)) = (&watched.status_sink, updated) {
                            sink(info);
                        }
                        break;
                    }
                    thread::sleep(PROCESS_MONITOR_INTERVAL);
                }
            })
        {
            terminate_process_tree(&child);
            return Err(ProcessError::Pipe {
                message: error.to_string(),
            });
        }
        Ok(info)
    }

    pub fn list(&self) -> Vec<ManagedProcessInfo> {
        let Ok(processes) = self.inner.processes.lock() else {
            return Vec::new();
        };
        let mut items = processes
            .values()
            .filter_map(|process| process.info.lock().ok().map(|info| info.clone()))
            .collect::<Vec<_>>();
        items.sort_by_key(|process| process.started_at_unix_ms);
        items
    }

    pub fn read_output(
        &self,
        id: &str,
        after_cursor: u64,
        max_bytes: usize,
    ) -> Result<ProcessLogBatch, ProcessError> {
        let process = self.get(id)?;
        let status = process
            .info
            .lock()
            .map_err(|_| ProcessError::Consumer)?
            .status;
        process
            .logs
            .lock()
            .map(|logs| logs.read(after_cursor, max_bytes.min(64 * 1024), status))
            .map_err(|_| ProcessError::Consumer)
    }

    pub fn stop(&self, id: &str) -> Result<ManagedProcessInfo, ProcessError> {
        let process = self.get(id)?;
        let status = process
            .info
            .lock()
            .map_err(|_| ProcessError::Consumer)?
            .status;
        if status.is_running() {
            process.stop_requested.store(true, Ordering::Release);
            terminate_process_tree(&process.child);
            let deadline = Instant::now() + Duration::from_secs(2);
            while Instant::now() < deadline {
                let status = process
                    .info
                    .lock()
                    .map_err(|_| ProcessError::Consumer)?
                    .status;
                if !status.is_running() {
                    break;
                }
                thread::sleep(PROCESS_MONITOR_INTERVAL);
            }
        }
        process
            .info
            .lock()
            .map(|info| info.clone())
            .map_err(|_| ProcessError::Consumer)
    }

    pub fn wait_for_output(
        &self,
        id: &str,
        pattern: &str,
        timeout: Duration,
        cancellation: &CancellationToken,
    ) -> Result<ProcessWaitResult, ProcessError> {
        let process = self.get(id)?;
        let deadline = Instant::now() + timeout.min(Duration::from_secs(60));
        loop {
            let info = process
                .info
                .lock()
                .map_err(|_| ProcessError::Consumer)?
                .clone();
            let logs = process
                .logs
                .lock()
                .map_err(|_| ProcessError::Consumer)?
                .read(0, MAX_BACKGROUND_LOG_BYTES, info.status);
            if logs.text.contains(pattern) {
                return Ok(ProcessWaitResult {
                    process: info,
                    ready: true,
                    cancelled: false,
                });
            }
            if !info.status.is_running()
                || cancellation.is_cancelled()
                || Instant::now() >= deadline
            {
                return Ok(ProcessWaitResult {
                    process: info,
                    ready: false,
                    cancelled: cancellation.is_cancelled(),
                });
            }
            thread::sleep(PROCESS_MONITOR_INTERVAL);
        }
    }

    fn get(&self, id: &str) -> Result<Arc<ManagedProcess>, ProcessError> {
        self.inner
            .processes
            .lock()
            .map_err(|_| ProcessError::Consumer)?
            .get(id)
            .cloned()
            .ok_or_else(|| ProcessError::Spawn {
                message: format!("no background process with handle {id}"),
            })
    }
}

fn read_background_pipe(mut reader: impl Read, process: Arc<ManagedProcess>, stream: &'static str) {
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => {
                let text = String::from_utf8_lossy(&buffer[..count]).into_owned();
                if let Ok(mut logs) = process.logs.lock() {
                    logs.push(stream, text);
                }
            }
        }
    }
}

fn read_pipe<R: Read>(
    mut reader: R,
    sender: SyncSender<ReaderEvent>,
    captured: Arc<AtomicUsize>,
    limit: usize,
    stdout: bool,
) {
    let mut buffer = [0_u8; READ_BUFFER_BYTES];
    loop {
        let count = match reader.read(&mut buffer) {
            Ok(0) | Err(_) => break,
            Ok(count) => count,
        };
        let allowed = reserve_capture(&captured, limit, count);
        if allowed == 0 {
            continue;
        }
        let chunk = String::from_utf8_lossy(&buffer[..allowed]).into_owned();
        let event = if stdout {
            ReaderEvent::Stdout(chunk)
        } else {
            ReaderEvent::Stderr(chunk)
        };
        if sender.send(event).is_err() {
            break;
        }
    }
}

fn reserve_capture(captured: &AtomicUsize, limit: usize, count: usize) -> usize {
    let mut current = captured.load(Ordering::Acquire);
    loop {
        if current >= limit {
            return 0;
        }
        let allowed = count.min(limit - current);
        match captured.compare_exchange(
            current,
            current + allowed,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => return allowed,
            Err(next) => current = next,
        }
    }
}

#[cfg(unix)]
pub(crate) fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
pub(crate) fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
}

#[cfg(not(any(unix, windows)))]
pub(crate) fn configure_process_group(_command: &mut Command) {}

#[cfg(unix)]
fn terminate_process_tree(child: &Arc<Mutex<Child>>) {
    let pid = child.lock().map(|child| child.id()).unwrap_or(0);
    if pid != 0 {
        let _ = Command::new("/bin/kill")
            .args(["-TERM", &format!("-{pid}")])
            .status();
    }
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
    }
}

#[cfg(windows)]
fn terminate_process_tree(child: &Arc<Mutex<Child>>) {
    let pid = child.lock().map(|child| child.id()).unwrap_or(0);
    if pid != 0 {
        let taskkill = std::env::var_os("SystemRoot")
            .map(PathBuf::from)
            .map(|root| root.join("System32").join("taskkill.exe"))
            .unwrap_or_else(|| PathBuf::from("taskkill.exe"));
        let mut command = Command::new(taskkill);
        command
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
        let _ = command.status();
    }
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
    }
}

#[cfg(not(any(unix, windows)))]
fn terminate_process_tree(child: &Arc<Mutex<Child>>) {
    if let Ok(mut child) = child.lock() {
        let _ = child.kill();
    }
}

#[cfg(test)]
mod background_process_tests {
    use super::*;

    #[test]
    fn dropping_the_runtime_manager_terminates_live_processes() {
        let workspace = tempfile::tempdir().unwrap();
        let manager = ProcessManager::default();
        #[cfg(windows)]
        let (program, args) = (
            "cmd",
            vec![
                "/C".to_owned(),
                "echo READY & powershell.exe -NoProfile -NonInteractive -Command Start-Sleep -Seconds 30".to_owned(),
            ],
        );
        #[cfg(not(windows))]
        let (program, args) = ("sh", vec!["-c".to_owned(), "exec sleep 30".to_owned()]);
        let info = manager
            .start(ProcessStartRequest {
                display_command: "test runtime shutdown".to_owned(),
                program: program.to_owned(),
                args,
                working_directory: workspace.path().to_path_buf(),
                timeout: None,
                cancellation: CancellationToken::new(),
                status_sink: None,
            })
            .unwrap();
        let process = manager.get(&info.id).unwrap();
        drop(manager);

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let status = process.info.lock().unwrap().status;
            if status == BackgroundProcessStatus::Stopped {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "process survived runtime manager shutdown"
            );
            thread::sleep(PROCESS_MONITOR_INTERVAL);
        }
    }
}
