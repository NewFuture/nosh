"""The planning entry point never starts a model, fixture or tool discovery."""

import contextlib
import io
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from eval import campaign, fixtures, report, runtime
from eval.checks.experience import experience


@unittest.skipUnless(sys.platform == "linux", "evaluation CLI requires Linux/WSL")
class PlanningTests(unittest.TestCase):
    def invoke(self, *arguments):
        stdout, stderr = io.StringIO(), io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr), \
                patch('eval.runtime.discover_tools', side_effect=AssertionError("planning discovered tools")), \
                patch('eval.runtime.model_files', side_effect=AssertionError("planning loaded a model")), \
                patch('eval.campaign.os.pidfd_open', side_effect=AssertionError("planning started a process")):
            status = campaign.main(list(arguments))
        return status, stdout.getvalue(), stderr.getvalue()

    def test_builtin_plan_requires_no_model_or_output_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary) / "must-not-exist"
            status, stdout, stderr = self.invoke("--suite", "smoke", "--plan", "--output", str(output))
            self.assertEqual(status, 0, stderr)
            data = json.loads(stdout)
            self.assertEqual((data["trials"], data["seeds"], data["repeat"]), (5, [0], 1))
            self.assertEqual(data["dataset_revision"], 12)
            self.assertEqual(data["maximum_trial_seconds"], 600)
            self.assertFalse(output.exists())

    def test_plan_uses_selected_scenarios_seeds_repeats_and_timeout(self):
        arguments = (
            "--suite", "regression", "--scenario", "zh-python-test", "--scenario", "largest-files",
            "--seeds", "2", "0", "--repeat", "2", "--timeout", "3", "--plan",
        )
        status, stdout, stderr = self.invoke(*arguments, "--budget", "24")
        self.assertEqual(status, 0, stderr)
        plan = json.loads(stdout)
        self.assertEqual(plan["scenarios"], ["largest-files", "zh-python-test"])
        self.assertEqual(plan["seeds"], [2, 0])
        self.assertEqual((plan["trials"], plan["maximum_trial_seconds"]), (8, 24))
        status, stdout, stderr = self.invoke(*arguments, "--budget", "23")
        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertIn("8 trial deadlines", stderr)

    def test_invalid_plans_are_errors_not_partial_successes(self):
        for arguments in (
            ("--scenario", "missing"),
            ("--seeds", "0", "0"),
            ("--repeat", "0"),
            ("--timeout", "nan"),
            ("--budget", "inf"),
            ("--budget", "-1"),
            ("--timeout", "1e308"),
            ("--repeat", "1" + "0" * 400),
            ("--threads", "0"),
        ):
            with self.subTest(arguments=arguments):
                status, stdout, stderr = self.invoke("--plan", *arguments)
                self.assertEqual(status, 2)
                self.assertEqual(stdout, "")
                self.assertTrue(stderr.startswith("eval: "))

    def test_execution_without_a_model_fails_before_tool_discovery(self):
        status, stdout, stderr = self.invoke("--suite", "smoke")
        self.assertEqual(status, 2)
        self.assertEqual(stdout, "")
        self.assertIn("--model-path is required", stderr)

    def test_public_entry_supports_planning_and_rejects_retired_flags(self):
        result = subprocess.run(
            [sys.executable, "-B", "-m", "eval", "--suite", "smoke", "--plan"],
            cwd=runtime.ROOT, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(json.loads(result.stdout)["trials"], 5)
        rejected = subprocess.run(
            [sys.executable, "-B", "-m", "eval", "--legacy", "--plan"],
            cwd=runtime.ROOT, capture_output=True, text=True, timeout=10,
        )
        self.assertEqual(rejected.returncode, 2)
        self.assertEqual(rejected.stdout, "")
        self.assertIn("unrecognized arguments: --legacy", rejected.stderr)

    def test_campaign_exit_codes_preserve_the_plan_and_completed_trials(self):
        save = report.save
        for outcome, expected_code in (
            ("pass", 0), ("fail", 1), ("error", 2), ("interrupt", 130),
            ("interrupt-write-error", 130), ("invalid-row", 2),
        ):
            with self.subTest(outcome=outcome), tempfile.TemporaryDirectory(prefix="nosh-campaign-") as temporary:
                root = Path(temporary)
                output, work = root / "report", root / "work"
                weights, tokenizer = root / "model.gguf", root / "tokenizer.json"
                weights.write_bytes(b"model stub")
                tokenizer.write_text("{}")

                def metadata(args, suite, binary, model, tokens, tools):
                    return {
                        "run_id": "campaign-" + outcome, "observation": "native-v1",
                        "dataset_revision": suite["dataset_revision"],
                        "build": {"binary_sha256": "a" * 64},
                        "settings": {"timeout_s": suite["timeout_s"]},
                        "scenarios": suite["scenarios"], "seeds": args.seeds, "repeat": args.repeat,
                    }

                def run_trial(args, meta, scenario, seed, repeat, workspace, destination, binary, model):
                    if outcome.startswith("interrupt") and seed == 1:
                        raise KeyboardInterrupt()
                    status = "pass" if outcome.startswith("interrupt") or outcome == "invalid-row" else outcome
                    metrics = {name: None for name in report.METRICS}
                    metrics.update(steps=1, confirmations=0, total_s=0.1)
                    grading = None if status == "error" else {
                        "facts": {"passed": status == "pass", "reasons": [] if status == "pass" else ["task failed"]},
                        "experience": experience(scenario, "Complete.", metrics),
                    }
                    return {
                        "scenario_id": scenario["id"], "seed": seed, "repeat": repeat,
                        "status": status, "metrics": metrics, "answer": "Complete.",
                        "final_state": None if status == "error" or outcome == "invalid-row" else {"files": {}, "cwd": None},
                        "grading": grading, "reasons": [] if status == "pass" else [status],
                    }

                def save_report(data, directory, previous=None):
                    if outcome == "interrupt-write-error" and data.get("error"):
                        raise OSError("simulated disk full")
                    return save(data, directory, previous)

                stderr = io.StringIO()
                with patch("eval.runtime.discover_tools", return_value={}), \
                        patch("eval.runtime.model_files", return_value=(weights, tokenizer)), \
                        patch("eval.runtime.metadata", side_effect=metadata), \
                        patch("eval.campaign.run_trial", side_effect=run_trial), \
                        patch("eval.campaign.report.save", side_effect=save_report), \
                        contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(stderr):
                    status = campaign.main([
                        "--scenario", "largest-files", "--seeds", "0", "1",
                        "--binary", sys.executable, "--model-path", str(weights),
                        "--output", str(output), "--work-dir", str(work),
                    ])
                self.assertEqual(status, expected_code)
                data = json.loads((output / "report.json").read_text())
                report.validate(data)
                overall = next(row for row in data["groups"] if row["group"] == "all")
                self.assertEqual(overall["planned"], 2)
                recorded = 0 if outcome == "invalid-row" else 1 if outcome.startswith("interrupt") else 2
                self.assertEqual(len(data["trials"]), recorded)
                self.assertEqual(overall["missing"], 2 - recorded)
                self.assertEqual(bool(data.get("error")), outcome == "interrupt")
                if outcome in ("interrupt-write-error", "invalid-row"):
                    self.assertIn("could not preserve partial report", stderr.getvalue())
                self.assertTrue((output / "report.md").is_file())
                with fixtures.Workspace(work):
                    pass
