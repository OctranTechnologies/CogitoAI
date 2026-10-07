use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use harness_policy::{AllowAllPolicy, DenyAllPolicy, ExecutionMode, PolicyEngine};
use harness_session::{EventBus, EventType};
use harness_tools::{
    BackgroundProcessStatus, CancellationToken, LocalProcessRunner, ProcessError, ProcessEvent,
    ProcessManager, ProcessRequest, ProcessResult, ProcessRunner, ShellTool, ToolContext,
    ToolRegistry, ToolRequest,
};
use serde_json::json;
use tempfile::tempdir;

fn request(name: &str, arguments: serde_json::Value) -> ToolRequest {
    ToolRequest::new(name, arguments)
}

fn process_request(program: &str, args: &[String], workspace: &std::path::Path) -> ProcessRequest {
    ProcessRequest {
        program: program.to_owned(),
        args: args.to_vec(),
        working_directory: workspace.to_path_buf(),
        timeout: Duration::from_secs(5),
        max_output_bytes: 64 * 1024,
    }
}

fn echo_request(command: &str) -> (&'static str, Vec<String>) {
    #[cfg(windows)]
    {
        ("cmd", vec!["/C".to_owned(), command.to_owned()])
    }
    #[cfg(not(windows))]
    {
        ("sh", vec!["-c".to_owned(), command.to_owned()])
    }
}

fn shell_context<'a>(
    workspace: &'a std::path::Path,
    policy: &'a dyn harness_policy::Policy,
) -> ToolContext<'a> {
    ToolContext {
        policy,
        working_directory: workspace,
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: None,
        session_id: None,
        correlation_id: None,
    }
}

#[test]
fn local_runner_streams_stdout_stderr_and_exit_code() {
    let temporary = tempdir().unwrap();
    let command = if cfg!(windows) {
        "echo out & echo err 1>&2 & exit /B 7"
    } else {
        "printf out; printf err >&2; exit 7"
    };
    let (program, args) = echo_request(command);
    let request = process_request(program, &args, temporary.path());
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);

    let result = LocalProcessRunner
        .execute(request, &CancellationToken::new(), &mut |event| {
            captured.lock().unwrap().push(event);
            Ok(())
        })
        .unwrap();

    assert_eq!(result.exit_code, Some(7));
    assert!(!result.success);
    assert!(result.stdout.contains("out"));
    assert!(result.stderr.contains("err"));
    let events = events.lock().unwrap();
    assert!(events
        .iter()
        .any(|event| matches!(event, ProcessEvent::Started { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, ProcessEvent::Stdout { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, ProcessEvent::Stderr { .. })));
    assert!(events
        .iter()
        .any(|event| matches!(event, ProcessEvent::Exited { .. })));
}

#[test]
fn shell_tool_returns_success_and_preserves_shell_quoting() {
    let temporary = tempdir().unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let result = registry
        .execute(
            &shell_context(temporary.path(), &AllowAllPolicy),
            request("shell", json!({ "command": "echo \"hello world\"" })),
        )
        .unwrap();

    assert!(result.output.contains("hello world"));
    assert_eq!(result.metadata["exit_code"], 0);
    assert_eq!(result.metadata["success"], true);
}

#[test]
fn shell_tool_reports_timeout_and_large_output_safely() {
    let temporary = tempdir().unwrap();
    #[cfg(windows)]
    let timeout_request = ProcessRequest {
        program: "powershell.exe".to_owned(),
        args: vec![
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "Start-Sleep -Seconds 10".to_owned(),
        ],
        working_directory: temporary.path().to_path_buf(),
        timeout: Duration::from_millis(100),
        max_output_bytes: 1024,
    };
    #[cfg(not(windows))]
    let timeout_request = ProcessRequest {
        program: "sh".to_owned(),
        args: vec!["-c".to_owned(), "sleep 10".to_owned()],
        working_directory: temporary.path().to_path_buf(),
        timeout: Duration::from_millis(100),
        max_output_bytes: 1024,
    };
    let timeout_result = LocalProcessRunner
        .execute(timeout_request, &CancellationToken::new(), &mut |_| Ok(()))
        .unwrap();
    assert!(timeout_result.timed_out);
    assert!(timeout_result.duration_ms < 5_000);

    let registry = ToolRegistry::with_workspace_tools();
    let large_command = if cfg!(windows) {
        "for /L %i in (1,1,20000) do @echo 123456789012345678901234567890"
    } else {
        "yes 123456789012345678901234567890 | head -c 2000000"
    };
    let large = registry
        .execute(
            &shell_context(temporary.path(), &AllowAllPolicy),
            request(
                "shell",
                json!({ "command": large_command, "max_output_bytes": 1024 }),
            ),
        )
        .unwrap();
    assert!(large.output.len() <= 2_048);
    assert_eq!(large.metadata["success"], true);
}

#[test]
fn cancellation_stops_a_running_process_and_returns_control() {
    let temporary = tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let workspace = temporary.path().to_path_buf();
    let cancel = cancellation.clone();
    let cancellation_thread = thread::spawn(move || {
        thread::sleep(Duration::from_millis(150));
        cancel.cancel();
    });
    #[cfg(windows)]
    let process = process_request(
        "powershell.exe",
        &[
            "-NoProfile".to_owned(),
            "-NonInteractive".to_owned(),
            "-Command".to_owned(),
            "Start-Sleep -Seconds 10".to_owned(),
        ],
        &workspace,
    );
    #[cfg(not(windows))]
    let process = process_request("sh", &["-c".to_owned(), "sleep 10".to_owned()], &workspace);
    let mut on_event = |_event| Ok(());
    let result = LocalProcessRunner
        .execute(process, &cancellation, &mut on_event)
        .unwrap();
    cancellation_thread.join().unwrap();

    assert!(result.cancelled);
    assert!(result.duration_ms < 5_000);
}

#[test]
fn background_process_lifecycle_supports_readiness_logs_listing_and_stop() {
    let temporary = tempdir().unwrap();
    let manager = Arc::new(ProcessManager::default());
    let registry = ToolRegistry::with_workspace_tools_and_process_manager(
        CancellationToken::new(),
        Arc::clone(&manager),
    );
    let cancellation = CancellationToken::new();
    let policy = AllowAllPolicy;
    let context = ToolContext {
        policy: &policy,
        working_directory: temporary.path(),
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: Some(&cancellation),
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let command = long_running_ready_command();
    let started = registry
        .execute(
            &context,
            request(
                "start_background_command",
                json!({"command": command, "working_directory": "."}),
            ),
        )
        .unwrap();
    let process = started.metadata["process"].clone();
    let process_id = process["id"].as_str().unwrap().to_owned();
    assert_eq!(process["status"], "running");

    let ready = registry
        .execute(
            &context,
            request(
                "wait_for_process_output",
                json!({"process_id": process_id, "pattern": "READY", "timeout_ms": 5000}),
            ),
        )
        .unwrap();
    assert_eq!(ready.metadata["ready"], true);

    let smoke_test_command = if cfg!(windows) {
        "echo test server is reachable"
    } else {
        "printf 'test server is reachable'"
    };
    let verification = registry
        .execute(
            &context,
            request("run_command", json!({"command": smoke_test_command})),
        )
        .unwrap();
    assert_eq!(verification.metadata["success"], true);
    assert!(verification.output.contains("test server is reachable"));

    let logs = registry
        .execute(
            &context,
            request(
                "read_process_output",
                json!({"process_id": process_id, "after_cursor": 0}),
            ),
        )
        .unwrap();
    assert!(logs.output.contains("READY"));
    assert!(logs.metadata["next_cursor"].as_u64().unwrap() > 0);

    let listed = registry
        .execute(&context, request("list_processes", json!({})))
        .unwrap();
    assert!(
        listed.metadata["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| { entry["id"] == process_id && entry["status"] == "running" }),
        "process list was: {}",
        listed.metadata["processes"]
    );

    let stopped = registry
        .execute(
            &context,
            request("stop_process", json!({"process_id": process_id})),
        )
        .unwrap();
    assert_eq!(stopped.metadata["process"]["status"], "stopped");
}

#[test]
fn background_start_and_stop_require_policy_approval_before_side_effects() {
    let temporary = tempdir().unwrap();
    let manager = Arc::new(ProcessManager::default());
    let registry = ToolRegistry::with_workspace_tools_and_process_manager(
        CancellationToken::new(),
        Arc::clone(&manager),
    );
    let policy = PolicyEngine::new(ExecutionMode::Normal, temporary.path());
    let cancellation = CancellationToken::new();
    let context = ToolContext {
        policy: &policy,
        working_directory: temporary.path(),
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: Some(&cancellation),
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let start = registry.execute(
        &context,
        request(
            "start_background_command",
            json!({"command": long_running_ready_command()}),
        ),
    );
    assert!(start
        .unwrap_err()
        .to_string()
        .contains("permission approval required"));
    assert!(manager.list().is_empty());
}

#[test]
fn background_process_lifecycle_is_published_as_runtime_events() {
    let temporary = tempdir().unwrap();
    let manager = Arc::new(ProcessManager::default());
    let registry = ToolRegistry::with_workspace_tools_and_process_manager(
        CancellationToken::new(),
        Arc::clone(&manager),
    );
    let policy = AllowAllPolicy;
    let cancellation = CancellationToken::new();
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        captured.lock().unwrap().push(event.payload.clone());
    }));
    let session_id = harness_core::SessionId::new("background-process-events".to_owned()).unwrap();
    let context = ToolContext {
        policy: &policy,
        working_directory: temporary.path(),
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: Some(&cancellation),
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };

    let started = registry
        .execute(
            &context,
            request(
                "start_background_command",
                json!({"command": long_running_ready_command()}),
            ),
        )
        .unwrap();
    let process_id = started.metadata["process"]["id"].as_str().unwrap();
    registry
        .execute(
            &context,
            request("stop_process", json!({"process_id": process_id})),
        )
        .unwrap();

    let events = events.lock().unwrap();
    assert!(events.iter().any(|event| matches!(
        event,
        harness_session::EventPayload::BackgroundProcessStarted { process_id: id, .. }
            if id == process_id
    )));
    assert!(events.iter().any(|event| matches!(
        event,
        harness_session::EventPayload::BackgroundProcessStatus { process_id: id, status, .. }
            if id == process_id && status == "stopped"
    )));
}

#[test]
fn background_manager_tracks_multiple_processes_crashes_timeouts_and_cancellation() {
    let temporary = tempdir().unwrap();
    let manager = Arc::new(ProcessManager::default());
    let registry = ToolRegistry::with_workspace_tools_and_process_manager(
        CancellationToken::new(),
        Arc::clone(&manager),
    );
    let cancellation = CancellationToken::new();
    let policy = AllowAllPolicy;
    let context = ToolContext {
        policy: &policy,
        working_directory: temporary.path(),
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: Some(&cancellation),
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };

    let command = long_running_ready_command();
    for _ in 0..2 {
        registry
            .execute(
                &context,
                request("start_background_command", json!({"command": command})),
            )
            .unwrap();
    }
    assert_eq!(
        manager
            .list()
            .iter()
            .filter(|process| process.status.is_running())
            .count(),
        2
    );

    let failed = registry
        .execute(
            &context,
            request(
                "start_background_command",
                json!({"command": failing_command()}),
            ),
        )
        .unwrap();
    let failed_id = failed.metadata["process"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    wait_for_process_status(&manager, &failed_id, BackgroundProcessStatus::Failed);
    assert_eq!(
        manager
            .list()
            .iter()
            .find(|process| process.id == failed_id)
            .unwrap()
            .exit_code,
        Some(7)
    );

    cancellation.cancel();
    for process in manager
        .list()
        .into_iter()
        .filter(|process| process.status.is_running())
    {
        wait_for_process_status(&manager, &process.id, BackgroundProcessStatus::Stopped);
    }
}

#[test]
fn background_logs_are_truncated_and_process_timeout_is_enforced() {
    let temporary = tempdir().unwrap();
    let manager = Arc::new(ProcessManager::default());
    let registry = ToolRegistry::with_workspace_tools_and_process_manager(
        CancellationToken::new(),
        Arc::clone(&manager),
    );
    let policy = AllowAllPolicy;
    let cancellation = CancellationToken::new();
    let context = ToolContext {
        policy: &policy,
        working_directory: temporary.path(),
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: Some(&cancellation),
        event_bus: None,
        session_id: None,
        correlation_id: None,
    };
    let noisy = registry
        .execute(
            &context,
            request(
                "start_background_command",
                json!({"command": large_output_command()}),
            ),
        )
        .unwrap();
    let noisy_id = noisy.metadata["process"]["id"].as_str().unwrap().to_owned();
    wait_until_not_running(&manager, &noisy_id);
    let logs = registry
        .execute(
            &context,
            request(
                "read_process_output",
                json!({"process_id": noisy_id, "after_cursor": 0, "max_bytes": 65536}),
            ),
        )
        .unwrap();
    assert_eq!(logs.metadata["truncated"], true);
    assert!(logs.output.len() <= 65_536);

    let timeout = registry
        .execute(
            &context,
            request(
                "start_background_command",
                json!({"command": long_running_ready_command(), "timeout_ms": 100}),
            ),
        )
        .unwrap();
    let timeout_id = timeout.metadata["process"]["id"].as_str().unwrap();
    wait_for_process_status(&manager, timeout_id, BackgroundProcessStatus::Stopped);
    assert!(
        manager
            .list()
            .iter()
            .find(|process| process.id == timeout_id)
            .unwrap()
            .timed_out
    );
}

fn wait_for_process_status(manager: &ProcessManager, id: &str, expected: BackgroundProcessStatus) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if manager
            .list()
            .iter()
            .any(|process| process.id == id && process.status == expected)
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "process {id} did not reach {expected}"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn wait_until_not_running(manager: &ProcessManager, id: &str) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if manager
            .list()
            .iter()
            .any(|process| process.id == id && !process.status.is_running())
        {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "process {id} did not finish"
        );
        thread::sleep(Duration::from_millis(20));
    }
}

fn long_running_ready_command() -> &'static str {
    if cfg!(windows) {
        "echo READY & powershell.exe -NoProfile -NonInteractive -Command Start-Sleep -Seconds 30"
    } else {
        "printf 'READY\\n'; exec sleep 30"
    }
}

fn failing_command() -> &'static str {
    if cfg!(windows) {
        "exit /B 7"
    } else {
        "exit 7"
    }
}

fn large_output_command() -> &'static str {
    if cfg!(windows) {
        "for /L %i in (1,1,15000) do @echo 12345678901234567890"
    } else {
        "yes 12345678901234567890 | head -c 400000"
    }
}

#[test]
fn agent_run_cancellation_from_tool_context_interrupts_a_reused_registry() {
    let temporary = tempdir().unwrap();
    let cancellation = CancellationToken::new();
    let cancel = cancellation.clone();
    let workspace = temporary.path().to_path_buf();
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(ShellTool::new(
        Arc::new(ContextCancellationRunner),
        CancellationToken::new(),
    )));
    let handle = thread::spawn(move || {
        let policy = AllowAllPolicy;
        let context = ToolContext {
            policy: &policy,
            working_directory: &workspace,
            execution_environment: harness_tools::local_execution_environment(),
            cancellation: Some(&cancellation),
            event_bus: None,
            session_id: None,
            correlation_id: None,
        };
        registry.execute(
            &context,
            request("shell", json!({ "command": "sleep for cancellation" })),
        )
    });
    thread::sleep(Duration::from_millis(150));
    cancel.cancel();
    let result = handle.join().unwrap().unwrap();

    assert_eq!(result.metadata["cancelled"], true);
    assert!(result.metadata["duration_ms"].as_u64().unwrap() < 5_000);
}

struct ContextCancellationRunner;

impl ProcessRunner for ContextCancellationRunner {
    fn execute(
        &self,
        _request: ProcessRequest,
        cancellation: &CancellationToken,
        on_event: &mut dyn FnMut(ProcessEvent) -> Result<(), ProcessError>,
    ) -> Result<ProcessResult, ProcessError> {
        on_event(ProcessEvent::Started { pid: None })?;
        let started = std::time::Instant::now();
        while !cancellation.is_cancelled() && started.elapsed() < Duration::from_secs(2) {
            thread::sleep(Duration::from_millis(5));
        }
        let result = ProcessResult {
            exit_code: None,
            success: false,
            timed_out: false,
            cancelled: cancellation.is_cancelled(),
            stdout: String::new(),
            stderr: String::new(),
            duration_ms: started.elapsed().as_millis() as u64,
        };
        on_event(ProcessEvent::Exited {
            result: result.clone(),
        })?;
        Ok(result)
    }
}

#[test]
fn invalid_working_directory_and_denied_shell_are_rejected() {
    let temporary = tempdir().unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let invalid = registry.execute(
        &shell_context(temporary.path(), &AllowAllPolicy),
        request(
            "shell",
            json!({ "command": "echo no", "working_directory": "missing" }),
        ),
    );
    assert!(invalid
        .unwrap_err()
        .to_string()
        .contains("path does not exist"));

    let denied = registry.execute(
        &shell_context(temporary.path(), &DenyAllPolicy),
        request("shell", json!({ "command": "echo denied" })),
    );
    assert!(denied
        .unwrap_err()
        .to_string()
        .contains("permission denied"));
}

#[test]
fn process_events_are_emitted_alongside_tool_events() {
    let temporary = tempdir().unwrap();
    let registry = ToolRegistry::with_workspace_tools();
    let bus = EventBus::new();
    let events = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&events);
    let _subscription = bus.subscribe(Arc::new(move |event: &harness_session::HarnessEvent| {
        captured.lock().unwrap().push(event.event_type);
    }));
    let session_id = harness_core::SessionId::new("process-session").unwrap();
    let context = ToolContext {
        policy: &AllowAllPolicy,
        working_directory: temporary.path(),
        execution_environment: harness_tools::local_execution_environment(),
        cancellation: None,
        event_bus: Some(&bus),
        session_id: Some(&session_id),
        correlation_id: None,
    };

    registry
        .execute(
            &context,
            request("shell", json!({ "command": "echo event" })),
        )
        .unwrap();

    let events = events.lock().unwrap();
    assert!(events.contains(&EventType::ProcessStarted));
    assert!(events.contains(&EventType::ProcessStdout));
    assert!(events.contains(&EventType::ProcessExited));
    assert!(events.contains(&EventType::ToolCompleted));
}
