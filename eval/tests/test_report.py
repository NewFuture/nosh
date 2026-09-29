from __future__ import annotations
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from eval import report
from eval.checks import experience as experience_checks
from .support import SCENARIOS, SUITE


class ReportTests(unittest.TestCase):
    def sample(self):
        return {
            "schema_version": 2,
            "metadata": {"run_id": "test", "observation": "native-v1",
                         "dataset_revision": 12,
                         "build": {"binary_sha256": "a" * 64},
                         "scenarios": [{"id": "example"}], "seeds": [0, 1], "repeat": 1},
            "trials": [{"scenario_id": "example", "seed": 0, "repeat": 0, "status": "pass",
                        "metrics": {"steps": 1, "confirmations": 0, "ttft_s": None, "total_s": 2, "peak_rss_mib": 10},
                        "answer": "answer\n```", "inputs": None, "final_state": {},
                        "grading": {"facts": {"passed": True, "reasons": []}, "experience": None}}],
        }

    def test_denominator_keeps_missing_and_error_trials(self):
        data = self.sample()
        row = report.aggregate(data)[0]
        self.assertEqual((row["pass"], row["planned"], row["missing"]), (1, 2, 1))
        data["trials"].append(dict(data["trials"][0], seed=1, status="error"))
        row = report.aggregate(data)[0]
        self.assertEqual((row["pass"], row["planned"], row["error"]), (1, 2, 1))
        self.assertIn("1/2 (50%)", report.markdown(data))

    def test_device_is_visible_and_cpu_gpu_comparisons_are_not_controlled(self):
        cpu = self.sample()
        cpu["metadata"]["settings"] = {"device": "cpu"}
        gpu = copy.deepcopy(cpu)
        gpu["metadata"]["settings"]["device"] = "cuda:0"
        self.assertIn("Inference device: `cuda:0`", report.markdown(gpu))
        self.assertTrue(any("settings differs" in warning for warning in report.compare(gpu, cpu)["warnings"]))
        cpu["metadata"]["settings"]["device"] = "auto"
        gpu["metadata"]["settings"]["device"] = "auto"
        cpu["trials"][0]["engines"] = [{"device": "cpu"}]
        gpu["trials"][0]["engines"] = [{"device": "cuda:0"}]
        self.assertTrue(any("observed devices differ" in warning for warning in report.compare(gpu, cpu)["warnings"]))
        for engines in ("cuda", [None], [{"device": "auto"}], [{"device": 0}]):
            invalid = copy.deepcopy(cpu)
            invalid["trials"][0]["engines"] = engines
            with self.assertRaisesRegex(ValueError, "invalid observed devices"):
                report.validate(invalid)

    def test_paired_comparison_and_incompatibility(self):
        before = self.sample()
        after = copy.deepcopy(before)
        after["trials"][0]["status"] = "fail"
        after["trials"][0]["metrics"]["total_s"] = 3
        after["metadata"]["model"] = {"weights_sha256": "different"}
        comparison = report.compare(after, before)
        self.assertEqual(comparison["paired_trials"], 1)
        self.assertEqual(len(comparison["regressions"]), 1)
        self.assertEqual(comparison["pairs"][0]["metric_delta"]["total_s"], 1)
        self.assertIsNone(comparison["pairs"][0]["metric_delta"]["ttft_s"])
        self.assertTrue(comparison["warnings"])
        before["metadata"].update(harness_sha256="raw-crlf", harness_content_sha256="same-content")
        after = copy.deepcopy(before)
        after["metadata"]["harness_sha256"] = "raw-lf"
        self.assertEqual(report.compare(after, before)["warnings"], [])

    def test_repeatability_does_not_require_identical_answers_or_timings(self):
        first = self.sample()["trials"][0]
        second = dict(first, repeat=1, answer="different prose")
        pair = report.repetitions([first, second])[0]
        self.assertTrue(pair["consistent"])
        self.assertTrue(pair["answer_changed"])
        self.assertIsNone(pair["inputs_changed"])
        second["final_state"] = {"changed": True}
        self.assertFalse(report.repetitions([first, second])[0]["consistent"])
        second["status"] = "error"
        self.assertIsNone(report.repetitions([first, second])[0]["consistent"])

    def test_missing_state_is_neither_a_pass_nor_consistent_evidence(self):
        data = self.sample()
        data["trials"][0]["final_state"] = None
        with self.assertRaisesRegex(ValueError, "final-state"):
            report.validate(data)
        first = dict(data["trials"][0], status="fail")
        second = dict(first, repeat=1)
        self.assertIsNone(report.repetitions([first, second])[0]["consistent"])
        first.update(final_state={}, inputs=[])
        second.update(final_state={}, inputs=[])
        pair = report.repetitions([first, second])[0]
        self.assertTrue(pair["consistent"])
        self.assertFalse(pair["inputs_changed"])
        self.assertIsNone(pair["tools_changed"])

    def test_plan_membership_and_contradictory_success_are_rejected(self):
        for changed in ({"scenario_id": "unknown"}, {"seed": 2}, {"repeat": 1}):
            data = self.sample()
            data["trials"][0].update(changed)
            with self.subTest(changed=changed), self.assertRaisesRegex(ValueError, "declared plan"):
                report.validate(data)
        for reasons in (["failed evidence"], [1]):
            data = self.sample()
            data["trials"][0]["grading"]["facts"]["reasons"] = reasons
            with self.assertRaisesRegex(ValueError, "fact grading"):
                report.validate(data)
        data = self.sample()
        data["trials"][0]["metrics"]["total_s"] = 10**400
        with self.assertRaisesRegex(ValueError, "invalid total_s"):
            report.validate(data)

    def test_large_partial_plan_does_not_allocate_all_planned_identities(self):
        code = """
import tracemalloc
from eval import report
from eval.tests.test_report import ReportTests
data = ReportTests().sample()
data["metadata"]["repeat"] = 100000
data["trials"] = []
tracemalloc.start()
report.validate(data)
peak = tracemalloc.get_traced_memory()[1]
assert peak < 1000000, peak
"""
        result = subprocess.run([sys.executable, "-B", "-c", code],
                                cwd=Path(__file__).resolve().parents[2], capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_report_shape_roundtrip_and_fences(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            data = self.sample()
            data["trials"][0]["answer"] = "answer  \n```"
            report.save(data, root)
            loaded = json.loads((root / "report.json").read_text())
            report.validate(loaded)
            self.assertEqual(loaded["trials"][0]["answer"], "answer  \n```")
            self.assertIn("````text", (root / "report.md").read_text())
            self.assertFalse(any(line.endswith(" ") for line in (root / "report.md").read_text().splitlines()))
            self.assertIn("Not measured", (root / "report.md").read_text())
        data["trials"].append(data["trials"][0])
        with self.assertRaises(ValueError):
            report.validate(data)


class ExpandedReportTests(unittest.TestCase):
    def data(self):
        data = {
            "schema_version": 2,
            "metadata": {"run_id": "unit-test", "observation": "native-v1",
                         "dataset_revision": 12,
                         "build": {"binary_sha256": "a" * 64},
                         "scenarios": SUITE["scenarios"], "seeds": SUITE["seeds"], "repeat": 2},
            "trials": [],
        }
        for sid, steps, confirmations in (("largest-files", 4, 0), ("typo-correction", 0, 0), ("zh-rust-build", 2, 1)):
            metrics = {"steps": steps, "confirmations": confirmations, "ttft_s": None, "total_s": 2, "peak_rss_mib": 10}
            data["trials"].append({
                "scenario_id": sid, "seed": 0, "repeat": 0, "status": "pass", "metrics": metrics,
                "answer": "任务已经完成。", "final_state": {}, "inputs": None,
                "grading": {"facts": {"passed": True, "reasons": []},
                            "experience": experience_checks.experience(SCENARIOS[sid], "任务已经完成。", metrics)},
            })
        return data

    def test_270_trial_denominator_groups_and_weighted_means(self):
        data = self.data()
        rows = {r["group"]: r for r in report.groups(data)}
        self.assertEqual(rows["all"]["planned"], 270)
        self.assertEqual(rows["all"]["missing"], 267)
        self.assertEqual(rows["all"]["steps"], 2)
        self.assertEqual(rows["model"]["steps"], 3)
        self.assertEqual(rows["model"]["planned"], 260)
        self.assertEqual(rows["local"]["planned"], 10)
        self.assertEqual(rows["expanded"]["planned"], 170)
        self.assertEqual(rows["mvp"]["planned"], 100)
        data["metadata"]["repeat"] = 1
        self.assertEqual(report.groups(data)[0]["planned"], 135)

    def test_v2_round_trip_and_no_success_shaped_missing_grades(self):
        data = self.data()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            report.save(data, root)
            loaded = json.loads((root / "report.json").read_text(encoding="utf-8"))
            report.validate(loaded)
            text = (root / "report.md").read_text(encoding="utf-8")
            self.assertIn("3/270", text)
            self.assertIn("Declared experience budgets", text)
            self.assertIn("facts=pass", text)
        data["trials"][0]["grading"]["experience"] = None
        with self.assertRaises(ValueError):
            report.validate(data)
        data["trials"][0]["status"] = "error"
        report.validate(data)
        data["trials"][0]["metrics"]["steps"] = 1.5
        with self.assertRaises(ValueError):
            report.validate(data)


    def test_save_computes_each_summary_once_for_json_and_markdown(self):
        data = self.data()
        data["trials"][0]["metrics"].update(prompt_tokens=100, cached_tokens=20, completion_tokens=30)
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            with patch.object(report, "aggregate", wraps=report.aggregate) as scenarios, \
                    patch.object(report, "groups", wraps=report.groups) as groups, \
                    patch.object(report, "capture_components", wraps=report.capture_components) as capture:
                report.save(data, directory)
                self.assertEqual((scenarios.call_count, groups.call_count, capture.call_count), (1, 1, 1))
            rendered = (directory / "report.md").read_text(encoding="utf-8")
            self.assertEqual(rendered, report.markdown(data))
            data["summary"] = []
            data["groups"] = []
            self.assertEqual(report.markdown(data), rendered, "standalone rendering must not trust cached tables")

    def test_save_drops_stale_capture_tables_for_a_non_capture_plan(self):
        data = self.data()
        data["metadata"]["scenarios"] = [SCENARIOS["largest-files"]]
        data["trials"] = data["trials"][:1]
        data["capture_components"] = [{"stale": True}]
        with tempfile.TemporaryDirectory() as temporary:
            report.save(data, Path(temporary))
            saved = json.loads(Path(temporary, "report.json").read_text())
        self.assertNotIn("capture_components", saved)
