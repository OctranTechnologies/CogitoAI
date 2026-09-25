use std::io::Read;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const READ_BUFFER_BYTES: usize = 8_192;
const MAX_CAPTURE_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct CancellationToken {
    cancelled: Arc<AtomicBool>,
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
        }
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
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
fn configure_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_process_group(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(0x0000_0200);
}

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
        let _ = Command::new(taskkill)
            .args(["/PID", &pid.to_string(), "/T", "/F"])
            .status();
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
