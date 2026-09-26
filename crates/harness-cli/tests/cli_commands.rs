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

/// The real CLI binary, against a real Cargo project, running a real
/// verification command: a deliberately broken edit fails `cargo test`, the
/// agent sees the failure, corrects the file, and verification passes.
#[test]
fn a_broken_edit_is_verified_failed_and_then_corrected_through_the_real_cli() {
    const FIXED: &str = "pub fn value() -> u32 { 2 }\n\
                         \n\
                         #[cfg(test)]\n\
                         mod tests {\n\
                         \x20   #[test]\n\
                         \x20   fn value_is_two() {\n\
                         \x20       assert_eq!(super::value(), 2);\n\
                         \x20   }\n\
                         }\n";
    // Not valid Rust, so the test command genuinely fails.
    const BROKEN: &str = "pub fn value() -> u32 { this is not rust\n";

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
    let first_failure = results.iter().position(|event| !passed(event)).unwrap();
    let first_pass = results.iter().position(|event| passed(event)).unwrap();
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

    // Undo must not silently clobber the file. `cargo fmt` changed it after the
    // checkpoint was taken, so the restore is refused and reported rather than
    // overwriting whatever is on disk now.
    let undo = cli(root, &["undo"]);
    assert!(
        !undo.status.success(),
        "undo must refuse when the file changed after the checkpoint"
    );
    let stderr = String::from_utf8_lossy(&undo.stderr);
    assert!(
        stderr.contains("src/lib.rs"),
        "expected the conflicting file to be named, got: {stderr}"
    );
    assert!(
        fs::read_to_string(root.join("src/lib.rs"))
            .unwrap()
            .contains("value_is_two"),
        "a refused undo must leave the file alone"
    );
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
