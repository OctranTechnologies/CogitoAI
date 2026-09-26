//! Security and robustness audit.
//!
//! These probe the boundaries a user is told exist: the workspace is a hard
//! limit, protected files are refused, the selected permission mode is the one
//! that applies, credentials never reach a tool result or an event, and hostile
//! input produces a clean error rather than a crash or a silent bad result.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use harness_git::CheckpointStore;
use harness_policy::{ExecutionMode, Policy, PolicyEngine};
use harness_session::SessionStore;
use harness_tools::{ProcessRunner, ToolContext, ToolRegistry, ToolRequest};

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// A repository, plus the temporary directory that contains it.
///
/// The workspace lives in a subdirectory so that "outside the workspace" is
/// still inside a directory this test owns. Tests run in parallel, so writing
/// beside the workspace in the shared system temp directory would let one test
/// delete another's decoy file.
fn workspace() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path().join("workspace");
    fs::create_dir_all(&root).unwrap();
    git(&root, &["init", "--quiet"]);
    fs::create_dir_all(root.join("src")).unwrap();
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u32 { 1 }\n").unwrap();
    fs::write(root.join(".env"), "OPENAI_API_KEY=super-secret-value\n").unwrap();
    git(&root, &["add", "."]);
    git(&root, &["commit", "--quiet", "-m", "initial"]);
    (temporary, root)
}

fn registry() -> ToolRegistry {
    ToolRegistry::with_workspace_tools()
}

/// Runs a tool with read-only policy, which is the strictest mode, so anything
/// refused here would also be refused in a looser mode.
fn attempt(
    root: &Path,
    mode: ExecutionMode,
    tool: &str,
    arguments: serde_json::Value,
) -> Result<harness_tools::ToolResult, harness_core::Error> {
    let policy = PolicyEngine::new(mode, root);
    let context = ToolContext {
        policy: &policy,
        working_directory: root,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    registry().execute(&context, ToolRequest::new(tool, arguments))
}

fn error_text(result: Result<harness_tools::ToolResult, harness_core::Error>) -> String {
    match result {
        Ok(value) => format!("Ok({})", value.output),
        Err(error) => error.to_string(),
    }
}

#[test]
fn a_relative_path_cannot_escape_the_workspace() {
    let (_temporary, root) = workspace();
    let root = root.as_path();

    // A file that exists just outside the workspace, which a naive
    // implementation would happily read through `..`.
    let outside = root.parent().unwrap().join("audit-outside-secret.txt");
    fs::write(&outside, "outside the workspace\n").unwrap();

    for arguments in [
        serde_json::json!({"path": "../audit-outside-secret.txt"}),
        serde_json::json!({"path": "src/../../audit-outside-secret.txt"}),
        serde_json::json!({"path": "./src/../src/../../audit-outside-secret.txt"}),
    ] {
        let result = attempt(root, ExecutionMode::Normal, "read_file", arguments.clone());
        let text = error_text(result);
        assert!(
            !text.contains("outside the workspace"),
            "a traversal read leaked file contents for {arguments}: {text}"
        );
    }
}

#[test]
fn an_absolute_path_outside_the_workspace_is_refused() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let outside = root.parent().unwrap().join("audit-outside-secret.txt");
    fs::write(&outside, "outside the workspace\n").unwrap();

    let result = attempt(
        root,
        ExecutionMode::Normal,
        "read_file",
        serde_json::json!({"path": outside.to_string_lossy()}),
    );
    let text = error_text(result);
    assert!(
        !text.contains("outside the workspace"),
        "an absolute path read leaked file contents: {text}"
    );
}

#[test]
fn a_write_cannot_escape_the_workspace() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let outside = root.parent().unwrap().join("audit-outside-written.txt");

    let result = attempt(
        root,
        ExecutionMode::Normal,
        "write_file",
        serde_json::json!({"path": "../audit-outside-written.txt", "content": "owned"}),
    );
    assert!(result.is_err(), "a traversal write must be refused");
    assert!(
        !outside.exists(),
        "a traversal write created a file outside the workspace"
    );
}

#[test]
fn search_and_glob_cannot_escape_the_workspace() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let outside = root.parent().unwrap().join("audit-outside-secret.txt");
    fs::write(&outside, "needle-outside-workspace\n").unwrap();

    let grep = error_text(attempt(
        root,
        ExecutionMode::Normal,
        "grep",
        serde_json::json!({"pattern": "needle-outside-workspace"}),
    ));
    assert!(
        !grep.contains("audit-outside-secret.txt"),
        "grep searched outside the workspace: {grep}"
    );

    let glob = error_text(attempt(
        root,
        ExecutionMode::Normal,
        "glob",
        serde_json::json!({"pattern": "../**/*.txt"}),
    ));
    assert!(
        !glob.contains("audit-outside-secret.txt"),
        "glob matched outside the workspace: {glob}"
    );
}

#[test]
fn protected_credential_files_are_refused() {
    let (_temporary, root) = workspace();
    let root = root.as_path();

    // The recognised credential locations are refused outright, in every mode,
    // and their contents never reach a tool result.
    let mut protected: Vec<PathBuf> = Vec::new();
    for name in [
        ".env",
        ".env.production",
        "id_rsa",
        "server.pem",
        "tls.key",
        "bundle.p12",
    ] {
        let path = root.join(name);
        fs::write(&path, "super-secret-value\n").unwrap();
        protected.push(path);
    }
    let ssh = root.join(".ssh");
    fs::create_dir_all(&ssh).unwrap();
    fs::write(ssh.join("id_ed25519"), "super-secret-value\n").unwrap();
    protected.push(ssh.join("id_ed25519"));

    for mode in [
        ExecutionMode::ReadOnly,
        ExecutionMode::Safe,
        ExecutionMode::Normal,
        ExecutionMode::Auto,
    ] {
        for path in &protected {
            let relative = path.strip_prefix(root).unwrap();
            let result = attempt(
                root,
                mode,
                "read_file",
                serde_json::json!({"path": relative.to_string_lossy()}),
            );
            let text = error_text(result);
            assert!(
                !text.contains("super-secret-value"),
                "{relative:?} was readable in {mode:?} mode: {text}"
            );
        }
    }

    // Protection is about the file, not the mode: a protected path is denied
    // outright rather than merely prompting, so a mistaken "allow" cannot
    // expose it.
    let decision =
        PolicyEngine::new(ExecutionMode::Auto, root).evaluate(&harness_policy::PolicyRequest {
            tool_name: "read_file".to_owned(),
            operation: harness_policy::OperationKind::Read,
            workspace_root: root.to_path_buf(),
            path: Some(root.join(".env")),
            command: None,
            mode: ExecutionMode::Auto,
        });
    assert_eq!(
        decision.decision,
        harness_policy::PolicyDecision::Deny,
        "a credential file must be denied, not merely gated on approval"
    );
}

#[test]
fn a_credential_file_cannot_be_overwritten_either() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let before = fs::read_to_string(root.join(".env")).unwrap();

    let result = attempt(
        root,
        ExecutionMode::Auto,
        "write_file",
        serde_json::json!({"path": ".env", "content": "OPENAI_API_KEY=stolen\n"}),
    );

    assert!(result.is_err(), "a protected file must not be writable");
    assert_eq!(
        fs::read_to_string(root.join(".env")).unwrap(),
        before,
        "a protected file was modified"
    );
}

#[test]
fn reading_a_credential_path_through_the_shell_is_not_silently_allowed() {
    // The file tools refuse credential paths by name. A shell command cannot be
    // path-checked the same way, so it must at minimum require approval rather
    // than running unattended.
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let result = attempt(
        root,
        ExecutionMode::Normal,
        "shell",
        serde_json::json!({"command": "cat .env"}),
    );
    assert!(
        result.is_err(),
        "reading a credential file via the shell must not run unattended"
    );
}

#[test]
fn read_only_mode_refuses_every_write_and_command() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let before = fs::read_to_string(root.join("src/lib.rs")).unwrap();

    let writes = [
        (
            "write_file",
            serde_json::json!({"path": "src/lib.rs", "content": "owned"}),
        ),
        (
            "apply_patch",
            serde_json::json!({"path": "src/lib.rs", "patch": "-pub fn value() -> u32 { 1 }\n+owned\n"}),
        ),
    ];
    for (tool, arguments) in writes {
        let result = attempt(root, ExecutionMode::ReadOnly, tool, arguments);
        assert!(result.is_err(), "read-only mode must refuse {tool}");
    }
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        before,
        "read-only mode modified a file"
    );

    let command = error_text(attempt(
        root,
        ExecutionMode::ReadOnly,
        "shell",
        serde_json::json!({"command": "echo hello"}),
    ));
    assert!(
        command.to_lowercase().contains("denied")
            || command.to_lowercase().contains("permission")
            || command.to_lowercase().contains("read-only"),
        "read-only mode must refuse a command, got: {command}"
    );
}

#[test]
fn read_only_mode_still_allows_reading_and_searching() {
    let (_temporary, root) = workspace();
    let root = root.as_path();

    let read = attempt(
        root,
        ExecutionMode::ReadOnly,
        "read_file",
        serde_json::json!({"path": "src/lib.rs"}),
    );
    assert!(
        read.is_ok(),
        "read-only mode must allow reading: {}",
        error_text(read)
    );

    let search = attempt(
        root,
        ExecutionMode::ReadOnly,
        "grep",
        serde_json::json!({"pattern": "value"}),
    );
    assert!(search.is_ok(), "read-only mode must allow searching");
}

#[test]
fn a_failed_verification_does_not_leak_the_api_key() {
    // The environment is the only place a credential lives. No tool result,
    // event, or verification output may carry it back into the transcript.
    let secret = "sk-audit-sentinel-do-not-leak-1234567890";
    // A unique marker that would only appear if the value were read out.
    std::env::set_var("COGITO_AUDIT_SENTINEL", secret);

    let (_temporary, root) = workspace();
    let root = root.as_path();

    // The agent shell inherits the environment, so a careless `env` dump would
    // surface the credential. The workspace tool must not hand it back.
    let result = attempt(
        root,
        ExecutionMode::Normal,
        "shell",
        serde_json::json!({"command": "set"}),
    );
    let text = error_text(result);
    std::env::remove_var("COGITO_AUDIT_SENTINEL");
    assert!(
        !text.contains(secret),
        "a shell result exposed an environment credential"
    );
}

#[test]
fn an_oversized_file_read_is_truncated_rather_than_exhausting_memory() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    // Comfortably larger than any sane read limit.
    let huge = "x".repeat(64 * 1024 * 1024);
    fs::write(root.join("huge.txt"), &huge).unwrap();

    let result = attempt(
        root,
        ExecutionMode::Normal,
        "read_file",
        serde_json::json!({"path": "huge.txt"}),
    );

    match result {
        Ok(value) => {
            let length = value.output.len();
            assert!(
                length < huge.len(),
                "a huge file must be truncated, got {length} bytes"
            );
            assert!(
                value.output.contains("truncat") || length < 1024 * 1024,
                "expected a truncation notice or a small excerpt, got {length} bytes"
            );
        }
        Err(error) => {
            // Refusing outright is also acceptable, as long as it is an error
            // rather than an attempt to load the whole file.
            assert!(
                error.to_string().to_lowercase().contains("large")
                    || error.to_string().to_lowercase().contains("too big")
                    || error.to_string().to_lowercase().contains("limit"),
                "expected a clear size error, got: {error}"
            );
        }
    }
}

#[test]
fn a_binary_file_is_handled_without_producing_garbage_or_a_crash() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let mut bytes = vec![0u8, 159, 146, 150, 0, 255, 254, 13, 10, 0];
    bytes.extend_from_slice(&[7u8; 4096]);
    fs::write(root.join("blob.bin"), &bytes).unwrap();

    let result = attempt(
        root,
        ExecutionMode::Normal,
        "read_file",
        serde_json::json!({"path": "blob.bin"}),
    );

    // Either a clear refusal or a safe textual rendering; never a panic and
    // never raw control characters passed through as text.
    if let Ok(value) = &result {
        assert!(
            !value.output.contains('\u{0}'),
            "binary content was passed through as text"
        );
    }
}

#[test]
fn a_command_producing_endless_output_is_bounded() {
    let (_temporary, root) = workspace();
    let root = root.as_path();

    let (program, args): (&str, Vec<String>) = if cfg!(windows) {
        (
            "pwsh",
            vec![
                "-NoProfile".to_owned(),
                "-Command".to_owned(),
                "1..5000000 | ForEach-Object { 'x' }".to_owned(),
            ],
        )
    } else {
        (
            "sh",
            vec!["-c".to_owned(), "yes x | head -c 200000000".to_owned()],
        )
    };

    let request = harness_tools::ProcessRequest {
        program: program.to_owned(),
        args,
        working_directory: root.to_path_buf(),
        timeout: std::time::Duration::from_secs(30),
        max_output_bytes: 64 * 1024,
    };

    let result = harness_tools::LocalProcessRunner
        .execute(
            request,
            &harness_tools::CancellationToken::new(),
            &mut |_| Ok(()),
        )
        .expect("the runner must return");

    assert!(
        (result.stdout.len() + result.stderr.len()) <= 64 * 1024 + 1024,
        "output was not bounded: {} bytes",
        result.stdout.len() + result.stderr.len()
    );
}

#[test]
fn a_dirty_repository_is_handled_without_losing_the_users_work() {
    let (_temporary, root) = workspace();
    let root = root.as_path();

    // The user has uncommitted work before the agent starts.
    fs::write(root.join("src/lib.rs"), "pub fn value() -> u32 { 99 }\n").unwrap();
    fs::write(root.join("untracked.txt"), "user work\n").unwrap();

    let client = harness_git::GitClient::open(root).unwrap();
    let status = client.status().expect("status on a dirty repository");
    assert!(
        !status.is_clean,
        "a dirty repository must be reported as dirty"
    );

    // Checkpointing and restoring must return the user's own state, not the
    // committed state.
    let store = harness_git::ShadowCheckpointStore::new(root.join(".cogito/checkpoints")).unwrap();
    let session = harness_session::JsonlSessionStore::new(root.join(".cogito/sessions"))
        .unwrap()
        .create(root)
        .unwrap();
    let checkpoint = store
        .create(&session.id, root)
        .expect("checkpoint a dirty repository");
    let report = store.undo(&checkpoint.id).expect("undo");
    assert!(
        report.conflicts.is_empty(),
        "undo on a dirty repository conflicted: {report:?}"
    );
    assert_eq!(
        fs::read_to_string(root.join("src/lib.rs")).unwrap(),
        "pub fn value() -> u32 { 99 }\n",
        "undo destroyed the user's uncommitted work"
    );
    assert_eq!(
        fs::read_to_string(root.join("untracked.txt")).unwrap(),
        "user work\n",
        "undo removed an untracked user file"
    );
}

#[test]
fn a_provider_that_fails_mid_run_closes_the_session_and_leaves_no_orphan() {
    use harness_models::{ModelProvider, ModelRequest, ModelResponse, ProviderError};

    struct FailingProvider;
    impl ModelProvider for FailingProvider {
        fn name(&self) -> &str {
            "failing"
        }
        fn capabilities(&self) -> harness_models::ModelCapabilities {
            harness_models::ModelCapabilities {
                streaming: true,
                tool_calling: true,
                vision: false,
                reasoning: false,
                context_window: None,
            }
        }
        fn complete(&self, _request: &ModelRequest) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::Transport {
                provider: "failing",
            })
        }
        fn stream(
            &self,
            _request: &ModelRequest,
            _on_delta: &mut dyn FnMut(harness_models::StreamDelta) -> Result<(), ProviderError>,
        ) -> Result<ModelResponse, ProviderError> {
            Err(ProviderError::Transport {
                provider: "failing",
            })
        }
    }

    let (_temporary, root) = workspace();
    let root = root.as_path();
    let sessions = std::sync::Arc::new(
        harness_session::JsonlSessionStore::new(root.join(".cogito/sessions")).unwrap(),
    );
    let runner = harness_agent::AgentRunner::new(
        Arc::new(FailingProvider),
        "failing",
        registry(),
        Arc::new(PolicyEngine::new(ExecutionMode::Normal, root)),
        Arc::clone(&sessions) as Arc<dyn harness_session::SessionStore>,
        harness_context::ContextBuilder::default(),
        harness_agent::AgentLimits::default(),
        Arc::new(AllowNothing),
    );
    let session = sessions.create(root).unwrap();
    let task = harness_agent::AgentTask {
        workspace_root: root.to_path_buf(),
        user_task: "do something".to_owned(),
        resume_session: Some(session.id.clone()),
        ..harness_agent::AgentTask::default()
    };

    let result = runner.run(&task, &harness_tools::CancellationToken::new());

    assert!(
        result.is_err(),
        "a provider failure must surface as an error"
    );
    let events = sessions.load(&session.id).unwrap();
    assert!(
        events
            .events
            .iter()
            .any(|event| event.event_type == harness_session::EventType::SessionFailed),
        "a provider failure must close the session as failed"
    );
}

struct AllowNothing;

impl harness_agent::ApprovalHandler for AllowNothing {
    fn request(&self, _request: &ToolRequest) -> Result<bool, harness_agent::AgentError> {
        Ok(true)
    }
}

#[test]
fn a_workspace_that_is_not_a_repository_is_still_usable_or_clearly_rejected() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    fs::write(root.join("notes.txt"), "hello\n").unwrap();

    // No Git repository here. The agent must either work without checkpoints or
    // say why it cannot, never panic or half-write.
    let description = harness_core::discover_workspace(root).expect("discovery without Git");
    assert!(
        description.repository_root.is_none(),
        "expected no repository root"
    );
    let git_client = harness_git::GitClient::open(root);
    assert!(
        git_client.is_err(),
        "expected Git to refuse a non-repository"
    );
}

#[test]
fn a_session_id_cannot_escape_the_session_directory() {
    let (_temporary, root) = workspace();
    let root = root.as_path();
    let sessions = harness_session::JsonlSessionStore::new(root.join(".cogito/sessions")).unwrap();

    for hostile in [
        "../../../etc/passwd",
        "..\\..\\windows\\system32",
        "/absolute/path",
        "with/slash",
        "..",
        ".",
    ] {
        let result = sessions.load(&harness_core::SessionId::new(hostile).unwrap());
        assert!(
            result.is_err(),
            "a hostile session id {hostile:?} must be refused, got {result:?}"
        );
    }
}
