use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use tempfile::tempdir;

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

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
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
