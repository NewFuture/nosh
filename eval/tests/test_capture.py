import copy
import json
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import tomllib
import unittest

from eval import checks, driver, fixtures, runtime, suite


def capture_trial(base, scenario):
    root = base / "project"
    facts = fixtures.create(root, scenario["fixture"],
                            trial_identity=f"{scenario['id']}:0:0")
    original = subprocess.run([sys.executable, "once.py"], cwd=root,
                              env=fixtures.project_environment(base / "home"),
                              capture_output=True, text=True, check=False)
    if original.returncode != 17:
        raise AssertionError(f"fixture did not fail: {original}")
    code = re.search(r"CAPTURE-[0-9a-f]{8}", original.stderr)[0]
    metadata = {
        "command_id": 1, "command": scenario["inputs"][0],
        "execution_cwd": str(root), "exit": 17, "source": "terminal", "state": "captured",
        "mixed": False, "incomplete": False, "truncated": False,
        "retained_bytes": len(original.stderr.encode("utf-8")),
    }
    result = driver.Result(exit_code=0, turns=[{
        "kind": "shell", "exit_code": 17, "output": original.stderr,
    }])
    answer = f"REGION 未设置。diagnostic_id 是 {code}，请配置 REGION 环境变量。"
    metrics = {"task_status": "completed", "steps": 1, "confirmations": 0}
    return root, facts, original.stderr, metadata, result, answer, metrics


def observed_input(text, metadata, question):
    block = f"[user_output {json.dumps(metadata)}]\n{text}\n[/user_output]"
    header = f"[context]\ncwd: {metadata['execution_cwd']}\nexit: 17\nfailed_command: {metadata['command']}"
    return {"inputs": [{"ev": "step_start", "messages": [
        {"role": "system", "text": f"{header}\n{block}"},
        {"role": "user", "text": question},
    ]}]}


class CaptureEvaluationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.data = suite.load_suite(runtime.HERE / "suites" / "regression.json")
        self.scenario = next(s for s in self.data["scenarios"] if s["check"] == "captured-citation")

    def test_binary_defaults_and_explicit_capture_overrides_are_distinct(self):
        for scenario in self.data["scenarios"]:
            expected = "last" if scenario["check"] in suite.CAPTURE_CHECKS else None
            self.assertEqual(scenario.get("capture_output"), expected)
        for mode in ("off", "last", None):
            home = self.base / (mode or "default")
            home.mkdir()
            env = runtime.environment(home, 1, None, capture_output=mode)
            config = tomllib.loads((Path(env["NOSH_HOME"]) / "config.toml").read_text())
            self.assertEqual(config.get("shell", {}).get("capture_output"), mode)
            if mode is None:
                self.assertNotIn("capture_output", config.get("shell", {}), "capture defaults must not be silently overridden")
                self.assertFalse(config["shell"]["command_assist"], "Agent evaluation excludes automatic assistance")

    def test_invalid_capture_settings_and_missing_initial_failure_are_rejected(self):
        for change in ({"capture_output": "all"}, {"capture_output": True},
                       {"capture_output": "off"}, {"capture_output": None},
                       {"completions": [{"kind": "agent"}, {"kind": "agent"}]}):
            data = copy.deepcopy(self.data)
            data["scenarios"] = [dict(self.scenario, **change)]
            path = self.base / "invalid.json"
            path.write_text(json.dumps(data), encoding="utf-8")
            with self.subTest(change=change), self.assertRaises(ValueError):
                suite.load_suite(path)

    def prepare(self):
        return capture_trial(self.base, self.scenario)

    def evidence(self, text, metadata):
        question = self.scenario["inputs"][-1].removeprefix("#fix").strip()
        return observed_input(text, metadata, question)

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
        self.assertIn("the one-shot program was rerun", failed.details["facts"]["components"]["capture"]["reasons"])

    def test_tool_text_cannot_impersonate_original_system_evidence(self):
        root, facts, text, metadata, result, answer, metrics = self.prepare()
        evidence = self.evidence(text, metadata)
        evidence["inputs"][0]["messages"][0]["role"] = "tool"
        verdict = checks.judge(self.scenario, answer, facts, root, fixtures.snapshot(root),
                               result, metrics, evidence)
        self.assertFalse(verdict.passed)
        self.assertIn("the first model request lacks captured output evidence",
                      verdict.details["facts"]["components"]["capture"]["reasons"])
