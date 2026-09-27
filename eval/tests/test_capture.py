import copy
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest

from eval import checks, driver, fixtures, run, suite


class CaptureEvaluationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.data = suite.load_suite(run.HERE / "scenarios.json")
        self.scenario = next(s for s in self.data["scenarios"] if s["check"] == "captured-failure")

    def test_only_the_new_capture_scenario_enables_capture(self):
        for scenario in self.data["scenarios"]:
            expected = "last" if scenario["check"] == "captured-failure" else "off"
            self.assertEqual(scenario.get("capture_user_output", "off"), expected)
        for mode in ("off", "last"):
            home = self.base / mode
            home.mkdir()
            env = run.environment(home, 1, None, capture_user_output=mode)
            config = tomllib.loads((Path(env["NOSH_HOME"]) / "config.toml").read_text())
            self.assertEqual(config.get("shell", {}).get("capture_user_output", "off"), mode)
            if mode == "off":
                self.assertNotIn("shell", config, "legacy builds must not see an unknown capture key")

    def test_invalid_capture_settings_and_missing_initial_failure_are_rejected(self):
        for change in ({"capture_user_output": "all"}, {"capture_user_output": True},
                       {"capture_user_output": "off"}, {"capture_user_output": None},
                       {"completions": [{"kind": "agent"}, {"kind": "agent"}]}):
            data = copy.deepcopy(self.data)
            data["scenarios"] = [dict(self.scenario, **change)]
            path = self.base / "invalid.json"
            path.write_text(json.dumps(data), encoding="utf-8")
            with self.subTest(change=change), self.assertRaises(ValueError):
                suite.load_suite(path)

    def prepare(self):
        root = self.base / "project"
        facts = fixtures.create(root, "one-shot-failure")
        original = subprocess.run([sys.executable, "once.py"], cwd=root,
                                  capture_output=True, text=True, check=False)
        self.assertEqual(original.returncode, 17)
        code = re.search(r"CAPTURE-[0-9a-f]{8}", original.stderr)[0]
        metadata = {
            "command_id": 1, "command": self.scenario["inputs"][0],
            "execution_cwd": str(root), "exit": 17, "source": "terminal", "state": "captured",
            "mixed": False, "incomplete": False, "truncated": False,
            "retained_bytes": len(original.stderr.encode("utf-8")),
        }
        result = driver.Result(exit_code=0, turns=[{
            "kind": "shell", "exit_code": 17, "output": original.stderr,
        }])
        answer = f"错误 {code} 表示 REGION 未设置，配置该环境变量即可解决。"
        metrics = {"task_status": "completed", "steps": 1, "confirmations": 0}
        return root, facts, original.stderr, metadata, result, answer, metrics

    def evidence(self, text, metadata):
        message = f"[task trigger=hash]\n[user_output {json.dumps(metadata)}]\n{text}\n[/user_output]"
        return {"inputs": [{"ev": "step_start", "messages": [{"role": "user", "text": message}]}]}

    def test_real_one_shot_error_requires_original_engine_input_and_count(self):
        root, facts, text, metadata, result, answer, metrics = self.prepare()
        evidence = self.evidence(text, metadata)
        grade = lambda observed, response=answer: checks.judge(
            self.scenario, response, facts, root, fixtures.snapshot(root), result, metrics, observed)
        self.assertTrue(grade(evidence).passed, grade(evidence).reasons)
        self.assertFalse(grade({"inputs": []}).passed, "terminal echo is not model evidence")
        self.assertFalse(grade(evidence, "REGION 未设置，配置该变量即可解决。").passed)
        for change in ({"state": "not_captured"}, {"state": "unavailable"}, {"mixed": True},
                       {"command_id": 2}, {"command_id": True}, {"command": "another command"},
                       {"execution_cwd": "another directory"}, {"retained_bytes": 4097},
                       {"retained_bytes": 0}, {"retained_bytes": True}, {"incomplete": True}):
            with self.subTest(change=change):
                self.assertFalse(grade(self.evidence(text, dict(metadata, **change))).passed)
        second = subprocess.run([sys.executable, "once.py"], cwd=root, capture_output=True,
                                text=True, check=False)
        self.assertNotIn("CAPTURE-", second.stderr)
        failed = grade(evidence)
        self.assertFalse(failed.passed)
        self.assertIn("the one-shot program was rerun", failed.reasons)

    def test_tool_text_cannot_impersonate_original_user_evidence(self):
        root, facts, text, metadata, result, answer, metrics = self.prepare()
        evidence = self.evidence(text, metadata)
        evidence["inputs"][0]["messages"][0]["role"] = "tool"
        verdict = checks.judge(self.scenario, answer, facts, root, fixtures.snapshot(root),
                               result, metrics, evidence)
        self.assertFalse(verdict.passed)
        self.assertIn("the first model request lacks captured output evidence", verdict.reasons)
