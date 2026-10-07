import json
import os
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import compare
import run


class EvaluationSuiteTests(unittest.TestCase):
    def test_suite_contains_the_ten_required_coding_task_shapes(self):
        suite = run.load_suite()
        categories = {task["category"] for task in suite["tasks"]}
        self.assertEqual(len(suite["tasks"]), 10)
        self.assertTrue({
            "small_bug", "feature", "failing_test_repair", "cross_file_refactor",
            "api_update", "unfamiliar_code_trace", "validation", "type_error",
            "ambiguous_requirement", "dirty_git_tree",
        }.issubset(categories))

    def test_fixture_tests_have_declared_baseline_state(self):
        suite = run.load_suite()
        for task in suite["tasks"]:
            with self.subTest(task=task["id"]):
                directory = tempfile.TemporaryDirectory(prefix=".eval-fixture-test-", dir=run.ROOT)
                workspace = Path(directory.name) / "repo"
                try:
                    run.initialize_fixture(task, workspace)
                    commands = task.get("test_commands", [suite["default_test_command"]])
                    results = [run.run_command(command, workspace) for command in commands]
                    if task["baseline_tests_expected_pass"] is None:
                        self.assertEqual(results, [])
                    else:
                        self.assertTrue(results)
                        self.assertEqual(all(result["passed"] for result in results), task["baseline_tests_expected_pass"])
                finally:
                    run.cleanup_temp_directory(directory)

    def test_tagged_event_data_extracts_task_metrics(self):
        events = [
            {"payload": {"type": "model.requested", "data": {"provider": "mock"}}},
            {"payload": {"type": "tool.requested", "data": {"tool": "read_file", "arguments": {"path": "src/a.py"}}}},
            {"payload": {"type": "context.compacted", "data": {}}},
        ]
        self.assertEqual([run.payload_type(item) for item in events], [
            "model.requested", "tool.requested", "context.compacted",
        ])
        self.assertEqual(run.event_data(events[1])["tool"], "read_file")

    def test_automatic_policy_events_are_not_reported_as_human_approvals(self):
        summary = run.summary_for([{
            "success": True, "tests_passed": 1, "tests_total": 1, "regressions": 0,
            "wall_clock_seconds": 1.0, "agent_wall_clock_seconds": 0.5,
            "tokens": {"total": 100, "source": "context_estimate"}, "estimated_cost_usd": None,
            "human_approvals": 0, "approval_events": 2, "approval_requests": 1,
            "safety_violations": 0, "context_compactions": 0, "repeated_failures": 0,
        }])
        self.assertEqual(summary["human_approval_events"], 0)
        self.assertEqual(summary["approval_events"], 2)

    def test_mock_usage_is_not_mistaken_for_provider_tokens_or_cost(self):
        events = [
            {"payload": {"type": "model.response", "data": {"input_tokens": 99, "output_tokens": 7}}},
            {"payload": {"type": "task.run.updated", "data": {"task_run": {
                "completion_status": "done", "changed_files": [],
                "context_metrics": {"estimated_tokens_sent": 20},
            }}}},
        ]
        result = run.collect_metrics(
            {"id": "mock", "category": "fixture", "expected_status": "done"},
            events, {"passed": True, "exit_code": 0, "duration_seconds": 1.0, "stdout": "", "stderr": ""},
            run.ROOT, {}, [], 1.0, 1.0, True,
        )
        self.assertEqual(result["tokens"]["total"], 20)
        self.assertEqual(result["tokens"]["source"], "context_estimate")
        self.assertIsNone(result["tokens"]["reported_input"])
        self.assertIsNone(result["estimated_cost_usd"])

    def test_source_fingerprint_changes_for_uncommitted_fixture_content(self):
        directory = tempfile.TemporaryDirectory(prefix=".eval-source-test-", dir=run.ROOT)
        root = Path(directory.name)
        try:
            git_env = os.environ.copy()
            git_env.update({
                "GIT_AUTHOR_NAME": "Evaluation Test",
                "GIT_AUTHOR_EMAIL": "eval@example.invalid",
                "GIT_COMMITTER_NAME": "Evaluation Test",
                "GIT_COMMITTER_EMAIL": "eval@example.invalid",
            })
            self.assertTrue(run.run_command(["git", "init", "-q"], root, timeout=15)["passed"])
            file = root / "work.txt"
            file.write_text("base\n", encoding="utf-8")
            self.assertTrue(run.run_command(["git", "add", "work.txt"], root, timeout=15, env=git_env)["passed"])
            self.assertTrue(run.run_command(["git", "commit", "-qm", "baseline"], root, timeout=15, env=git_env)["passed"])
            file.write_text("before\n", encoding="utf-8")
            before = run.source_fingerprint(root)
            self.assertTrue(run.run_command(["git", "add", "work.txt"], root, timeout=15, env=git_env)["passed"])
            staged = run.source_fingerprint(root)
            file.write_text("after\n", encoding="utf-8")
            after = run.source_fingerprint(root)
            self.assertTrue(before["working_tree_dirty"])
            self.assertEqual(before["sha256"], staged["sha256"])
            self.assertNotEqual(before["sha256"], after["sha256"])
        finally:
            run.cleanup_temp_directory(directory)

    def test_provider_quality_and_harness_quality_are_not_comparable(self):
        baseline = {"suite_id": "s", "suite_version": 1, "lane": "harness", "provider": {"id": "mock"}}
        candidate = {**baseline, "lane": "model"}
        with self.assertRaisesRegex(ValueError, "lane"):
            compare.compatible(baseline, candidate)

    def test_regression_gate_needs_improvement_and_rejects_worse_correctness(self):
        base = {
            "suite_id": "s", "suite_version": 1, "lane": "harness", "provider": {"id": "mock", "model": "m"},
            "summary": {"success_rate": 0.8, "test_pass_rate": 0.9, "median_wall_clock_seconds": 10,
                        "estimated_cost_usd": None, "total_reported_or_estimated_tokens": 1000,
                        "approval_events": 2, "safety_violations": 0, "regressions": 0},
        }
        candidate = json.loads(json.dumps(base))
        candidate["summary"].update({"success_rate": 0.9, "median_wall_clock_seconds": 9})
        self.assertTrue(compare.decision(base, candidate)["passed"])
        candidate["summary"]["test_pass_rate"] = 0.7
        result = compare.decision(base, candidate)
        self.assertFalse(result["passed"])
        self.assertTrue(any("test correctness" in item for item in result["unacceptable_regressions"]))

    def test_file_map_does_not_allow_workspace_escape(self):
        with self.assertRaisesRegex(ValueError, "escapes workspace"):
            run.write_fixture_file(Path.cwd() / "evals" / "tmp", "../outside.txt", "x")

    def test_report_paths_are_relative_and_flag_workspace_escapes(self):
        directory = tempfile.TemporaryDirectory(prefix=".eval-path-test-", dir=run.ROOT)
        workspace = Path(directory.name) / "repo"
        workspace.mkdir()
        try:
            paths, outside = run.normalize_report_paths([
                str(workspace / "src" / "main.py"),
                str(Path(directory.name) / "secret.txt"),
            ], workspace)
            self.assertEqual(paths, ["<outside-workspace>", "src/main.py"])
            self.assertEqual(outside, 1)
        finally:
            run.cleanup_temp_directory(directory)

    def test_report_output_redacts_temporary_workspace_root(self):
        directory = tempfile.TemporaryDirectory(prefix=".eval-redaction-test-", dir=run.ROOT)
        workspace = Path(directory.name) / "repo"
        workspace.mkdir()
        try:
            text = f"error under {workspace}/src/main.py"
            self.assertEqual(
                run.redact_workspace_paths(text, workspace),
                "error under <workspace>/src/main.py",
            )
        finally:
            run.cleanup_temp_directory(directory)


if __name__ == "__main__":
    unittest.main()
