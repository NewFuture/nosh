"""Captured-output evidence, diagnosis and citation contracts."""

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
        self.data = suite.load_suite(run.HERE / "suites" / "regression.json")
        self.scenarios = {s["check"]: s for s in self.data["scenarios"]
                          if s["check"] in suite.CAPTURE_CHECKS}
        self.scenario = self.scenarios["captured-diagnosis"]
        self.root, self.facts, self.text, self.metadata, self.result, self.answer, self.metrics = capture_trial(
            self.base, self.scenario)

    def grade(self, answer=None, scenario=None, evidence=None):
        scenario = scenario or self.scenario
        question = scenario["inputs"][-1].removeprefix("ai fix").strip()
        return checks.judge(
            scenario, self.answer if answer is None else answer, self.facts,
            self.root, fixtures.snapshot(self.root), self.result, self.metrics,
            observed_input(self.text, self.metadata, question)
            if evidence is None else evidence,
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

    def test_diagnostic_id_is_deterministic_per_trial_identity(self):
        with fixtures.Workspace(self.base / "identities") as workspace:
            ids = [
                workspace.prepare(self.scenario, seed, 1)[2]["diagnostic_id"]
                for seed in (3, 3, 4)
            ]
        self.assertEqual(ids[0], ids[1])
        self.assertNotEqual(ids[0], ids[2])
        self.assertRegex(ids[0], r"^CAPTURE-[0-9a-f]{8}$")

    def test_current_capture_scenarios_require_explicit_ai_fix(self):
        data = copy.deepcopy(self.data)
        scenario = next(item for item in data["scenarios"]
                        if item["check"] == "captured-diagnosis")
        scenario["inputs"][-1] = "# explain the failure"
        path = self.base / "invalid-route.json"
        path.write_text(json.dumps(data), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "explicit ai fix"):
            suite.load_suite(path)

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
                question = self.scenario["inputs"][-1].removeprefix("ai fix").strip()
                verdict = self.grade(evidence=observed_input(
                    self.text, dict(self.metadata, **change), question))
                self.assertFalse(verdict.details["facts"]["components"]["capture"]["passed"])
        second = subprocess.run([sys.executable, "once.py"], cwd=self.root, capture_output=True,
                                text=True, check=False)
        self.assertNotIn("CAPTURE-", second.stderr)
        self.assertFalse(self.grade().details["facts"]["components"]["capture"]["passed"])

    def test_failed_context_and_fix_question_are_required(self):
        question = self.scenario["inputs"][-1].removeprefix("ai fix").strip()
        missing_failure = observed_input(self.text, self.metadata, question)
        missing_failure["inputs"][0]["messages"][0]["text"] = (
            missing_failure["inputs"][0]["messages"][0]["text"].replace("\nexit: 17", "")
        )
        for evidence in (
            missing_failure,
            observed_input(self.text, self.metadata, "different question"),
        ):
            with self.subTest(evidence=evidence):
                verdict = self.grade(evidence=evidence)
                self.assertFalse(verdict.details["facts"]["components"]["capture"]["passed"])
        self.assertTrue(
            self.grade(evidence=observed_input(
                self.text, self.metadata, question
            )).details["facts"]["components"]["capture"]["passed"]
        )

    def test_merged_user_task_headers_are_not_capture_context(self):
        question = self.scenario["inputs"][-1].removeprefix("ai fix").strip()
        text = f"[task trigger=failed exit=17]\n[user_output {json.dumps(self.metadata)}]\n{self.text}\n[/user_output]\n{question}"
        evidence = {"inputs": [{"ev": "step_start", "messages": [{"role": "user", "text": text}]}]}
        self.assertFalse(self.grade(evidence=evidence).details["facts"]["components"]["capture"]["passed"])

    def test_system_context_preserves_capture_and_the_separate_request(self):
        question = self.scenario["inputs"][-1].removeprefix("ai fix").strip()
        evidence = observed_input(self.text, self.metadata, question)
        self.assertTrue(self.grade(evidence=evidence).passed)
        for role in ("tool", "assistant", "user"):
            changed = copy.deepcopy(evidence)
            changed["inputs"][0]["messages"][0]["role"] = role
            with self.subTest(role=role):
                self.assertFalse(self.grade(evidence=changed).details["facts"]["components"]["capture"]["passed"])
        for before, after in (("exit: 17", "exit: 0"), ("failed_command: python3 once.py", "failed_command: other")):
            changed = copy.deepcopy(evidence)
            changed["inputs"][0]["messages"][0]["text"] = changed["inputs"][0]["messages"][0]["text"].replace(before, after)
            with self.subTest(after=after):
                self.assertFalse(self.grade(evidence=changed).details["facts"]["components"]["capture"]["passed"])
        for changed_question in ("different question", ""):
            changed = observed_input(self.text, self.metadata, changed_question)
            with self.subTest(question=changed_question):
                self.assertFalse(self.grade(evidence=changed).details["facts"]["components"]["capture"]["passed"])
        changed = copy.deepcopy(evidence)
        changed["inputs"][0]["messages"].reverse()
        self.assertFalse(self.grade(evidence=changed).details["facts"]["components"]["capture"]["passed"])

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
