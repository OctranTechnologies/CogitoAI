#!/usr/bin/env python3
"""Run the coding-agent-core-v1 tasks against one harness model provider.

The scripted mock lane measures harness behavior with fixed ideal tool traces.
Provider lanes use the same repositories, prompts, checks, and event metrics;
they measure model-plus-harness behavior. Reports are never aggregated across
lanes, which keeps model quality distinct from harness regressions.
"""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import json
import os
import re
import shutil
import socket
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
EVALS = Path(__file__).resolve().parent
MANIFEST = EVALS / "tasks.json"
PROVIDER_KEY_ENV = {
    "openai": "OPENAI_API_KEY",
    "anthropic": "ANTHROPIC_API_KEY",
    "gemini": "GEMINI_API_KEY",
    "opencode-zen": "OPENCODE_API_KEY",
    "opencode-go": "OPENCODE_API_KEY",
}
PROVIDERS = ("mock", "openai", "anthropic", "gemini", "opencode-zen", "opencode-go")


def load_suite() -> dict[str, Any]:
    suite = json.loads(MANIFEST.read_text(encoding="utf-8"))
    ids = [task["id"] for task in suite["tasks"]]
    if len(ids) != len(set(ids)) or not ids:
        raise ValueError("task IDs must be non-empty and unique")
    for task in suite["tasks"]:
        if not task.get("files") or not task.get("prompt") or not task.get("mock_steps"):
            raise ValueError(f"{task.get('id')}: prompt, files, and mock_steps are required")
    return suite


def source_fingerprint(root: Path) -> dict[str, Any]:
    """Identify the evaluated source tree, including uncommitted source edits."""
    root = root.resolve()
    commit = run_command(["git", "rev-parse", "HEAD"], root, timeout=5)
    status = subprocess.run(
        ["git", "status", "--porcelain=v1", "--untracked-files=all"],
        cwd=root, capture_output=True, check=False,
    )
    if status.returncode != 0:
        return {"commit": None, "working_tree_dirty": None, "sha256": None}

    digest = hashlib.sha256()
    changed = subprocess.run(
        ["git", "diff", "--name-only", "--no-renames", "-z", "HEAD", "--"],
        cwd=root, capture_output=True, check=False,
    )
    untracked = subprocess.run(
        ["git", "ls-files", "--others", "--exclude-standard", "-z"],
        cwd=root, capture_output=True, check=False,
    )
    changed_paths = changed.stdout.split(b"\0") if changed.returncode == 0 else []
    untracked_paths = untracked.stdout.split(b"\0") if untracked.returncode == 0 else []
    for encoded_path in sorted(set(changed_paths + untracked_paths)):
        if not encoded_path:
            continue
        relative = encoded_path.decode("utf-8", errors="surrogateescape")
        # Reports and Python bytecode are generated outputs and must not make
        # a report fingerprint itself or a test cache.
        path_parts = Path(relative).parts
        if (relative == "evals/reports" or relative.startswith("evals/reports/")
                or "__pycache__" in path_parts or relative.endswith(".pyc")):
            continue
        candidate = (root / relative).resolve()
        digest.update(encoded_path)
        if not candidate.is_relative_to(root):
            digest.update(b"<outside-symlink>")
            continue
        if not candidate.is_file():
            digest.update(b"<deleted>")
            continue
        with candidate.open("rb") as source:
            for block in iter(lambda: source.read(1024 * 1024), b""):
                digest.update(block)

    return {
        "commit": commit["stdout"].strip() if commit["passed"] else None,
        "working_tree_dirty": bool(status.stdout.strip()),
        "sha256": digest.hexdigest(),
    }


def run_command(command: list[str], cwd: Path, timeout: int = 90, env: dict[str, str] | None = None) -> dict[str, Any]:
    started = time.perf_counter()
    try:
        creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0) if os.name == "nt" else 0
        completed = subprocess.run(
            command,
            cwd=cwd,
            env=env,
            capture_output=True,
            text=True,
            errors="replace",
            timeout=timeout,
            check=False,
            creationflags=creationflags,
        )
        return {
            "passed": completed.returncode == 0,
            "exit_code": completed.returncode,
            "duration_seconds": time.perf_counter() - started,
            "stdout": completed.stdout[-12000:],
            "stderr": completed.stderr[-12000:],
            "timed_out": False,
        }
    except subprocess.TimeoutExpired as error:
        return {
            "passed": False,
            "exit_code": None,
            "duration_seconds": time.perf_counter() - started,
            "stdout": str(error.stdout or "")[-12000:],
            "stderr": str(error.stderr or "")[-12000:],
            "timed_out": True,
        }


def count_test_cases(result: dict[str, Any]) -> tuple[int, int]:
    output = f"{result.get('stdout', '')}\n{result.get('stderr', '')}"
    unittest = re.search(r"Ran (\d+) tests?", output)
    if unittest:
        total = int(unittest.group(1))
        if result["passed"]:
            return total, total
        failed = re.search(r"FAILED \((?:failures=(\d+))?(?:, )?(?:errors=(\d+))?\)", output)
        failures = sum(int(value or 0) for value in failed.groups()) if failed else 1
        return max(0, total - failures), total
    cargo = re.search(r"test result: (?:ok|FAILED)\.\s+(\d+) passed;\s+(\d+) failed", output)
    if cargo:
        return int(cargo.group(1)), int(cargo.group(1)) + int(cargo.group(2))
    pytest_passed = re.search(r"(\d+) passed", output)
    pytest_failed = re.search(r"(\d+) failed", output)
    if pytest_passed or pytest_failed:
        passed = int(pytest_passed.group(1)) if pytest_passed else 0
        failed = int(pytest_failed.group(1)) if pytest_failed else 0
        return passed, passed + failed
    return (1, 1) if result["passed"] else (0, 1)


def initialize_fixture(task: dict[str, Any], workspace: Path) -> dict[str, str]:
    workspace.mkdir(parents=True, exist_ok=True)
    fixture_files = dict(task["files"])
    if task.get("test_commands", "default") != []:
        fixture_files.setdefault("pytest.ini", "[pytest]\npythonpath = .\n")
    for relative, content in fixture_files.items():
        write_fixture_file(workspace, relative, content)
    dirty_files = task.get("dirty_files", {})

    # Record a clean base revision before introducing intentional user changes.
    # This makes post-run regressions and preservation of dirty work measurable.
    git = shutil.which("git")
    if git:
        git_env = os.environ.copy()
        git_env.update({"GIT_AUTHOR_NAME": "Harness Eval", "GIT_AUTHOR_EMAIL": "eval@example.invalid",
                       "GIT_COMMITTER_NAME": "Harness Eval", "GIT_COMMITTER_EMAIL": "eval@example.invalid"})
        for command in ([git, "init", "-q"], [git, "add", "-A"], [git, "commit", "-qm", "fixture baseline"]):
            result = run_command(list(command), workspace, timeout=15, env=git_env)
            if not result["passed"]:
                raise RuntimeError(f"could not initialize fixture Git repository: {result['stderr']}")
    for relative, content in dirty_files.items():
        write_fixture_file(workspace, relative, content)
    return {relative: sha256_text(content) for relative, content in dirty_files.items()}


def write_fixture_file(root: Path, relative: str, content: str) -> None:
    path = (root / relative).resolve()
    if not path.is_relative_to(root.resolve()):
        raise ValueError(f"fixture path escapes workspace: {relative}")
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8", newline="")


def sha256_text(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def sha256_file(path: Path) -> str | None:
    try:
        return hashlib.sha256(path.read_bytes()).hexdigest()
    except OSError:
        return None


def workspace_path(value: str, root: Path) -> Path:
    normalized = value.removeprefix("\\\\?\\")
    path = Path(normalized)
    return path.resolve() if path.is_absolute() else (root / path).resolve()


def normalize_report_paths(values: list[str], workspace: Path) -> tuple[list[str], int]:
    """Keep report file lists workspace-relative and avoid leaking host paths."""
    root = workspace.resolve()
    paths: set[str] = set()
    outside = 0
    for value in values:
        resolved = workspace_path(value, root)
        try:
            relative = resolved.relative_to(root)
        except ValueError:
            outside += 1
            paths.add("<outside-workspace>")
        else:
            paths.add(relative.as_posix())
    return sorted(paths), outside


def redact_workspace_paths(text: str, workspace: Path) -> str:
    root = str(workspace.resolve())
    for value in {root, root.replace("\\", "/"), "\\\\?\\" + root}:
        text = text.replace(value, "<workspace>")
    return text


def cleanup_temp_directory(context: Any) -> None:
    path = Path(context.name).resolve()
    if path.parent != ROOT.resolve():
        raise RuntimeError(f"refusing to clean temporary evaluation data outside the repository: {path}")
    last_error: OSError | None = None
    for attempt in range(8):
        try:
            context.cleanup()
            return
        except OSError as error:
            last_error = error
            if not path.exists():
                return
            try:
                shutil.rmtree(path)
                return
            except OSError as cleanup_error:
                last_error = cleanup_error
                if attempt < 7:
                    time.sleep(0.05 * (2 ** min(attempt, 4)))
    raise RuntimeError(f"could not clean temporary evaluation data at {path}: {last_error}")


def redact_secrets(text: str) -> str:
    for variable in set(PROVIDER_KEY_ENV.values()):
        secret = os.environ.get(variable)
        if secret:
            text = text.replace(secret, "[REDACTED]")
    return text


def reserve_loopback_endpoint() -> str:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as listener:
        listener.bind(("127.0.0.1", 0))
        return f"127.0.0.1:{listener.getsockname()[1]}"


def parse_jsonl_events(output: str) -> list[dict[str, Any]]:
    events = []
    for line in output.splitlines():
        try:
            item = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(item, dict) and isinstance(item.get("payload"), dict):
            events.append(item)
    return events


def payload_type(event: dict[str, Any]) -> str:
    payload = event.get("payload", {})
    return str(payload.get("type", payload.get("event_type", "")))


def event_data(event: dict[str, Any]) -> dict[str, Any]:
    payload = event.get("payload", {})
    data = payload.get("data", {}) if isinstance(payload, dict) else {}
    return data if isinstance(data, dict) else {}


def task_state(events: list[dict[str, Any]]) -> dict[str, Any]:
    state: dict[str, Any] = {}
    for event in events:
        payload = event_data(event)
        if payload_type(event) == "task.run.updated" and isinstance(payload.get("task_run"), dict):
            state = payload["task_run"]
    return state


def agent_elapsed_seconds(events: list[dict[str, Any]]) -> float | None:
    started = None
    finished = None
    for event in events:
        stamp = event.get("timestamp")
        if not isinstance(stamp, int):
            continue
        kind = payload_type(event)
        if kind == "model.requested" and started is None:
            started = stamp
        if kind in {"session.completed", "session.failed"}:
            finished = stamp
    if started is None or finished is None or finished < started:
        return None
    return (finished - started) / 1000


def collect_metrics(task: dict[str, Any], events: list[dict[str, Any]], cli_result: dict[str, Any],
                    workspace: Path, dirty_hashes: dict[str, str], baseline_results: list[dict[str, Any]],
                    input_rate: float | None, output_rate: float | None,
                    is_mock_lane: bool) -> dict[str, Any]:
    state = task_state(events)
    event_types = [payload_type(event) for event in events]
    typed_payloads = [(payload_type(event), event_data(event)) for event in events]
    payloads = [payload for _, payload in typed_payloads]
    model_responses = [payload for kind, payload in typed_payloads if kind == "model.response"]
    input_values = [payload.get("input_tokens") for payload in model_responses if isinstance(payload.get("input_tokens"), int)]
    output_values = [payload.get("output_tokens") for payload in model_responses if isinstance(payload.get("output_tokens"), int)]
    verification = state.get("verification_results", [])
    verification = verification if isinstance(verification, list) else []

    tests = task.get("test_commands", [load_suite()["default_test_command"]])
    after = [run_command(command, workspace, timeout=90) for command in tests]
    test_counts = [count_test_cases(result) for result in after]
    baseline_counts = [count_test_cases(result) for result in baseline_results]
    regressions = sum(
        1 for baseline, result in zip(baseline_results, after)
        if baseline["passed"] and not result["passed"]
    )
    dirty_preserved = {
        relative: sha256_file(workspace / relative) == expected
        for relative, expected in dirty_hashes.items()
    }
    regressions += sum(not preserved for preserved in dirty_preserved.values())

    final_messages = [payload.get("text", "") for kind, payload in typed_payloads if kind == "assistant.message"]
    final_message = redact_secrets(final_messages[-1]) if final_messages else ""
    status = str(state.get("completion_status", "unavailable")).lower()
    expected_status = task.get("expected_status", "done")
    final_checks = [needle.casefold() in final_message.casefold() for needle in task.get("final_contains", [])]
    no_file_changes_ok = not state.get("changed_files", []) if task.get("no_file_changes") else True
    all_tests_pass = all(result["passed"] for result in after)
    successful = (
        cli_result["exit_code"] == 0
        and status == expected_status
        and all(final_checks)
        and no_file_changes_ok
        and all_tests_pass
        and regressions == 0
        and all(dirty_preserved.values())
    )

    read_paths: set[str] = set()
    for event_type, payload in typed_payloads:
        if event_type == "tool.requested" and payload.get("tool") in {"read_file", "get_file_outline"}:
            arguments = payload.get("arguments", {})
            if isinstance(arguments, dict) and arguments.get("path"):
                read_paths.add(str(arguments["path"]))
    read_paths.update(str(path) for path in state.get("relevant_files", []))
    normalized_reads, _ = normalize_report_paths(sorted(read_paths), workspace)
    changed_files, outside_changes = normalize_report_paths(
        [str(path) for path in state.get("changed_files", [])], workspace
    )
    safety_violations = outside_changes + sum(not preserved for preserved in dirty_preserved.values())
    if task.get("no_file_changes") and changed_files:
        safety_violations += 1

    repeated_signatures: dict[str, int] = {}
    for event_type, payload in typed_payloads:
        if event_type == "tool.failed":
            signature = f"tool:{payload.get('tool')}:{payload.get('error')}"
        elif event_type == "verification.result" and not payload.get("passed", False):
            signature = f"verification:{payload.get('command')}:{payload.get('relevant_output', payload.get('output', ''))}"
        else:
            continue
        repeated_signatures[signature] = repeated_signatures.get(signature, 0) + 1
    repeated_failures = sum(max(0, count - 1) for count in repeated_signatures.values())

    # Scripted providers sometimes return deterministic placeholder Usage for
    # agent-loop tests. It is not tokenizer/provider usage, so the harness lane
    # reports only the runtime's explicit context estimate.
    tokens_reported = (
        sum(input_values) + sum(output_values)
        if not is_mock_lane and (input_values or output_values) else None
    )
    context = state.get("context_metrics", {})
    estimated_context = context.get("estimated_tokens_sent") if isinstance(context, dict) else None
    token_total = tokens_reported if tokens_reported is not None else estimated_context
    estimated_cost = None
    if not is_mock_lane and input_rate is not None and output_rate is not None and input_values and output_values:
        estimated_cost = (sum(input_values) * input_rate + sum(output_values) * output_rate) / 1_000_000

    verification_summary = [{
        "command": item.get("command"), "passed": item.get("passed"),
        "exit_code": item.get("exit_code"),
        "failure_origin": item.get("failure_origin"),
        "affected_files": normalize_report_paths(
            [str(path) for path in item.get("affected_files", [])], workspace
        )[0],
        "relevant_output": redact_workspace_paths(
            redact_secrets(str(item.get("relevant_output", ""))), workspace
        )[:3000],
    } for item in verification]
    return {
        "task_id": task["id"],
        "category": task["category"],
        "success": successful,
        "completion_status": status,
        "expected_status": expected_status,
        "tests_passed": sum(passed for passed, _ in test_counts),
        "tests_total": sum(total for _, total in test_counts),
        "baseline_tests_passed": sum(passed for passed, _ in baseline_counts),
        "baseline_tests_total": sum(total for _, total in baseline_counts),
        "regressions": regressions,
        "turn_count": sum(kind == "model.requested" for kind in event_types),
        "tool_calls": sum(kind == "tool.requested" for kind in event_types),
        "tokens": {
            "reported_input": sum(input_values) if input_values and not is_mock_lane else None,
            "reported_output": sum(output_values) if output_values and not is_mock_lane else None,
            "total": token_total,
            "context_estimated_sent": estimated_context,
            "source": "provider_usage" if tokens_reported is not None else ("context_estimate" if estimated_context is not None else None),
        },
        "estimated_cost_usd": estimated_cost,
        "wall_clock_seconds": cli_result["duration_seconds"],
        "agent_wall_clock_seconds": agent_elapsed_seconds(events),
        "human_approvals": 0,
        "approval_events": sum(kind in {"tool.approved", "tool.denied"} for kind in event_types),
        "approval_mode": "automatic_noninteractive",
        "approval_requests": sum(
            kind == "policy.decision" and payload.get("action") == "ask"
            for kind, payload in typed_payloads
        ),
        "safety_violations": safety_violations,
        "files_read": normalized_reads,
        "files_modified": changed_files,
        "context_compactions": sum(kind == "context.compacted" for kind in event_types),
        "repeated_failures": repeated_failures,
        "verification": verification_summary,
        "final_response": final_message,
        "dirty_files_preserved": dirty_preserved,
        "cli_exit_code": cli_result["exit_code"],
        "error": redact_workspace_paths(
            redact_secrets(str(cli_result.get("error") or "")), workspace
        ) or None,
        "failure_output": redact_workspace_paths(redact_secrets(
            cli_result.get("stderr", "") + "\n" + cli_result.get("stdout", "")
        ), workspace)[-3000:] if not cli_result["passed"] else "",
    }


def run_cli_case(cli: Path, task: dict[str, Any], workspace: Path, args: argparse.Namespace,
                 runtime_binary: Path | None) -> tuple[dict[str, Any], list[dict[str, Any]]]:
    endpoint = reserve_loopback_endpoint()
    session_root = workspace / ".cogito" / "sessions"
    command = [
        str(cli), "--json", "--yes", "--log-level", args.log_level, "--workspace", str(workspace),
        "--session-root", str(session_root), "--rpc-address", endpoint,
        "--model-provider", args.provider, "--model", args.model or "mock-eval",
        "run", task["prompt"], str(workspace),
    ]
    env = os.environ.copy()
    env["PYTHONDONTWRITEBYTECODE"] = "1"
    env["PYTHONPATH"] = str(workspace)
    if runtime_binary:
        env["COGITO_RUNTIME_BINARY"] = str(runtime_binary)
    env.pop("COGITO_MOCK_REPAIR", None)
    env.pop("COGITO_MOCK_SCRIPT", None)
    if args.provider == "mock":
        env["COGITO_MOCK_SCRIPT"] = json.dumps({"steps": task["mock_steps"]}, ensure_ascii=False, separators=(",", ":"))

    started = time.perf_counter()
    try:
        creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0) if os.name == "nt" else 0
        process = subprocess.run(command, cwd=ROOT, env=env, capture_output=True, text=True,
                                 errors="replace", timeout=args.task_timeout, check=False,
                                 creationflags=creationflags)
        result = {
            "passed": process.returncode == 0, "exit_code": process.returncode,
            "duration_seconds": time.perf_counter() - started, "stdout": process.stdout,
            "stderr": process.stderr, "timed_out": False,
        }
    except subprocess.TimeoutExpired as error:
        result = {
            "passed": False, "exit_code": None, "duration_seconds": time.perf_counter() - started,
            "stdout": str(error.stdout or ""), "stderr": str(error.stderr or ""), "timed_out": True,
        }

    result["stdout"] = redact_secrets(result["stdout"])
    result["stderr"] = redact_secrets(result["stderr"])
    for line in result["stdout"].splitlines():
        try:
            value = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(value, dict) and value.get("type") == "error":
            result["error"] = value.get("error")
            break
    events = parse_jsonl_events(result["stdout"])
    # This endpoint is unique to this disposable fixture. Stop only that runtime;
    # the benchmark never shuts down an unrelated user runtime.
    stop = [str(cli), "--json", "--workspace", str(workspace), "--session-root", str(session_root),
            "--rpc-address", endpoint, "runtime", "stop"]
    try:
        creationflags = getattr(subprocess, "CREATE_NO_WINDOW", 0) if os.name == "nt" else 0
        subprocess.run(stop, cwd=ROOT, env=env, capture_output=True, text=True,
                       errors="replace", timeout=10, check=False, creationflags=creationflags)
    except (subprocess.TimeoutExpired, OSError):
        pass
    return result, events


def summary_for(tasks: list[dict[str, Any]]) -> dict[str, Any]:
    completed = [task for task in tasks if task.get("success") is not None]
    latencies = [task["wall_clock_seconds"] for task in completed]
    test_total = sum(task["tests_total"] for task in completed)
    test_passed = sum(task["tests_passed"] for task in completed)
    token_values = [task["tokens"]["total"] for task in completed if task["tokens"]["total"] is not None]
    token_sources = {task["tokens"]["source"] for task in completed if task["tokens"]["total"] is not None}
    cost_values = [task["estimated_cost_usd"] for task in completed if task["estimated_cost_usd"] is not None]
    return {
        "tasks_total": len(tasks),
        "tasks_completed": len(completed),
        "success_rate": sum(bool(task["success"]) for task in completed) / len(completed) if completed else None,
        "test_pass_rate": test_passed / test_total if test_total else None,
        "tests_passed": test_passed,
        "tests_total": test_total,
        "regressions": sum(task["regressions"] for task in completed),
        "median_wall_clock_seconds": statistics.median(latencies) if latencies else None,
        "median_agent_wall_clock_seconds": statistics.median(
            [task["agent_wall_clock_seconds"] for task in completed if task["agent_wall_clock_seconds"] is not None]
        ) if any(task["agent_wall_clock_seconds"] is not None for task in completed) else None,
        "total_reported_or_estimated_tokens": sum(token_values) if token_values else None,
        "token_source": next(iter(token_sources)) if len(token_sources) == 1 else ("mixed" if token_sources else None),
        "estimated_cost_usd": sum(cost_values) if cost_values else None,
        "human_approval_events": sum(task["human_approvals"] for task in completed),
        "approval_events": sum(task["approval_events"] for task in completed),
        "approval_requests": sum(task["approval_requests"] for task in completed),
        "safety_violations": sum(task["safety_violations"] for task in completed),
        "context_compactions": sum(task["context_compactions"] for task in completed),
        "repeated_failures": sum(task["repeated_failures"] for task in completed),
    }


def render_markdown(report: dict[str, Any]) -> str:
    summary = report["summary"]
    source = report.get("source_fingerprint", {})
    tree_state = "dirty" if source.get("working_tree_dirty") else "clean"
    source_hash = source.get("sha256")
    lines = [f"# Evaluation baseline: {report['suite_id']}", "",
             f"- Status: **{report['status']}**", f"- Lane: `{report['lane']}`",
             f"- Provider/model: `{report['provider']['id']}/{report['provider']['model']}`",
             f"- Commit: `{report.get('commit', 'unknown')}`",
             f"- Evaluated source tree: {tree_state}, fingerprint `{source_hash[:16] if source_hash else 'unavailable'}`", "",
             "| Tasks | Success | Tests | Regressions | Median seconds | Tokens | Cost |",
             "|---:|---:|---:|---:|---:|---:|---:|"]
    fmt = lambda value, suffix="": "n/a" if value is None else f"{value}{suffix}"
    success_rate = "n/a" if summary["success_rate"] is None else f"{summary['success_rate']:.0%}"
    lines.append(
        f"| {summary['tasks_completed']}/{summary['tasks_total']} | "
        f"{success_rate} | "
        f"{summary['tests_passed']}/{summary['tests_total']} | {summary['regressions']} | "
        f"{fmt(round(summary['median_wall_clock_seconds'], 2) if summary['median_wall_clock_seconds'] is not None else None)} | "
        f"{fmt(summary['total_reported_or_estimated_tokens'])} | {fmt(summary['estimated_cost_usd'])} |"
    )
    lines.extend(["",
        f"- Tokens: {summary.get('token_source') or 'unavailable'}",
        f"- Human approvals: {summary['human_approval_events']}; approval requests: {summary['approval_requests']}",
        f"- Safety violations: {summary['safety_violations']}; context compactions: {summary['context_compactions']}; repeated failures: {summary['repeated_failures']}",
    ])
    if report.get("setup_error"):
        lines.extend(["", "## Execution blocked", "", "```text", report["setup_error"][-4000:], "```"])
    lines.extend(["", "## Per-task results", "", "| Task | Category | Result | Tests | Turns | Tools | Regressions |", "|---|---|---:|---:|---:|---:|---:|"])
    for task in report["tasks"]:
        result = "not run" if task.get("success") is None else ("pass" if task["success"] else "fail")
        lines.append(f"| {task['task_id']} | {task['category']} | {result} | {task.get('tests_passed', 0)}/{task.get('tests_total', 0)} | {task.get('turn_count', 0)} | {task.get('tool_calls', 0)} | {task.get('regressions', 0)} |")
    lines.extend(["", "Harness-lane tokens are context estimates; model-lane tokens are provider-reported when available. Cost is omitted unless explicit rates are supplied. This report does not combine scripted harness validation with live-model quality.", ""])
    return "\n".join(lines)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--provider", required=True, choices=PROVIDERS,
                        help="Run one lane only. Repeat the suite separately for provider/model comparisons.")
    parser.add_argument("--model", help="Explicit model ID; required for real providers.")
    parser.add_argument("--harness", type=Path, help="Prebuilt CLI executable; defaults to a Cargo build.")
    parser.add_argument("--skip-build", action="store_true", help="Use the expected CLI executable without building.")
    parser.add_argument("--task-timeout", type=int, default=900)
    parser.add_argument("--log-level", choices=("error", "warn", "info", "debug", "trace"), default="error")
    parser.add_argument("--input-usd-per-million", type=float)
    parser.add_argument("--output-usd-per-million", type=float)
    parser.add_argument("--output", type=Path, default=EVALS / "reports" / "latest.json")
    parser.add_argument("--keep-workspaces", action="store_true", help="Retain disposable repositories under evals/reports/workspaces.")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    suite = load_suite()
    if args.provider != "mock" and not args.model:
        raise SystemExit("--model is required when selecting a real provider")

    report: dict[str, Any] = {
        "schema_version": 1,
        "suite_id": suite["suite_id"],
        "suite_version": suite["version"],
        "started_at": dt.datetime.now(dt.timezone.utc).isoformat(),
        "lane": "harness" if args.provider == "mock" else "model",
        "provider": {"id": args.provider, "model": args.model or "mock-eval"},
        "commit": "unknown",
        "status": "running",
        "tasks": [],
        "summary": {},
    }
    source = source_fingerprint(ROOT)
    report["commit"] = source["commit"] or "unknown"
    report["source_fingerprint"] = source

    cli = args.harness.resolve() if args.harness else None
    runtime_binary: Path | None = None
    build_dir: Path | None = None
    if cli is None:
        target_dir = os.environ.get("CARGO_TARGET_DIR")
        build_dir = Path(target_dir).resolve() if target_dir else ROOT / ".eval-target"
        env = os.environ.copy()
        env["CARGO_TARGET_DIR"] = str(build_dir)
        build = run_command(["cargo", "build", "--offline", "-p", "harness-cli", "-p", "harness-rpc"],
                            ROOT, timeout=1800, env=env)
        extension = ".exe" if os.name == "nt" else ""
        cli_candidate = build_dir / "debug" / f"harness-cli{extension}"
        runtime_candidate = build_dir / "debug" / f"cogito-harness-runtime{extension}"
        if not build["passed"] or not cli_candidate.is_file() or not runtime_candidate.is_file():
            report["status"] = "blocked"
            report["setup_error"] = (build["stderr"] or build["stdout"] or "Cargo build did not produce the CLI/runtime binaries")
            cli = None
        else:
            cli, runtime_binary = cli_candidate, runtime_candidate
    else:
        runtime_binary_env = os.environ.get("COGITO_RUNTIME_BINARY")
        runtime_binary = Path(runtime_binary_env).resolve() if runtime_binary_env else None
        if not args.skip_build and not cli.is_file():
            report["status"] = "blocked"
            report["setup_error"] = f"harness executable not found: {cli}"
            cli = None

    if cli:
        report["status"] = "completed"
        cases = suite["tasks"]
        keep_root = EVALS / "reports" / "workspaces" if args.keep_workspaces else None
        if keep_root:
            keep_root.mkdir(parents=True, exist_ok=True)
        for task in cases:
            temp_context = None if keep_root else tempfile.TemporaryDirectory(
                prefix=f".harness-eval-{task['id']}-", dir=ROOT
            )
            workspace = (keep_root / task["id"]).resolve() if keep_root else Path(temp_context.name) / "repo"
            if keep_root and workspace.exists():
                if workspace.parent != keep_root.resolve():
                    raise RuntimeError(f"refusing to remove evaluation path outside {keep_root}: {workspace}")
                shutil.rmtree(workspace)
            dirty_hashes = initialize_fixture(task, workspace)
            before = [run_command(command, workspace, timeout=90) for command in task.get("test_commands", [suite["default_test_command"]])]
            if args.provider in PROVIDER_KEY_ENV and not os.environ.get(PROVIDER_KEY_ENV[args.provider]):
                # Continue: the runtime may have a key in the OS credential store.
                pass
            cli_result, events = run_cli_case(cli, task, workspace, args, runtime_binary)
            metrics = collect_metrics(task, events, cli_result, workspace, dirty_hashes, before,
                                      args.input_usd_per_million, args.output_usd_per_million,
                                      args.provider == "mock")
            report["tasks"].append(metrics)
            if temp_context:
                cleanup_temp_directory(temp_context)
        report["summary"] = summary_for(report["tasks"])
        if report["summary"]["tasks_completed"] == 0:
            report["status"] = "blocked"
        elif report["summary"]["success_rate"] != 1:
            report["status"] = "completed_with_failures"
    else:
        report["tasks"] = [{"task_id": task["id"], "category": task["category"], "success": None,
                             "tests_passed": 0, "tests_total": 0, "regressions": 0, "turn_count": 0,
                             "tool_calls": 0, "tokens": {"total": None}, "estimated_cost_usd": None,
                             "wall_clock_seconds": 0, "human_approvals": 0, "approval_events": 0,
                             "approval_requests": 0, "safety_violations": 0,
                             "files_read": [], "files_modified": [], "context_compactions": 0,
                             "repeated_failures": 0} for task in suite["tasks"]]
        report["summary"] = summary_for(report["tasks"])

    report["finished_at"] = dt.datetime.now(dt.timezone.utc).isoformat()
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    markdown_path = args.output.with_suffix(".md")
    markdown_path.write_text(render_markdown(report), encoding="utf-8")
    print(render_markdown(report))

    # Only delete a target directory created by this runner, and only after its
    # path is confirmed under the repository. Respect a caller-provided target.
    if build_dir == ROOT / ".eval-target" and build_dir.exists():
        resolved_root = ROOT.resolve()
        resolved_build = build_dir.resolve()
        if resolved_build.parent == resolved_root:
            shutil.rmtree(resolved_build, ignore_errors=True)
    return 0 if report["status"] in {"completed", "completed_with_failures"} else 2


if __name__ == "__main__":
    raise SystemExit(main())
