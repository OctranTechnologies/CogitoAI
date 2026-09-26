use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use harness_pty::{
    default_shell, ExitReason, PtyError, PtyManager, PtyRequest, SessionOrigin, TerminalEvent,
    TerminalSink,
};

fn recorder() -> (Arc<Mutex<Vec<TerminalEvent>>>, Arc<dyn TerminalSink>) {
    let events: Arc<Mutex<Vec<TerminalEvent>>> = Arc::new(Mutex::new(Vec::new()));
    let sink_events = Arc::clone(&events);
    let sink: Arc<dyn TerminalSink> = Arc::new(move |event: TerminalEvent| {
        sink_events.lock().unwrap().push(event);
    });
    (events, sink)
}

fn workspace() -> tempfile::TempDir {
    let temporary = tempfile::tempdir().unwrap();
    std::fs::write(temporary.path().join("file.txt"), "workspace\n").unwrap();
    temporary
}

/// Shell used by the interaction tests.
///
/// The product default on Windows is PowerShell, whose line editor repaints
/// indefinitely unless a terminal emulator tracks its cursor exactly. That
/// emulation belongs to the frontend (xterm.js does it) and is asserted
/// separately by [`the_default_shell_queries_the_terminal_for_its_cursor`].
/// These tests cover PTY mechanics, so they use a shell whose prompt behaviour
/// does not depend on cursor tracking.
fn test_shell() -> Option<String> {
    if cfg!(windows) {
        Some("cmd.exe".to_owned())
    } else {
        None
    }
}

/// The Enter key as a terminal actually sends it.
///
/// A PTY receives a carriage return for Enter, not a line feed. Sending only
/// `\n` makes the shell echo the typed line without ever running it, so every
/// test that expects real output must use this.
const ENTER: &str = "\r\n";

/// A command that runs long enough that Ctrl+C has something to interrupt, and
/// long enough to outlive the test if the interrupt is lost.
fn long_running_command() -> &'static str {
    if cfg!(windows) {
        "ping -n 120 127.0.0.1"
    } else {
        "sleep 120"
    }
}

/// A command that writes to both standard output and standard error.
fn both_streams_command() -> &'static str {
    if cfg!(windows) {
        "echo to-stdout & echo to-stderr 1>&2"
    } else {
        "echo to-stdout; echo to-stderr 1>&2"
    }
}

/// A one-shot program invocation that prints `marker`.
fn echo_marker_command(marker: &str) -> (Option<String>, Vec<String>) {
    if cfg!(windows) {
        (
            Some("cmd.exe".to_owned()),
            vec!["/c".to_owned(), format!("chcp 65001>nul & echo {marker}")],
        )
    } else {
        (
            Some("/bin/sh".to_owned()),
            vec!["-c".to_owned(), format!("echo {marker}")],
        )
    }
}

/// Removes ANSI escape sequences so output can be compared as plain lines.
fn strip_ansi(text: &str) -> String {
    let mut result = String::with_capacity(text.len());
    let mut characters = text.chars().peekable();
    while let Some(character) = characters.next() {
        if character != '\u{1b}' {
            result.push(character);
            continue;
        }
        // CSI: ESC [ ... final-byte
        if characters.peek() == Some(&'[') {
            characters.next();
            for next in characters.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&next) {
                    break;
                }
            }
            continue;
        }
        // OSC: ESC ] ... BEL or ST
        if characters.peek() == Some(&']') {
            characters.next();
            for next in characters.by_ref() {
                if next == '\u{7}' {
                    break;
                }
            }
            continue;
        }
        characters.next();
    }
    result
}

/// Counts how often `marker` appears in the terminal output.
///
/// A shell echoes the line it is sent, so finding the text of a command is not
/// proof it ran: `echo AAA` contains `AAA` once as a typed echo and a second
/// time as output. Requiring two occurrences distinguishes real execution from
/// the echo, which is why these tests do not try to reconstruct the screen (a
/// real terminal emulator, xterm.js, does that in the frontend).
fn marker_count(output: &str, marker: &str) -> usize {
    strip_ansi(output).matches(marker).count()
}

/// Asserts a typed command actually executed: echoed once, plus its own output.
#[track_caller]
fn assert_command_ran(output: &str, marker: &str) {
    assert!(
        marker_count(output, marker) >= 2,
        "command did not execute (marker {marker:?} appeared {} time(s)); got {:?}",
        marker_count(output, marker),
        strip_ansi(output)
    );
}

fn output_so_far(events: &Arc<Mutex<Vec<TerminalEvent>>>, terminal_id: &str) -> String {
    events
        .lock()
        .unwrap()
        .iter()
        .filter_map(|event| match event {
            TerminalEvent::Output {
                terminal_id: id,
                data,
            } if id == terminal_id => Some(data.clone()),
            _ => None,
        })
        .collect()
}

/// Cursor-position report request.
///
/// On Windows the console host inside the PTY emits this during startup and
/// blocks until a terminal answers. Answering is a *terminal emulator*
/// responsibility, satisfied in the product by rendering with xterm.js; the
/// runtime never synthesises a reply. Tests answer it so a shell can start.
const DSR_REQUEST: &str = "\u{1b}[6n";

/// Finds the most recent absolute cursor-position (CUP) the shell requested.
fn latest_cursor_position(text: &str) -> (u16, u16) {
    let bytes = text.as_bytes();
    let mut position = (1_u16, 1_u16);
    let mut index = 0;
    while index + 1 < bytes.len() {
        if bytes[index] != 0x1b || bytes[index + 1] != b'[' {
            index += 1;
            continue;
        }
        let mut end = index + 2;
        while end < bytes.len() && !(0x40..=0x7e).contains(&bytes[end]) {
            end += 1;
        }
        if end < bytes.len() && bytes[end] == b'H' {
            let mut parts = text[index + 2..end].split(';');
            let row = parts
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);
            let column = parts
                .next()
                .and_then(|value| value.parse().ok())
                .unwrap_or(1);
            position = (row.max(1), column.max(1));
        }
        index = end + 1;
    }
    position
}

/// Emulates just enough of a terminal for a shell to become interactive.
struct TerminalClient {
    manager: PtyManager,
    id: String,
    output: String,
    answered: usize,
}

impl TerminalClient {
    fn open(manager: PtyManager, workspace: &std::path::Path) -> Self {
        let request = PtyRequest {
            program: test_shell(),
            ..PtyRequest::human_shell(workspace)
        };
        let id = manager.open(request).expect("open terminal").id;
        Self {
            manager,
            id,
            output: String::new(),
            answered: 0,
        }
    }

    /// Streams new output and answers any outstanding cursor-position query.
    fn pump(&mut self, events: &Arc<Mutex<Vec<TerminalEvent>>>) {
        self.output.push_str(&output_so_far(events, &self.id));
        let requested = self.output.matches(DSR_REQUEST).count();
        while self.answered < requested {
            self.answered += 1;
            let (row, column) = latest_cursor_position(&self.output);
            let _ = self
                .manager
                .write(&self.id, &format!("\u{1b}[{row};{column}R"));
        }
    }

    /// Types raw input, such as a control character.
    fn write_raw(&self, data: &str) {
        self.manager
            .write(&self.id, data)
            .expect("write to terminal");
    }

    /// Types a command and presses Enter.
    fn run(&self, command: &str) {
        self.write_raw(&format!("{command}{ENTER}"));
    }

    /// Waits until `predicate` holds over the accumulated output.
    fn wait_for(
        &mut self,
        events: &Arc<Mutex<Vec<TerminalEvent>>>,
        predicate: impl Fn(&str) -> bool,
    ) -> String {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.pump(events);
            if predicate(&self.output) {
                return self.output.clone();
            }
            assert!(
                Instant::now() < deadline,
                "timed out; output so far: {:?}",
                strip_ansi(&self.output)
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Waits for the session to end, continuing to service cursor queries so a
    /// shell blocked on one can still finish exiting.
    fn wait_for_exit(&mut self, events: &Arc<Mutex<Vec<TerminalEvent>>>) -> Option<u32> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            self.pump(events);
            if let Some(exit_code) = observed_exit(events, &self.id) {
                return exit_code;
            }
            assert!(Instant::now() < deadline, "terminal never reported an exit");
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn close(&self) {
        self.manager.close(&self.id).expect("close terminal");
    }
}

/// The exit code of a session, or `None` if it has not exited yet.
fn observed_exit(
    events: &Arc<Mutex<Vec<TerminalEvent>>>,
    terminal_id: &str,
) -> Option<Option<u32>> {
    events.lock().unwrap().iter().find_map(|event| match event {
        TerminalEvent::Exited {
            terminal_id: id,
            exit_code,
            ..
        } if id == terminal_id => Some(*exit_code),
        _ => None,
    })
}

#[test]
fn opens_a_human_shell_and_streams_interactive_output() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    let request = PtyRequest {
        program: test_shell(),
        ..PtyRequest::human_shell(workspace.path())
    };
    let info = manager.open(request).expect("open terminal");
    assert_eq!(info.origin, "human");
    assert!(info.pid.is_some());
    assert_eq!(manager.list().len(), 1);

    let mut client = TerminalClient {
        manager,
        id: info.id.clone(),
        output: String::new(),
        answered: 0,
    };
    // Wait for the banner so the shell has reached a prompt.
    client.wait_for(&events, |text| !text.is_empty());

    client.run("echo hello-from-pty");
    // The marker must appear twice: once as the shell's echo of the typed line
    // and once as the command's own output, proving it really ran.
    let output = client.wait_for(&events, |text| marker_count(text, "hello-from-pty") >= 2);
    assert_command_ran(&output, "hello-from-pty");
    client.close();
}

#[test]
fn streams_unicode_output_without_corrupting_multibyte_characters() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    // A long run of multi-byte characters forces splitting across read buffers.
    let marker = "héllo-wörld-→-日本語-Ωμέγα".repeat(12);
    let (program, args) = echo_marker_command(&marker);
    let request = PtyRequest {
        program,
        args,
        ..PtyRequest::human_shell(workspace.path())
    };
    let info = manager.open(request).expect("open terminal");

    let mut client = TerminalClient {
        manager,
        id: info.id.clone(),
        output: String::new(),
        answered: 0,
    };
    client.wait_for_exit(&events);

    let output = &client.output;
    assert!(
        output.contains("héllo-wörld") && output.contains("日本語"),
        "unicode output missing: {:?}",
        strip_ansi(output)
    );
    // Byte-wise decoding would insert U+FFFD wherever a character was split
    // across two reads.
    assert!(
        !output.contains('\u{FFFD}'),
        "unicode was corrupted by chunk-boundary decoding: {:?}",
        strip_ansi(output)
    );
}

#[test]
fn resizing_a_session_updates_the_reported_geometry() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (_events, sink) = recorder();
    manager.set_sink(sink);

    let info = manager
        .open(PtyRequest::human_shell(workspace.path()))
        .unwrap();
    assert_eq!((info.cols, info.rows), (80, 24));

    let resized = manager.resize(&info.id, 132, 43).expect("resize terminal");

    assert_eq!((resized.cols, resized.rows), (132, 43));
    assert_eq!(manager.info(&info.id).unwrap().cols, 132);
    assert_eq!(manager.info(&info.id).unwrap().rows, 43);
    assert!(matches!(
        manager.resize(&info.id, 0, 10),
        Err(PtyError::InvalidSize)
    ));

    manager.close(&info.id).unwrap();
}

#[test]
fn a_resized_shell_keeps_working_at_the_new_size() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    let mut client = TerminalClient::open(manager, workspace.path());
    client.wait_for(&events, |text| !text.is_empty());

    client.manager.resize(&client.id, 100, 30).unwrap();
    client.run("echo resized-ok");
    let output = client.wait_for(&events, |text| marker_count(text, "resized-ok") >= 2);

    assert_command_ran(&output, "resized-ok");
    client.close();
}

#[test]
fn ctrl_c_interrupts_a_running_command_instead_of_terminating_the_shell() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    let mut client = TerminalClient::open(manager, workspace.path());
    client.wait_for(&events, |text| !text.is_empty());

    client.run(long_running_command());
    // The echoed command line proves it was submitted.
    client.wait_for(&events, |text| {
        text.contains("ping") || text.contains("sleep")
    });

    // ETX (0x03) is what a real keyboard Ctrl+C produces.
    client.write_raw("\u{3}");

    // If the interrupt was lost, the shell is still busy and this command would
    // be buffered rather than run, so the test would time out.
    client.run("echo survived-ctrl-c");
    let output = client.wait_for(&events, |text| marker_count(text, "survived-ctrl-c") >= 2);
    assert_command_ran(&output, "survived-ctrl-c");
    client.close();
}

#[test]
fn exiting_the_shell_reports_a_single_exit_event() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    let mut client = TerminalClient::open(manager, workspace.path());
    client.wait_for(&events, |text| !text.is_empty());

    client.run("exit");
    assert_eq!(client.wait_for_exit(&events), Some(0));

    // The exit is reported once; a late reader EOF must not report a second.
    std::thread::sleep(Duration::from_millis(500));
    let exits = events
        .lock()
        .unwrap()
        .iter()
        .filter(|event| {
            matches!(event, TerminalEvent::Exited { terminal_id, .. } if terminal_id == &client.id)
        })
        .count();
    assert_eq!(exits, 1, "exit reported more than once");
}

#[test]
fn closing_a_session_terminates_the_process() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    let info = manager
        .open(PtyRequest::human_shell(workspace.path()))
        .unwrap();
    let pid = info.pid.expect("pid");

    manager.close(&info.id).expect("close terminal");

    assert_eq!(
        observed_exit(&events, &info.id).flatten(),
        None,
        "close reports no exit code"
    );
    assert!(manager.list().is_empty(), "closed session still listed");
    assert!(matches!(
        manager.write(&info.id, "echo too late\n"),
        Err(PtyError::NotFound { .. })
    ));
    assert!(!process_is_alive(pid), "process {pid} outlived close");
}

#[test]
fn close_all_terminates_every_live_session() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (_events, sink) = recorder();
    manager.set_sink(sink);

    let first = manager
        .open(PtyRequest::human_shell(workspace.path()))
        .unwrap();
    let second = manager
        .open(PtyRequest::human_shell(workspace.path()))
        .unwrap();
    assert_eq!(manager.list().len(), 2);

    manager.close_all();

    assert!(manager.list().is_empty());
    assert!(!process_is_alive(first.pid.unwrap()));
    assert!(!process_is_alive(second.pid.unwrap()));
}

#[test]
fn refuses_working_directories_outside_the_workspace() {
    let workspace = workspace();
    let outside = tempfile::tempdir().unwrap();
    let manager = PtyManager::new(workspace.path());
    let (_events, sink) = recorder();
    manager.set_sink(sink);

    let error = manager
        .open(PtyRequest::human_shell(outside.path()))
        .unwrap_err();
    assert!(
        matches!(error, PtyError::OutsideWorkspace { .. }),
        "expected OutsideWorkspace, got {error:?}"
    );
    assert_eq!(error.code(), "outside_workspace");
    assert!(manager.list().is_empty());
}

#[test]
fn rejects_invalid_sizes_and_unknown_sessions() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (_events, sink) = recorder();
    manager.set_sink(sink);

    let mut request = PtyRequest::human_shell(workspace.path());
    request.cols = 0;
    assert!(matches!(manager.open(request), Err(PtyError::InvalidSize)));

    assert!(matches!(
        manager.write("pty-missing", "echo hi\n"),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.resize("pty-missing", 80, 24),
        Err(PtyError::NotFound { .. })
    ));
    assert!(matches!(
        manager.close("pty-missing"),
        Err(PtyError::NotFound { .. })
    ));
}

#[test]
fn runs_an_explicit_program_when_one_is_requested() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    let (program, args) = if cfg!(windows) {
        (
            "cmd.exe",
            vec!["/c".to_owned(), "echo explicit-program".to_owned()],
        )
    } else {
        (
            "/bin/sh",
            vec!["-c".to_owned(), "echo explicit-program".to_owned()],
        )
    };
    let request = PtyRequest {
        program: Some(program.to_owned()),
        args,
        ..PtyRequest::human_shell(workspace.path())
    };

    let info = manager.open(request).expect("open terminal");
    assert_eq!(info.program, program);
    let mut client = TerminalClient {
        manager,
        id: info.id.clone(),
        output: String::new(),
        answered: 0,
    };
    client.wait_for_exit(&events);
    assert!(
        marker_count(&client.output, "explicit-program") >= 1,
        "got {:?}",
        strip_ansi(&client.output)
    );
}

#[test]
fn reports_the_working_directory_and_default_shell() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (_events, sink) = recorder();
    manager.set_sink(sink);

    let info = manager
        .open(PtyRequest::human_shell(workspace.path()))
        .unwrap();

    assert_eq!(
        info.working_directory,
        expected_working_directory(workspace.path())
    );
    assert_eq!(info.program, default_shell());
    manager.close(&info.id).unwrap();
}

#[test]
fn human_is_the_only_constructible_origin() {
    // The agent path is intentionally unreachable: `SessionOrigin` has a single
    // variant, so no model-driven call can name another origin. Adding an agent
    // origin would be a deliberate, visible change to this enum.
    assert_eq!(SessionOrigin::Human.as_str(), "human");
    assert_eq!(
        serde_json::to_string(&SessionOrigin::Human).unwrap(),
        "\"human\""
    );
}

#[test]
fn exit_reasons_are_distinguishable() {
    // The frontend uses the reason to explain *why* a terminal ended, so the two
    // causes must not collapse into one value.
    assert_ne!(ExitReason::Exited, ExitReason::Closed);
    assert_eq!(
        serde_json::to_string(&ExitReason::Closed).unwrap(),
        "\"closed\""
    );
    assert_eq!(
        serde_json::to_string(&ExitReason::Exited).unwrap(),
        "\"exited\""
    );
}

#[test]
fn stdout_and_stderr_arrive_on_one_interleaved_stream() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    let mut client = TerminalClient::open(manager, workspace.path());
    client.wait_for(&events, |text| !text.is_empty());

    client.run(both_streams_command());
    // A PTY merges both streams into one, so the frontend receives a single
    // ordered stream rather than separate stdout/stderr channels.
    let output = client.wait_for(&events, |text| {
        marker_count(text, "to-stdout") >= 2 && marker_count(text, "to-stderr") >= 2
    });

    assert_command_ran(&output, "to-stdout");
    assert_command_ran(&output, "to-stderr");
    client.close();
}

/// The default shell needs a real terminal emulator in the frontend.
///
/// On Windows the console host queries the terminal for the cursor position
/// during startup, and shells that repaint (PowerShell) do so continuously. This
/// documents why the desktop renders terminals with xterm.js rather than
/// printing output to a log, and confirms the runtime passes the query through
/// to the client instead of answering it on the shell's behalf.
#[test]
#[cfg(windows)]
fn the_default_shell_queries_the_terminal_for_its_cursor() {
    let workspace = workspace();
    let manager = PtyManager::new(workspace.path());
    let (events, sink) = recorder();
    manager.set_sink(sink);

    assert_eq!(default_shell(), "powershell.exe");
    let info = manager
        .open(PtyRequest::human_shell(workspace.path()))
        .unwrap();
    let mut client = TerminalClient {
        manager,
        id: info.id.clone(),
        output: String::new(),
        answered: 0,
    };

    client.wait_for(&events, |text| text.contains(DSR_REQUEST));
    client.close();
}

/// The working directory the runtime should report for a workspace.
///
/// Windows canonicalization produces a `\\?\` verbatim path, which shells reject,
/// so the runtime strips that prefix before handing a directory to a process.
#[cfg(windows)]
fn expected_working_directory(path: &std::path::Path) -> String {
    let canonical = std::fs::canonicalize(path).unwrap();
    canonical
        .to_string_lossy()
        .strip_prefix(r"\\?\")
        .unwrap_or(&canonical.to_string_lossy())
        .to_owned()
}

#[cfg(not(windows))]
fn expected_working_directory(path: &std::path::Path) -> String {
    std::fs::canonicalize(path)
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
fn process_is_alive(pid: u32) -> bool {
    let output = std::process::Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH"])
        .output();
    match output {
        Ok(output) => !String::from_utf8_lossy(&output.stdout).contains("INFO: No tasks"),
        Err(_) => false,
    }
}

#[cfg(not(windows))]
fn process_is_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}
