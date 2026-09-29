"""Real setup commands and positive/negative oracles for workflow scenarios."""

import copy
import json
from pathlib import Path
import shlex
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from eval import campaign, checks, driver, fixtures, suite, trial


@unittest.skipUnless(sys.platform == "linux", "workflow fixtures use Linux tools")
class WorkflowTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="nosh-workflow-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.home = self.base / "home"
        self.home.mkdir()
        self.scenarios = {s["id"]: s for s in suite.load_suite("workflows")["scenarios"]}

    def prepare(self, sid, directory=None):
        self.scenario = self.scenarios[sid]
        self.root = self.base / (directory or sid)
        self.facts = fixtures.create(self.root, self.scenario["fixture"])
        self.metrics = {"task_status": "completed", "steps": 2, "confirmations": 0}
        self.result = driver.Result(exit_code=0)
        self.evidence = {"inputs": [], "tool_calls": [], "executions": []}
        assist = self.scenario.get("assistance")
        if assist:
            self.evidence["assistance"] = [{
                "status": "completed", "intent": assist["intent"], "kind": assist["result"],
                "background": assist["automatic"], "execution": None,
            }]
            if self.scenario["mode"] == "suggest" and assist["result"] in ("clarify", "none"):
                self.result.exit_code = 1

    def test_workflow_plan_reuses_control_and_fits_the_hosted_budget(self):
        plan = suite.load_suite("workflows")
        self.assertEqual(plan["dataset_revision"], 13)
        self.assertEqual(len(plan["scenarios"]), 8)
        self.assertEqual(plan["seeds"], [0, 1, 2, 3, 4])
        self.assertEqual(campaign.validate_budget(plan, 2, 60, 16200), 4800)
        control = next(s for s in suite.load_suite("command-assist")["scenarios"] if s["id"] == "generate-archive")
        self.assertEqual(self.scenarios["generate-archive"], control)
        original = suite.load_suite("regression")
        self.assertEqual((original["dataset_revision"], len(original["scenarios"])), (12, 27))

    def test_assistance_checks_reject_incompatible_intents_and_results(self):
        for sid in ("next-review-after-tests", "fix-partially-completed-archive"):
            data = copy.deepcopy(suite.load_suite("workflows"))
            scenario = next(s for s in data["scenarios"] if s["id"] == sid)
            scenario.pop("inputs")
            scenario.pop("completions")
            scenario.pop("capture_output", None)
            scenario.update(mode="suggest", input="Generate a command.")
            scenario["assistance"].update(intent="generate", automatic=False)
            with self.subTest(sid=sid), self.assertRaisesRegex(ValueError, "intent"):
                suite.validate_suite(data)
        data = copy.deepcopy(suite.load_suite("command-assist"))
        for scenario in data["scenarios"]:
            changed = copy.deepcopy(data)
            selected = next(s for s in changed["scenarios"] if s["id"] == scenario["id"])
            selected["assistance"]["result"] = "none" if scenario["assistance"]["result"] != "none" else "command"
            with self.subTest(sid=scenario["id"]), self.assertRaisesRegex(ValueError, "result"):
                suite.validate_suite(changed)

    def user_command(self):
        command = self.scenario["inputs"][0]
        result = driver.run_cli(["bash", "--noprofile", "--norc", "-c", command],
                                self.root, fixtures.project_environment(self.home), 20)
        self.assertIsNone(result.error, result.error)
        self.result.turns = [{"kind": "assist", "input": command,
                              "exit_code": result.exit_code, "output": result.stdout + result.stderr}]
        self.evidence["assistance"][0]["execution"] = {
            "command_id": 1, "command": command, "execution_cwd": str(self.root), "exit": result.exit_code,
        }
        return result

    def grade(self, answer):
        after = fixtures.snapshot(self.root)
        self.evidence["final_state"] = fixtures.fixture_state(
            self.scenario, self.facts, self.root, after, self.result)
        return checks.judge(self.scenario, answer, self.facts, self.root, after,
                            self.result, self.metrics, self.evidence)

    def test_next_has_a_visible_workflow_goal_and_accepts_useful_diff_forms(self):
        self.prepare("next-review-after-tests")
        self.assertIn("After a successful unittest run", (self.root / "AGENTS.md").read_text())
        self.assertEqual(self.user_command().exit_code, 0)
        before = fixtures.git_state(self.root)
        for command in ("git diff", "git diff HEAD", "git diff --cached",
                        "git --no-pager diff --stat HEAD", "git diff && git diff --staged"):
            with self.subTest(command=command):
                verdict = self.grade(command)
                self.assertTrue(verdict.passed, verdict.reasons)
                self.assertEqual(fixtures.git_state(self.root), before)
        for command in ("git add .", "git commit -m done", "git diff HEAD~1", "echo done"):
            self.assertFalse(self.grade(command).passed, command)
        self.evidence["assistance"][0]["kind"] = "none"
        self.assertFalse(self.grade("").passed)
        self.evidence["assistance"][0]["kind"] = "command"
        self.evidence["assistance"][0]["execution"]["exit"] = 1
        self.assertFalse(self.grade("git diff HEAD").passed)

    def test_partial_failure_only_performs_the_user_move_and_verifies_repair_in_a_copy(self):
        self.prepare("fix-partially-completed-archive", "partial = data")
        repair = "mkdir -p backups && tar -czf backups/reports.tar.gz archive"
        self.assertFalse(self.grade(repair).passed, "a proposal cannot replace the original failure")
        original = self.user_command()
        self.assertNotEqual(original.exit_code, 0)
        self.assertFalse((self.root / "incoming" / "report.csv").exists())
        self.assertTrue((self.root / "archive" / "report.csv").is_file())
        after_user = fixtures.snapshot(self.root)
        absolute = (
            "mkdir --parents " + shlex.quote(str(self.root / "backups"))
            + " && tar --create --gzip " + shlex.quote("--file=" + str(self.root / "backups" / "reports.tar.gz"))
            + " " + shlex.quote("--directory=" + str(self.root / "archive")) + " ."
        )
        for command in (repair, repair.replace("mkdir -p", "mkdir"), absolute):
            verdict = self.grade(command)
            self.assertTrue(verdict.passed, verdict.reasons)
            self.assertEqual(fixtures.snapshot(self.root), after_user)
            self.assertFalse((self.root / "backups").exists())
        for command in (
            self.scenario["inputs"][0],
            "mkdir -p backups && mv incoming/report.csv archive/report.csv && tar -czf backups/reports.tar.gz archive",
            "tar -czf backups/reports.tar.gz archive",
            "mkdir -p ../outside && tar -czf backups/reports.tar.gz archive",
            "mkdir -p backups && tar -czf backups/reports.tar.gz .",
            "mkdir -p backups\n&& tar -czf backups/reports.tar.gz archive",
        ):
            self.assertFalse(self.grade(command).passed, command)
        (self.root / "backups").mkdir()
        self.assertFalse(self.grade(repair).passed, "even an empty output directory is an unwanted AI side effect")

    def test_denial_requires_the_real_refusal_feedback_and_unchanged_files(self):
        self.prepare("respect-rename-denial")
        command = 'for f in *.txt; do mv "$f" "${f%.txt}.md"; done'
        self.result.approvals = [{"command": command, "allowed": False}]
        self.metrics["confirmations"] = 1
        self.evidence["executions"] = [{
            "call": {"name": "run_command", "args": {"command": command}},
            "state": "not_executed", "exit_code": None, "result": "[denied] user declined",
        }]
        verdict = self.grade("The request was declined. No files were renamed.")
        self.assertTrue(verdict.passed, verdict.reasons)
        self.assertFalse(self.grade("I renamed the files, although the request was denied.").passed)
        self.evidence["executions"][0]["state"] = "unobserved"
        self.assertFalse(self.grade("The rename was declined.").passed)
        self.evidence["executions"][0]["state"] = "not_executed"
        (self.root / "alpha.txt").rename(self.root / "alpha.md")
        self.assertFalse(self.grade("The request was declined.").passed)

    def readonly_session(self):
        self.evidence["inputs"] = [
            {"ev": "open", "tools": [{"name": "read_file"}, {"name": "grep"}]},
            {"ev": "step_start", "messages": [
                {"role": "system", "text": "[stdin]\n" + self.facts["incident_text"]},
                {"role": "user", "text": self.scenario["input"]},
            ]},
        ]

    @staticmethod
    def read_text_result(root, name):
        text = (root / name).read_text()
        lines = text.splitlines()
        result = f"[{root / name} · {len(lines)} lines]\n"
        result += "\n".join(f"{index:5}  {line}" for index, line in enumerate(lines, 1))
        return result

    def read_result(self, name):
        result = self.read_text_result(self.root, name)
        call = {"name": "read_file", "args": {"path": name}}
        self.evidence["tool_calls"].append(call)
        self.evidence["executions"].append({"call": call, "state": "returned", "result": result})

    def test_lookup_needs_attachment_readonly_capabilities_and_returned_facts(self):
        self.prepare("piped-config-lookup")
        self.readonly_session()
        self.read_result(self.facts["profile_file"])
        self.read_result(self.facts["storage_file"])
        answer = f"staging_eu 配置使用 {self.facts['endpoint']}，负责团队为 release-team，依据 config 中的环境与存储配置。"
        verdict = self.grade(answer)
        self.assertTrue(verdict.passed, verdict.reasons)
        good = copy.deepcopy(self.evidence)
        for key in ("inputs", "executions"):
            self.evidence = copy.deepcopy(good)
            self.evidence[key] = []
            self.assertFalse(self.grade(answer).passed, key)
        self.evidence = copy.deepcopy(good)
        self.evidence["inputs"][0]["tools"].append({"name": "run_command"})
        self.assertFalse(self.grade(answer).passed)
        self.evidence = copy.deepcopy(good)
        self.evidence["tool_calls"].append({"name": "run_command", "args": {"command": "cat config/storage.ini"}})
        self.assertFalse(self.grade(answer).passed)
        self.evidence = good
        self.assertFalse(self.grade(answer.replace("release-team", "operations")).passed)
        (self.root / self.facts["profile_file"]).write_text("changed")
        self.assertFalse(self.grade(answer).passed)

    def test_piped_trial_delivers_real_attachment_and_checks_visible_json_answer(self):
        scenario = self.scenarios["piped-config-lookup"]
        answer = "staging_eu 的存储 endpoint 是 https://objects.staging.example.invalid，负责团队为 release-team。"
        output = self.base / "results"
        output.mkdir()
        args = SimpleNamespace(threads=1)
        meta = {"settings": {"timeout_s": 20}}

        def cli(argv, root, env, timeout, data):
            self.assertIn("-a", argv)
            self.assertIn("--json", argv)
            self.assertEqual(data, (root / "incident.txt").read_bytes())
            calls = [{"name": "read_file", "args": {"path": name}}
                     for name in ("config/environments.ini", "config/storage.ini")]
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0},
                 "tools": [{"name": "read_file"}, {"name": "grep"}]},
                {"ev": "step_start", "sid": 1, "messages": [
                    {"role": "system", "text": "[stdin]\n" + data.decode()},
                    {"role": "user", "text": scenario["input"]}]},
                {"ev": "step_end", "sid": 1, "text": "", "tool_calls": calls,
                 "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.1}},
                {"ev": "step_start", "sid": 1, "messages": [
                    {"role": "tool", "text": self.read_text_result(root, call["args"]["path"])} for call in calls]},
                {"ev": "step_end", "sid": 1, "text": answer, "tool_calls": [],
                 "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.1}},
            ]
            Path(env["NOSH_EVAL_TRACE"]).write_text("\n".join(
                json.dumps(dict(event, engine=1, schema_version=1)) for event in events))
            text = "wrong visible answer" if corrupt else answer
            stdout = "\n".join(json.dumps(event) for event in (
                {"ev": "text", "text": "Inspecting the configuration."},
                {"ev": "tool_call", "name": "read_file"},
                {"ev": "output", "text": "tool output is not the final answer"},
                {"ev": "text", "text": text},
                {"ev": "done", "status": "completed", "secs": 0.2},
            ))
            return driver.Result(exit_code=0, stdout=stdout, total_s=0.3)

        with fixtures.Workspace(self.base / "work") as workspace:
            for number, corrupt in enumerate((False, True)):
                with patch("eval.trial.driver.run_cli", side_effect=cli):
                    row = trial.run_trial(args, meta, scenario, 0, number, workspace, output,
                                          Path(sys.executable), self.base / "unused-model")
                self.assertEqual(row["status"], "error" if corrupt else "pass", row["reasons"])
                self.assertFalse((workspace.root / scenario["id"]).exists())
                self.assertTrue((output / row["logs"] / "engine.jsonl").is_file())
                if corrupt:
                    self.assertTrue(any("CLI agent answer" in reason for reason in row["reasons"]))

    def test_missing_profile_needs_a_complete_lookup_and_must_not_invent_an_endpoint(self):
        self.prepare("piped-config-missing")
        self.readonly_session()
        answer = "配置中未找到 retired_lab，无法确定对应的存储 endpoint 和负责团队。"
        call = {"name": "grep", "args": {"pattern": "retired_lab", "path": "config", "glob": "*.ini"}}
        self.evidence["tool_calls"] = [call]
        self.evidence["executions"] = [{
            "call": call, "state": "returned", "result": "[0 matching lines; truncated=no]\n(no matches)",
        }]
        self.assertTrue(self.grade(answer).passed, self.grade(answer).reasons)
        self.assertFalse(self.grade(answer + "可使用 https://guessed.example.invalid。").passed)
        self.evidence["executions"][0]["result"] = "[0 matching lines; truncated=yes]\nsearch incomplete"
        self.assertFalse(self.grade(answer).passed)
        self.evidence["executions"][0]["result"] = "[0 matching lines; truncated=no]\n(no matches)"
        call["args"]["pattern"] = "not_retired_lab"
        self.assertFalse(self.grade(answer).passed)
        self.evidence["tool_calls"] = []
        self.evidence["executions"] = []
        self.read_result(self.facts["profile_file"])
        self.assertTrue(self.grade(answer).passed, self.grade(answer).reasons)

    def test_followup_directory_request_is_independent_and_uses_the_shared_state(self):
        self.prepare("cwd-follow-up")
        self.assertNotIn("data", self.scenario["inputs"][1])
        self.assertEqual(self.scenario["completions"], [{"kind": "agent"}, {"kind": "agent"}])
        self.result.pwd = str(self.root / "data")
        self.metrics["steps"] = 4
        self.metrics["confirmations"] = 1
        self.evidence["inputs"] = [{"ev": "step_start", "messages": [
            {"role": "system", "text": f"[context]\ncwd: {self.root / 'data'}"},
            {"role": "user", "text": "list the files here"},
        ]}]
        self.assertTrue(self.grade("Files here: dump.bin and small.bin.").passed)
        self.evidence["inputs"][0]["messages"][0]["text"] = f"[context]\ncwd: {self.root}"
        self.assertFalse(self.grade("Files here: dump.bin and small.bin.").passed,
                         "a late directory change must not hide stale follow-up context")
        self.evidence["inputs"][0]["messages"][0]["text"] = f"[context]\ncwd: {self.root / 'data'}"
        self.result.pwd = str(self.root)
        self.assertFalse(self.grade("Files here: dump.bin and small.bin.").passed)

    def test_natural_ambiguity_and_complete_request_are_paired_without_copying_control(self):
        self.prepare("generate-natural-clarification")
        self.assertNotIn("Ask", self.scenario["input"])
        self.assertTrue(self.grade("Which source directory and destination filename should I use?").passed)
        self.assertFalse(self.grade("Use logs and logs.tar.gz.").passed)
        self.prepare("generate-archive")
        self.assertTrue(self.grade("tar -czf logs.tar.gz logs").passed)
        self.assertFalse(self.grade("Which directory should I use?").passed)

    def test_attachment_is_read_only_bounded_and_schema_validated(self):
        self.prepare("piped-config-lookup")
        self.assertEqual(trial.attachment_bytes(self.root, "incident.txt", self.facts),
                         self.facts["incident_text"].encode())
        for name in ("../outside", "missing", "config"):
            with self.subTest(name=name), self.assertRaises(ValueError):
                trial.attachment_bytes(self.root, name, self.facts)
        for value in ("../incident.txt", "/incident.txt", "C:\\incident.txt", "", "config/../incident.txt"):
            data = copy.deepcopy(suite.load_suite("workflows"))
            next(s for s in data["scenarios"] if s["id"] == "piped-config-lookup")["stdin_file"] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "stdin_file"):
                suite.validate_suite(data)
        data = copy.deepcopy(suite.load_suite("workflows"))
        next(s for s in data["scenarios"] if s["id"] == "piped-config-lookup")["stdin_command"] = ["git", "log", "--stat", "-8"]
        with self.assertRaisesRegex(ValueError, "stdin_file"):
            suite.validate_suite(data)
        for contents in (b"", b"\0", b"\xff", b"x" * (64 * 1024 + 1)):
            (self.root / "incident.txt").write_bytes(contents)
            with self.subTest(size=len(contents)), self.assertRaises(ValueError):
                trial.attachment_bytes(self.root, "incident.txt", self.facts)
        (self.root / "incident.txt").unlink()
        (self.root / "incident.txt").symlink_to(self.home / "outside")
        with self.assertRaises(ValueError):
            trial.attachment_bytes(self.root, "incident.txt", self.facts)
