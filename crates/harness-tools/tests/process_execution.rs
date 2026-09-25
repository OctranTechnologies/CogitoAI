use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use harness_policy::{AllowAllPolicy, DenyAllPolicy};
use harness_session::{EventBus, EventType};
use harness_tools::{
    CancellationToken, LocalProcessRunner, ProcessEvent, ProcessRequest, ProcessRunner, ShellTool,
    Tool, ToolContext, ToolRegistry, ToolRequest,
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
    let registry = ToolRegistry::with_workspace_tools();
    let timeout_command = if cfg!(windows) {
        "ping -n 10 127.0.0.1 > nul"
    } else {
        "sleep 10"
    };
    let timeout = registry
        .execute(
            &shell_context(temporary.path(), &AllowAllPolicy),
            request(
                "shell",
                json!({ "command": timeout_command, "timeout_ms": 100 }),
            ),
        )
        .unwrap();
    assert_eq!(timeout.metadata["timed_out"], true);
    assert!(timeout.metadata["duration_ms"].as_u64().unwrap() < 5_000);

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
    let shell = ShellTool::new(Arc::new(LocalProcessRunner), cancellation.clone());
    let workspace = temporary.path().to_path_buf();
    let cancel = cancellation.clone();
    let handle = thread::spawn(move || {
        let policy = AllowAllPolicy;
        let context = shell_context(&workspace, &policy);
        shell.execute(
            &context,
            request("shell", json!({ "command": "ping -n 10 127.0.0.1 > nul" })),
        )
    });
    thread::sleep(Duration::from_millis(150));
    cancel.cancel();
    let result = handle.join().unwrap().unwrap();

    assert_eq!(result.metadata["cancelled"], true);
    assert!(result.metadata["duration_ms"].as_u64().unwrap() < 5_000);
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
