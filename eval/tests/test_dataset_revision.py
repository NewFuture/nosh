import copy
import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from eval import campaign, checks, driver, fixtures, report, runtime, suite as suite_api


class DatasetRevisionTests(unittest.TestCase):
    def sample(self):
        return {
            "schema_version": 2,
            "metadata": {"run_id": "revision-test", "observation": "native-v1",
                         "dataset_revision": 12,
                         "build": {"binary_sha256": "a" * 64},
                         "scenarios": [{"id": "example"}], "seeds": [0], "repeat": 1},
            "trials": [{"scenario_id": "example", "seed": 0, "repeat": 0, "status": "pass",
                        "metrics": {"steps": 2, "confirmations": 0, "ttft_s": None,
                                    "total_s": 1, "peak_rss_mib": 10},
                        "answer": "done", "final_state": {},
                        "grading": {"facts": {"passed": True, "reasons": []}, "experience": None}}],
        }


    def test_semantics_change_is_visible_without_losing_paired_metrics(self):
        previous = self.sample()
        current = copy.deepcopy(previous)
        current["metadata"]["dataset_revision"] = 13
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
            self.assertEqual(saved["metadata"]["dataset_revision"], 13)
            self.assertIn("Dataset revision: 13.", (directory / "report.md").read_text(encoding="utf-8"))

    def test_report_requires_the_current_schema_revision_and_native_observation(self):
        for field, value in (("dataset_revision", None), ("observation", "legacy")):
            data = self.sample()
            data["metadata"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                report.validate(data)
        data = self.sample()
        data["schema_version"] = 1
        with self.assertRaisesRegex(ValueError, "unsupported report schema"):
            report.markdown(data)

    def test_invalid_report_revisions_are_rejected(self):
        for revision in (0, -1, True, 2.0, "2", None):
            data = self.sample()
            data["metadata"]["dataset_revision"] = revision
            with self.subTest(revision=revision), self.assertRaisesRegex(ValueError, "dataset_revision"):
                report.validate(data)

    @unittest.skipUnless(sys.platform == "linux", "runner metadata uses Linux process priority")
    def test_runtime_records_explicit_dataset_revisions(self):
        args = SimpleNamespace(build_info=None, label="revision-test",
                               threads=1, timeout=None, seeds=None, repeat=1)
        suite = {"schema_version": 2, "dataset_revision": 12,
                 "timeout_s": 30, "seeds": [0], "scenarios": [{"id": "example"}]}
        with tempfile.TemporaryDirectory() as temporary, patch("eval.runtime.machine_info", return_value={}):
            path = Path(temporary) / "test-input"
            path.write_bytes(b"metadata-only fixture")
            for revision in (12, 13):
                configured = dict(suite, dataset_revision=revision)
                measured = runtime.metadata(args, configured, path, path, path, {})
                self.assertEqual(measured["dataset_revision"], revision)
                self.assertEqual(measured["observation"], "native-v1")
                self.assertEqual(configured, dict(suite, dataset_revision=revision))
            args.timeout = 60
            configured = dict(suite, timeout_s=240)
            measured = runtime.metadata(args, configured, path, path, path, {})
            self.assertEqual(measured["settings"]["timeout_s"], 60)
            self.assertEqual(configured["timeout_s"], 240)
            with patch.dict("os.environ", {"LD_LIBRARY_PATH": "/cuda/lib64"}):
                measured = runtime.metadata(args, configured, path, path, path, {})
                self.assertEqual(measured["settings"]["device"], "cpu")
                self.assertEqual(measured["settings"]["ld_library_path"], "/cuda/lib64")

    def test_hosted_campaign_budget_covers_every_seed_and_repeat(self):
        suite = suite_api.load_suite(runtime.HERE / "suites" / "regression.json")
        available_s = (360 - 90) * 60
        self.assertEqual(campaign.validate_budget(suite, 2, 60, available_s), 270 * 60)
        self.assertEqual(campaign.validate_budget(suite, 2, 59, available_s), 270 * 59)
        for timeout in (61, suite["timeout_s"]):
            with self.subTest(timeout=timeout), self.assertRaisesRegex(ValueError, "270 trial deadlines"):
                campaign.validate_budget(suite, 2, timeout, available_s)
        self.assertEqual(campaign.validate_budget(suite, 2, 60, 270 * 60), 270 * 60)
        self.assertEqual(suite["timeout_s"], 240, "hosted limits must not change the local suite")
        subset = dict(suite, scenarios=suite["scenarios"][:1], seeds=[0])
        self.assertEqual(campaign.validate_budget(subset, 2, 240, available_s), 480)

    @unittest.skipUnless(sys.platform == "linux", "runner metadata uses Linux process priority")
    def test_nested_scorers_are_fingerprinted_but_tests_and_archives_are_not(self):
        args = SimpleNamespace(build_info=None, label="source-test",
                               threads=1, timeout=None, seeds=None, repeat=1)
        suite = {"schema_version": 2, "dataset_revision": 12,
                 "timeout_s": 30, "seeds": [0], "scenarios": [{"id": "example"}]}
        with tempfile.TemporaryDirectory() as temporary, patch("eval.runtime.machine_info", return_value={}):
            root = Path(temporary)
            (root / "checks").mkdir()
            (root / "tests").mkdir()
            (root / "baselines").mkdir()
            (root / "campaign.py").write_text("runner = 1\n")
            (root / "checks" / "__init__.py").write_text("")
            scorer = root / "checks" / "agent.py"
            scorer.write_text("score = 1\n")
            binary = root / "binary"
            binary.write_bytes(b"fixture")
            with patch("eval.runtime.HERE", root):
                before = runtime.metadata(args, suite, binary, binary, binary, {})
                scorer.write_text("score = 2\n")
                changed = runtime.metadata(args, suite, binary, binary, binary, {})
                self.assertNotEqual(before["grading_content_sha256"], changed["grading_content_sha256"])
                self.assertNotEqual(before["harness_content_sha256"], changed["harness_content_sha256"])
                self.assertEqual(before["suite_sha256"], changed["suite_sha256"])
                (root / "tests" / "test_ignore.py").write_text("test = 1\n")
                (root / "baselines" / "old.py").write_text("score = 0\n")
                ignored = runtime.metadata(args, suite, binary, binary, binary, {})
                for key in ("harness_sha256", "harness_content_sha256", "grading_content_sha256", "suite_sha256"):
                    self.assertEqual(changed[key], ignored[key])

    def test_invalid_campaign_budgets_are_rejected(self):
        suite = suite_api.load_suite(runtime.HERE / "suites" / "regression.json")
        for value in (0, -1, True, None, float("nan"), float("inf")):
            with self.subTest(timeout=value), self.assertRaises(ValueError):
                campaign.validate_budget(suite, 2, value, 1000)
            if value is not None:
                with self.subTest(budget=value), self.assertRaises(ValueError):
                    campaign.validate_budget(suite, 2, 60, value)
        self.assertEqual(campaign.validate_budget(suite, 2, 60), 270 * 60)
        for repeat in (0, -1, True, 1.5):
            with self.subTest(repeat=repeat), self.assertRaises(ValueError):
                campaign.validate_budget(suite, repeat, 60, 1000)

    def test_build_provenance_rejects_malformed_shapes_without_type_errors(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            binary, build = root / "binary", root / "build.json"
            binary.write_bytes(b"test binary")
            args = SimpleNamespace(build_info=build, label="build-test", threads=1,
                                   timeout=None, seeds=None, repeat=1)
            valid = {"schema_version": 1, "binary_sha256": fixtures.file_hash(binary), "source_revision": "a" * 40}
            cases = (None, [], "invalid", dict(valid, schema_version=True),
                     dict(valid, source_revision=[]), dict(valid, binary_sha256="wrong"))
            for value in cases:
                build.write_text(json.dumps(value))
                with self.subTest(value=value), self.assertRaisesRegex(ValueError, "build info"):
                    runtime.metadata(args, {}, binary, binary, binary, {})


@unittest.skipUnless(sys.platform == "linux", "fixture command paths use Linux shell semantics")
class TaskCompletionIntegrationTests(unittest.TestCase):
    def test_allowed_build_prerequisite_does_not_replace_test_execution(self):
        scenario = next(s for s in suite_api.load_suite(runtime.HERE / "suites" / "regression.json")["scenarios"]
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
                    "call": {"name": "exec", "args": {"command": command}},
                    "state": "executed", "exit_code": 0, "timed_out": False, "interrupted": False,
                    "result": "[exit_code=0 duration=0.01s truncated=no]\n--- stdout ---\n" + output,
                }]}
                return checks.judge(scenario, "两个测试均已通过。", facts, root, after, result, metrics, evidence)

            build_only = grade("npm run build", "Build completed.\n")
            self.assertFalse(build_only.passed)
            self.assertTrue(any("node-test command" in reason for reason in build_only.reasons))
            complete = grade("npm run build && npm test", "# tests 2\n# pass 2\n# fail 0\n")
            self.assertTrue(complete.passed, complete.reasons)
