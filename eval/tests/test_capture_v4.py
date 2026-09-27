import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest

from eval import checks, fixtures, report, run, suite
from .test_capture import capture_trial, observed_input


class DiagnosticContractTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.scenarios = {s["check"]: s for s in suite.load_suite(run.HERE / "scenarios.json")["scenarios"]
                          if s["check"] in suite.CAPTURE_CHECKS}
        self.scenario = self.scenarios["captured-diagnosis"]
        self.root, self.facts, self.text, self.metadata, self.result, self.answer, self.metrics = capture_trial(
            self.base, self.scenario)

    def grade(self, answer=None, scenario=None, evidence=None):
        return checks.judge(
            scenario or self.scenario, self.answer if answer is None else answer, self.facts,
            self.root, fixtures.snapshot(self.root), self.result, self.metrics,
            observed_input(self.text, self.metadata) if evidence is None else evidence,
        )

    def test_fixture_distinguishes_ids_codes_and_real_missing_setting(self):
        self.assertIn("diagnostic_id: CAPTURE-", self.text)
        self.assertIn("error_code: REGION_UNSET", self.text)
        self.assertIn("exit_code: 17", self.text)
        root = self.base / "configured"
        fixtures.create(root, "diagnostic-failure")
        env = dict(fixtures.project_environment(self.base / "home"), REGION="fixture-region")
        configured = subprocess.run([sys.executable, "once.py"], cwd=root, env=env,
                                    capture_output=True, text=True, check=False)
        self.assertEqual(configured.returncode, 0)
        self.assertEqual(configured.stderr, "")

    def test_ordinary_diagnosis_does_not_require_a_random_identifier(self):
        verdict = self.grade("REGION 未设置。请使用 export REGION=所需值 配置环境变量。")
        self.assertTrue(verdict.passed, verdict.reasons)
        parts = verdict.details["facts"]["components"]
        self.assertTrue(parts["capture"]["passed"])
        self.assertTrue(parts["diagnosis"]["passed"])
        self.assertIsNone(parts["citation"]["passed"])
        self.assertFalse(parts["citation"]["required"])

    def test_required_citation_fails_only_its_own_component(self):
        verdict = self.grade("REGION 未设置，配置该环境变量即可解决。退出码是 17。",
                             scenario=self.scenarios["captured-citation"])
        self.assertFalse(verdict.passed)
        parts = verdict.details["facts"]["components"]
        self.assertTrue(parts["capture"]["passed"])
        self.assertTrue(parts["diagnosis"]["passed"])
        self.assertFalse(parts["citation"]["passed"])
        self.assertTrue(all(reason.startswith("citation:") for reason in verdict.reasons))
        self.assertTrue(self.grade(scenario=self.scenarios["captured-citation"]).passed)

    def test_capture_failures_do_not_masquerade_as_diagnosis_failures(self):
        verdict = self.grade(evidence={"inputs": []})
        self.assertFalse(verdict.details["facts"]["components"]["capture"]["passed"])
        self.assertTrue(verdict.details["facts"]["components"]["diagnosis"]["passed"])
        for change in ({"command_id": 2}, {"mixed": True}, {"incomplete": True}, {"state": "unavailable"}):
            with self.subTest(change=change):
                verdict = self.grade(evidence=observed_input(self.text, dict(self.metadata, **change)))
                self.assertFalse(verdict.details["facts"]["components"]["capture"]["passed"])
        second = subprocess.run([sys.executable, "once.py"], cwd=self.root, capture_output=True,
                                text=True, check=False)
        self.assertNotIn("CAPTURE-", second.stderr)
        self.assertFalse(self.grade().details["facts"]["components"]["capture"]["passed"])

    def test_citing_an_id_does_not_excuse_known_unsupported_claims(self):
        for assertion in (
            "REGION 是 Kubernetes 集群的区域标识。",
            "该参数用于内容审核和安全过滤。",
            "退出码 17 对应 Bash 的 TERMINATE_REASIGNAL。",
            "这不是 REGION 未设置导致的错误。",
        ):
            with self.subTest(assertion=assertion):
                verdict = self.grade(self.answer + "\n" + assertion,
                                     scenario=self.scenarios["captured-citation"])
                parts = verdict.details["facts"]["components"]
                self.assertTrue(parts["capture"]["passed"])
                self.assertTrue(parts["citation"]["passed"])
                self.assertFalse(parts["diagnosis"]["passed"])
        for qualification in (
            "如果应用使用 Kubernetes，应查文档确定区域值。",
            "比如在 AWS 环境中，应按应用文档选择该值。",
            "输出未提供应用用途，不能认定是 Kubernetes。",
            "退出码 17 不能据此解释为某个系统信号。",
        ):
            with self.subTest(qualification=qualification):
                verdict = self.grade(self.answer + "\n" + qualification)
                self.assertTrue(verdict.passed, verdict.reasons)

    def test_reports_preserve_component_failures_and_unobserved_trials(self):
        citation = self.scenarios["captured-citation"]
        verdict = self.grade("REGION 未设置，设置该变量即可。", scenario=citation)
        metrics = {name: None for name in report.METRICS}
        metrics.update(self.metrics)
        data = {
            "schema_version": 2,
            "metadata": {"run_id": "components", "observation": "native-v1", "dataset_revision": 4,
                         "build": {"binary_sha256": "a" * 64}, "scenarios": [citation],
                         "seeds": [0, 1], "repeat": 2},
            "trials": [{"scenario_id": citation["id"], "seed": 0, "repeat": 0,
                        "status": "fail", "metrics": metrics, "answer": "REGION 未设置，设置该变量即可。",
                        "final_state": {}, "grading": verdict.details}],
        }
        report.validate(data)
        rows = {row["component"]: row for row in report.capture_components(data)}
        self.assertEqual((rows["capture"]["pass"], rows["capture"]["unobserved"]), (1, 3))
        self.assertEqual((rows["diagnosis"]["pass"], rows["citation"]["fail"]), (1, 1))
        directory = self.base / "report"
        directory.mkdir()
        report.save(data, directory)
        saved = json.loads((directory / "report.json").read_text())
        self.assertEqual(saved["trials"][0]["status"], "fail")
        self.assertEqual(saved["capture_components"], report.capture_components(data))
        markdown = (directory / "report.md").read_text()
        self.assertIn("Captured output: separate verdicts", markdown)
        self.assertIn("capture=pass; diagnosis=pass; citation=fail", markdown)

        invalid = copy.deepcopy(data)
        invalid["trials"][0]["status"] = "pass"
        invalid["trials"][0]["grading"]["facts"]["passed"] = True
        with self.assertRaisesRegex(ValueError, "failed capture components"):
            report.validate(invalid)
        del invalid["trials"][0]["grading"]["facts"]["components"]
        with self.assertRaisesRegex(ValueError, "no component evidence"):
            report.validate(invalid)
        invalid["trials"][0]["status"] = "fail"
        with self.assertRaisesRegex(ValueError, "no component evidence"):
            report.validate(invalid)
