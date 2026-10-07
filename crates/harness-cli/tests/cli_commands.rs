use std::fs;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use tempfile::TempDir;

struct TestTempDir(TempDir);

impl TestTempDir {
    fn path(&self) -> &Path {
        self.0.path()
    }
}

impl Drop for TestTempDir {
    fn drop(&mut self) {
        let binary = env!("CARGO_BIN_EXE_harness-cli");
        let _ = Command::new(binary)
            .arg("--workspace")
            .arg(self.path())
            .arg("--session-root")
            .arg(self.path().join("sessions"))
            .args(["--json", "runtime", "stop"])
            .output();
        for _ in 0..40 {
            let output = Command::new(binary)
                .arg("--workspace")
                .arg(self.path())
                .arg("--session-root")
                .arg(self.path().join("sessions"))
                .args(["--json", "runtime", "status"])
                .output();
            if output.as_ref().is_ok_and(|output| {
                String::from_utf8_lossy(&output.stdout).contains("\"status\":\"unavailable\"")
            }) {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

fn tempdir() -> Result<TestTempDir, std::io::Error> {
    tempfile::tempdir().map(TestTempDir)
}

fn cli(root: &Path, arguments: &[&str]) -> Output {
    let session_root = root.join("sessions");
    let root = root.to_string_lossy();
    let session_root = session_root.to_string_lossy();
    let mut command = Command::new(env!("CARGO_BIN_EXE_harness-cli"));
    command
        .args(["--workspace", &root, "--session-root", &session_root])
        .args(arguments);
    command.output().expect("CLI should start")
}

fn cli_child(root: &Path, arguments: &[&str]) -> std::process::Child {
    let session_root = root.join("sessions");
    let root = root.to_string_lossy();
    let session_root = session_root.to_string_lossy();
    Command::new(env!("CARGO_BIN_EXE_harness-cli"))
        .args(["--workspace", &root, "--session-root", &session_root])
        .args(arguments)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("CLI should start")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn tui_refuses_pipes_without_writing_terminal_control_sequences() {
    let temporary = tempdir().unwrap();
    let output = cli(temporary.path(), &["tui"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("needs a terminal"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains('\u{1b}'));
}

#[test]
fn json_tui_refusal_is_reported_as_a_json_error() {
    let temporary = tempdir().unwrap();
    let output = cli(temporary.path(), &["--json", "tui"]);

    assert!(!output.status.success());
    let error: serde_json::Value =
        serde_json::from_slice(&output.stderr).expect("--json errors stay machine-readable");
    assert_eq!(error["type"], "error");
    assert!(error["error"]
        .as_str()
        .unwrap()
        .contains("cannot be combined"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains('\u{1b}'));
}

#[test]
fn piped_plain_run_keeps_the_existing_human_readable_output() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let run = cli(
        root,
        &[
            "--yes",
            "run",
            "create a mock output",
            &root.to_string_lossy(),
        ],
    );

    assert_success(&run);
    let stdout = String::from_utf8_lossy(&run.stdout);
    assert!(
        stdout.contains("[tool] started list_directory"),
        "stdout: {stdout}"
    );
    assert!(stdout.contains("completed session"), "stdout: {stdout}");
    assert!(!stdout.contains('\u{1b}'));
}

#[test]
fn run_accepts_text_attachments_and_preserves_json_output() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let attachment = root.join("error-notes.txt");
    fs::write(&attachment, "Observed error: expected 200, received 500").unwrap();
    let output = cli(
        root,
        &[
            "--json",
            "--yes",
            "run",
            "Investigate the attached error",
            "--attach",
            &attachment.to_string_lossy(),
        ],
    );
    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout
        .lines()
        .all(|line| serde_json::from_str::<serde_json::Value>(line).is_ok()));
    assert!(stdout.contains("completed"));
}

#[test]
fn run_rejects_credential_attachments_before_starting_runtime() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let attachment = root.join(".env.local");
    fs::write(&attachment, "API_KEY=do-not-send").unwrap();
    let output = cli(
        root,
        &[
            "run",
            "inspect configuration",
            "--attach",
            &attachment.to_string_lossy(),
        ],
    );
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("credential and shell-profile files cannot be attached"));
}

#[test]
fn no_subcommand_keeps_the_legacy_workspace_summary() {
    let temporary = tempdir().unwrap();
    let output = cli(temporary.path(), &[]);

    assert_success(&output);
    assert!(String::from_utf8_lossy(&output.stdout).contains("CogitoAI harness workspace:"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains('\u{1b}'));
}

#[test]
fn runtime_status_json_does_not_start_a_missing_runtime() {
    let temporary = tempdir().unwrap();
    let output = cli(temporary.path(), &["--json", "runtime", "status"]);
    assert_success(&output);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["status"], "unavailable");
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Connecting to harness"));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("Starting runtime"));
}

#[test]
fn runtime_is_started_from_cold_and_shared_by_simultaneous_cli_clients() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let offline = cli(root, &["--json", "runtime", "status"]);
    assert_success(&offline);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&offline.stdout).unwrap()["status"],
        "unavailable"
    );

    let run = cli(
        root,
        &[
            "--json",
            "--yes",
            "run",
            "create a mock output",
            &root.to_string_lossy(),
        ],
    );
    assert_success(&run);
    for line in String::from_utf8_lossy(&run.stdout).lines() {
        serde_json::from_str::<serde_json::Value>(line)
            .expect("startup diagnostics must not pollute JSON event output");
    }

    let first = cli_child(root, &["--json", "runtime", "status"]);
    let second = cli_child(root, &["--json", "runtime", "status"]);
    let first = first.wait_with_output().unwrap();
    let second = second.wait_with_output().unwrap();
    assert_success(&first);
    assert_success(&second);
    let first: serde_json::Value = serde_json::from_slice(&first.stdout).unwrap();
    let second: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(first["status"], "connected");
    assert_eq!(second["status"], "connected");
    assert!(first["runtime"]["pid"].is_u64());
    assert_eq!(first["runtime"]["pid"], second["runtime"]["pid"]);
}

#[test]
fn runtime_restart_replaces_the_server_and_shutdown_remains_an_alias() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    let run = cli(
        root,
        &[
            "--json",
            "--yes",
            "run",
            "create a mock output",
            &root.to_string_lossy(),
        ],
    );
    assert_success(&run);
    let before = cli(root, &["--json", "runtime", "status"]);
    assert_success(&before);
    let before: serde_json::Value = serde_json::from_slice(&before.stdout).unwrap();
    let before_pid = before["runtime"]["pid"].as_u64().unwrap();

    let restarted = cli(root, &["--json", "runtime", "restart"]);
    assert_success(&restarted);
    let restarted: serde_json::Value = serde_json::from_slice(&restarted.stdout).unwrap();
    assert_eq!(restarted["status"], "restarted");
    assert_ne!(restarted["runtime"]["pid"].as_u64().unwrap(), before_pid);

    let stopped = cli(root, &["--json", "runtime", "shutdown"]);
    assert_success(&stopped);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&stopped.stdout).unwrap()["status"],
        "stopping"
    );
}

#[cfg(windows)]
#[test]
fn active_cli_run_reconnects_after_runtime_is_stopped_without_replaying_task() {
    use std::io::{BufRead, BufReader, Read};
    use std::thread;

    let temporary = tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join(".agent")).unwrap();
    fs::write(
        root.join(".agent/config.toml"),
        "[commands]\ntest = [\"cmd\", \"/C\", \"ping -n 12 127.0.0.1 > NUL\"]\n",
    )
    .unwrap();

    let mut child = cli_child(
        root,
        &[
            "--log-level",
            "debug",
            "--json",
            "--yes",
            "run",
            "create a mock output",
            &root.to_string_lossy(),
        ],
    );
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let (event_sender, event_receiver) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if event_sender.send(line.unwrap_or_default()).is_err() {
                break;
            }
        }
    });
    let (stderr_sender, stderr_receiver) = mpsc::channel();
    thread::spawn(move || {
        let mut text = String::new();
        let _ = BufReader::new(stderr).read_to_string(&mut text);
        let _ = stderr_sender.send(text);
    });

    let deadline = std::time::Instant::now() + Duration::from_secs(35);
    let mut session_id = None;
    let mut reached_verification = false;
    let mut observed_events = Vec::new();
    while std::time::Instant::now() < deadline {
        let Ok(line) = event_receiver.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        observed_events.push(line.clone());
        let Ok(event) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if event["event_type"] == "session.started" {
            session_id = event["session_id"].as_str().map(str::to_owned);
        }
        if event["event_type"] == "verification.started" {
            reached_verification = true;
            break;
        }
    }
    if !reached_verification {
        let _ = child.kill();
        let _ = child.wait();
        let stderr = stderr_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap_or_else(|_| "<stderr did not close>".to_owned());
        let tail = observed_events
            .iter()
            .rev()
            .take(12)
            .rev()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n");
        panic!("mock run did not reach its delayed verification step\nrecent events:\n{tail}\nstderr:\n{stderr}");
    }
    assert!(
        session_id.is_some(),
        "the active session identity should be known before recovery"
    );

    let stopped = cli(root, &["--json", "runtime", "stop"]);
    assert_success(&stopped);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&stopped.stdout).unwrap()["status"],
        "stopping"
    );

    let (exit_sender, exit_receiver) = mpsc::channel();
    thread::spawn(move || {
        let _ = exit_sender.send(child.wait());
    });
    let status = exit_receiver
        .recv_timeout(Duration::from_secs(25))
        .expect("CLI should finish reconnecting without hanging")
        .unwrap();
    assert!(
        !status.success(),
        "an interrupted task must not be silently replayed"
    );
    let stderr = stderr_receiver
        .recv_timeout(Duration::from_secs(2))
        .expect("CLI stderr should close");
    assert!(stderr.contains("runtime reconnected"), "stderr: {stderr}");
    assert!(
        stderr.contains("session state was refreshed"),
        "stderr: {stderr}"
    );
    assert!(stderr.contains("task was not replayed"), "stderr: {stderr}");
    let recovered = cli(root, &["--json", "runtime", "status"]);
    assert_success(&recovered);
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&recovered.stdout).unwrap()["status"],
        "connected"
    );
}

#[test]
fn auth_list_reports_environment_status_without_printing_the_key() {
    const TEST_KEY: &str = "sk-auth-list-secret-must-not-print";
    let temporary = tempdir().unwrap();
    let workspace = temporary.path().to_string_lossy();
    let sessions = temporary
        .path()
        .join("sessions")
        .to_string_lossy()
        .to_string();
    let output = Command::new(env!("CARGO_BIN_EXE_harness-cli"))
        .args([
            "--workspace",
            &workspace,
            "--session-root",
            &sessions,
            "--json",
            "auth",
            "list",
        ])
        .env("OPENAI_API_KEY", TEST_KEY)
        .output()
        .expect("CLI should start");

    assert_success(&output);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(stdout.contains("environment"));
    assert!(stdout.contains("OPENAI_API_KEY"));
    assert!(!stdout.contains(TEST_KEY));
    assert!(!result.to_string().contains(TEST_KEY));
}

#[test]
fn documents_and_runs_the_mock_cli_workflow() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .unwrap();

    let run = cli(
        root,
        &[
            "--json",
            "--yes",
            "run",
            "create a mock output",
            &root.to_string_lossy(),
        ],
    );
    assert_success(&run);
    let events = String::from_utf8_lossy(&run.stdout)
        .lines()
        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
        .collect::<Vec<_>>();
    assert!(events
        .iter()
        .any(|event| event["event_type"] == "tool.completed"));
    assert!(events
        .iter()
        .any(|event| event["event_type"] == "session.completed"));
    assert!(root.join("mock-output.txt").is_file());

    let undo = cli(root, &["undo"]);
    assert_success(&undo);
    assert!(!root.join("mock-output.txt").exists());

    let sessions = cli(root, &["sessions"]);
    assert_success(&sessions);
    let session_id = fs::read_dir(root.join("sessions"))
        .unwrap()
        .filter_map(Result::ok)
        .find(|entry| entry.path().extension().and_then(|value| value.to_str()) == Some("jsonl"))
        .unwrap()
        .path()
        .file_stem()
        .unwrap()
        .to_string_lossy()
        .to_string();

    let resume = cli(root, &["--yes", "resume", &session_id, "continue"]);
    assert_success(&resume);
    let inspect = cli(root, &["--json", "session", "inspect", &session_id]);
    assert_success(&inspect);
    let report: serde_json::Value = serde_json::from_slice(&inspect.stdout).unwrap();
    assert!(report["session"]["events"]
        .as_array()
        .unwrap()
        .iter()
        .any(|event| event["event_type"] == "session.resumed"));

    let workspace = cli(root, &["."]);
    assert_success(&workspace);
    assert!(String::from_utf8_lossy(&workspace.stdout).contains("Repository root"));

    let status = cli(root, &["status"]);
    assert_success(&status);
    assert!(String::from_utf8_lossy(&status.stdout).contains("compactions:"));

    let config = cli(root, &["config"]);
    assert_success(&config);
    assert!(String::from_utf8_lossy(&config.stdout).contains("test commands:"));

    let diff = cli(root, &["diff"]);
    assert_success(&diff);
}

#[test]
fn malformed_configuration_is_a_structured_cli_error() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();
    fs::create_dir_all(root.join(".agent")).unwrap();
    fs::write(root.join(".agent/config.toml"), "commands = [").unwrap();

    let output = cli(root, &["--json", "config"]);

    assert!(!output.status.success());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["type"], "error");
    assert!(error["error"].as_str().unwrap().contains("configuration"));
}

/// The real CLI binary, against a real Cargo project, running a real
/// verification command: a deliberately broken edit fails `cargo test`, the
/// agent sees the failure, corrects the file, and verification passes.
#[test]
fn a_broken_edit_is_verified_failed_and_then_corrected_through_the_real_cli() {
    const FIXED: &str = "pub fn value() -> u32 {\n\
                         \x20   2\n\
                         }\n\
                         \n\
                         #[cfg(test)]\n\
                         mod tests {\n\
                         \x20   #[test]\n\
                         \x20   fn value_is_two() {\n\
                         \x20       assert_eq!(super::value(), 2);\n\
                         \x20   }\n\
                         }\n";
    // This compiles but violates the test assertion, so the test command
    // genuinely fails after the cheaper formatting check passes.
    const BROKEN: &str = "pub fn value() -> u32 {\n\
                          \x20   3\n\
                          }\n\
                          \n\
                          #[cfg(test)]\n\
                          mod tests {\n\
                          \x20   #[test]\n\
                          \x20   fn value_is_two() {\n\
                          \x20       assert_eq!(super::value(), 2);\n\
                          \x20   }\n\
                          }\n";

    let temporary = tempdir().unwrap();
    let root = temporary.path();
    Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(root)
        .status()
        .unwrap();
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"cli-lifecycle\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n",
    )
    .unwrap();
    fs::write(root.join("src/lib.rs"), FIXED).unwrap();

    let repair = serde_json::json!({
        "path": "src/lib.rs",
        "broken": BROKEN,
        "fixed": FIXED,
    });
    let mut command = Command::new(env!("CARGO_BIN_EXE_harness-cli"));
    let output = command
        .args([
            "--workspace",
            &root.to_string_lossy(),
            "--session-root",
            &root.join("sessions").to_string_lossy(),
            "--json",
            "--yes",
            "run",
            "make the test pass",
            &root.to_string_lossy(),
        ])
        .env("COGITO_MOCK_REPAIR", repair.to_string())
        .output()
        .expect("CLI should start");

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let events: Vec<serde_json::Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("each stdout line is JSON"))
        .collect();

    let results: Vec<&serde_json::Value> = events
        .iter()
        .filter(|event| event["event_type"] == "verification.result")
        .map(|event| &event["payload"]["data"])
        .collect();
    assert!(
        results.len() >= 2,
        "expected verification to run again after the failure, got {results:#?}"
    );
    let passed = |event: &serde_json::Value| event["passed"] == true;
    assert!(
        results.iter().any(|event| !passed(event)),
        "expected the broken edit to fail verification, got {results:#?}"
    );
    assert!(
        results.iter().any(|event| passed(event)),
        "expected the corrected edit to pass verification, got {results:#?}"
    );
    // The failure must come before the pass, otherwise nothing was corrected.
    let test_results = results
        .iter()
        .filter(|event| {
            matches!(
                event["category"].as_str(),
                Some("TargetedTest" | "GeneralTest")
            )
        })
        .collect::<Vec<_>>();
    let first_failure = test_results
        .iter()
        .position(|event| !passed(event))
        .unwrap();
    let first_pass = test_results.iter().position(|event| passed(event)).unwrap();
    assert!(
        first_failure < first_pass,
        "verification must fail before it passes"
    );
    // The commands are real, not placeholders.
    assert!(
        results
            .iter()
            .any(|event| event["command"].as_str().unwrap().starts_with("cargo ")),
        "expected real cargo verification commands, got {results:#?}"
    );

    // `cargo fmt` is one of the verification steps, so the file on disk is the
    // formatted form of the corrective edit rather than a byte-for-byte copy.
    let contents = fs::read_to_string(root.join("src/lib.rs")).unwrap();
    assert!(
        !contents.contains("not rust"),
        "the broken edit survived: {contents:?}"
    );
    for expected in [
        "pub fn value() -> u32",
        "fn value_is_two",
        "super::value(), 2",
    ] {
        assert!(
            contents.contains(expected),
            "the corrective edit is missing {expected:?}: {contents:?}"
        );
    }

    // The final source is formatted before it is persisted, so undo should
    // restore the original formatted fixture without a false conflict.
    let undo = cli(root, &["undo"]);
    assert_success(&undo);
    assert_eq!(fs::read_to_string(root.join("src/lib.rs")).unwrap(), FIXED);
}

#[test]
fn a_malformed_mock_repair_script_is_reported_clearly() {
    let temporary = tempdir().unwrap();
    let root = temporary.path();

    let mut command = Command::new(env!("CARGO_BIN_EXE_harness-cli"));
    let output = command
        .args([
            "--workspace",
            &root.to_string_lossy(),
            "--session-root",
            &root.join("sessions").to_string_lossy(),
            "--json",
            "--yes",
            "run",
            "do something",
            &root.to_string_lossy(),
        ])
        .env("COGITO_MOCK_REPAIR", "{ not json")
        .output()
        .expect("CLI should start");

    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("COGITO_MOCK_REPAIR"),
        "expected the offending setting to be named, got: {stderr}"
    );
}
