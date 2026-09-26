import copy
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from eval import checks, driver, fixtures, report, run


class DatasetRevisionTests(unittest.TestCase):
    def sample(self):
        return {
            "schema_version": 1,
            "metadata": {"run_id": "revision-test", "observation": "native-v1",
                         "build": {"binary_sha256": "a" * 64},
                         "scenarios": [{"id": "example"}], "seeds": [0], "repeat": 1},
            "trials": [{"scenario_id": "example", "seed": 0, "repeat": 0, "status": "pass",
                        "metrics": {"steps": 2, "confirmations": 0, "ttft_s": None,
                                    "total_s": 1, "peak_rss_mib": 10},
                        "answer": "done", "final_state": {}}],
        }

    def test_historical_reports_implicitly_use_the_first_dataset_revision(self):
        previous = self.sample()
        current = copy.deepcopy(previous)
        current["metadata"]["dataset_revision"] = 1
        self.assertEqual(report.compare(current, previous)["warnings"], [])
        self.assertNotIn("Dataset revision:", report.markdown(previous))
        self.assertIn("Dataset revision: 1.", report.markdown(current))

    def test_semantics_change_is_visible_without_losing_paired_metrics(self):
        previous = self.sample()
        current = copy.deepcopy(previous)
        current["metadata"]["dataset_revision"] = 2
        current["trials"][0]["metrics"]["steps"] = 3
        comparison = report.compare(current, previous)
        self.assertEqual(comparison["paired_trials"], 1)
        self.assertEqual(comparison["pairs"][0]["metric_delta"]["steps"], 1)
        self.assertEqual(comparison["warnings"], [
            "dataset_revision differs; paired deltas are descriptive, not a controlled regression",
        ])
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            report.save(current, directory, previous)
            saved = json.loads((directory / "report.json").read_text(encoding="utf-8"))
            self.assertEqual(saved["metadata"]["dataset_revision"], 2)
            self.assertIn("Dataset revision: 2.", (directory / "report.md").read_text(encoding="utf-8"))

    def test_invalid_report_revisions_are_rejected(self):
        for revision in (0, -1, True, 2.0, "2", None):
            data = self.sample()
            data["metadata"]["dataset_revision"] = revision
            with self.subTest(revision=revision), self.assertRaisesRegex(ValueError, "dataset_revision"):
                report.validate(data)

    @unittest.skipUnless(sys.platform == "linux", "runner metadata uses Linux process priority")
    def test_runtime_records_explicit_and_legacy_dataset_revisions(self):
        args = SimpleNamespace(build_info=None, label="revision-test", legacy=False,
                               threads=1, timeout=None, seeds=None, repeat=1)
        suite = {"schema_version": 2, "timeout_s": 30, "seeds": [0], "scenarios": [{"id": "example"}]}
        with tempfile.TemporaryDirectory() as temporary, patch("eval.run.machine_info", return_value={}):
            path = Path(temporary) / "test-input"
            path.write_bytes(b"metadata-only fixture")
            for revision in (None, 2):
                configured = dict(suite) if revision is None else dict(suite, dataset_revision=revision)
                measured = run.metadata(args, configured, path, path, path, {})
                self.assertEqual(measured["dataset_revision"], revision or 1)
                self.assertEqual(configured, suite if revision is None else dict(suite, dataset_revision=2))
            args.timeout = 60
            configured = dict(suite, timeout_s=240)
            measured = run.metadata(args, configured, path, path, path, {})
            self.assertEqual(measured["settings"]["timeout_s"], 60)
            self.assertEqual(configured["timeout_s"], 240)

    def test_hosted_campaign_budget_covers_every_seed_and_repeat(self):
        suite = run.load_suite(run.HERE / "scenarios.json")
        available_s = (360 - 90) * 60
        self.assertEqual(run.validate_campaign_budget(suite, 2, 60, available_s), 250 * 60)
        self.assertEqual(run.validate_campaign_budget(suite, 2, 64, available_s), 250 * 64)
        for timeout in (65, suite["timeout_s"]):
            with self.subTest(timeout=timeout), self.assertRaisesRegex(ValueError, "250 trial deadlines"):
                run.validate_campaign_budget(suite, 2, timeout, available_s)
        self.assertEqual(run.validate_campaign_budget(suite, 2, 60, 250 * 60), 250 * 60)
        self.assertEqual(suite["timeout_s"], 240, "hosted limits must not change the local suite")
        subset = dict(suite, scenarios=suite["scenarios"][:1], seeds=[0])
        self.assertEqual(run.validate_campaign_budget(subset, 2, 240, available_s), 480)

    def test_invalid_campaign_budgets_are_rejected(self):
        suite = run.load_suite(run.HERE / "scenarios.json")
        for value in (0, -1, True, None, float("nan"), float("inf")):
            with self.subTest(timeout=value), self.assertRaises(ValueError):
                run.validate_campaign_budget(suite, 2, value, 1000)
            with self.subTest(budget=value), self.assertRaises(ValueError):
                run.validate_campaign_budget(suite, 2, 60, value)
        for repeat in (0, -1, True, 1.5):
            with self.subTest(repeat=repeat), self.assertRaises(ValueError):
                run.validate_campaign_budget(suite, repeat, 60, 1000)


@unittest.skipUnless(sys.platform == "linux", "fixture command paths use Linux shell semantics")
class TaskCompletionIntegrationTests(unittest.TestCase):
    def test_allowed_build_prerequisite_does_not_replace_test_execution(self):
        scenario = next(s for s in run.load_suite(run.HERE / "scenarios.json")["scenarios"]
                        if s["id"] == "zh-node-test")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "project"
            facts = fixtures.create(root, "node")
            after = dict(facts["before"])
            for name in ("main.js", "math.js"):
                after["dist/" + name] = facts["before"]["src/" + name]
                (root / "dist").mkdir(exist_ok=True)
                (root / "dist" / name).write_bytes((root / "src" / name).read_bytes())
            metrics = {"task_status": "completed", "steps": 3, "confirmations": 1}
            result = driver.Result(exit_code=0)

            def grade(command, output):
                evidence = {"executions": [{
                    "call": {"name": "run_command", "args": {"command": command}},
                    "state": "executed", "exit_code": 0, "timed_out": False, "interrupted": False,
                    "result": "[exit_code=0 duration=0.01s truncated=no]\n--- stdout ---\n" + output,
                }]}
                return checks.judge(scenario, "两个测试均已通过。", facts, root, after, result, metrics, evidence)

            build_only = grade("npm run build", "Build completed.\n")
            self.assertFalse(build_only.passed)
            self.assertTrue(any("node-test command" in reason for reason in build_only.reasons))
            complete = grade("npm run build && npm test", "# tests 2\n# pass 2\n# fail 0\n")
            self.assertTrue(complete.passed, complete.reasons)
