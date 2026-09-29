import copy
import json
from pathlib import Path
import tempfile
import unittest

from eval import checks, driver, observations, report, run, suite


class CommandAssistTests(unittest.TestCase):
    def setUp(self):
        self.suite = suite.load_suite(run.HERE / "suites" / "command-assist.json")
        self.scenarios = {s["id"]: s for s in self.suite["scenarios"]}

    def events(self, intent="next", kind="none", text=None, status="completed"):
        args = {"kind": kind}
        if text is not None:
            args["text"] = text
        return [
            {"ev": "engine", "info": {"load_s": 0.1}},
            {"ev": "open", "sid": 1, "label": f"command_assist.{intent}.background",
             "sampling": {"seed": 0}},
            {"ev": "step_start", "sid": 1, "messages": [
                {"role": "system", "text": "[execution]\n{\"command_id\":1,\"exit\":0}"}]},
            {"ev": "step_end", "sid": 1, "text": "", "think": "",
             "tool_calls": [{"name": "finish", "args": args}], "errors": [],
             "stop": "end_of_turn",
             "usage": {"ttft_s": 0.1, "prompt_tokens": 200, "cached_tokens": 10, "completion_tokens": 15}},
            {"ev": "observation", "sid": 1, "value": {
                "workflow": "command_assist", "intent": intent, "background": True,
                "command_id": 1, "status": status, "kind": kind, "text": text}},
            {"ev": "close", "sid": 1},
        ]

    def observe(self, events, scenario=None, result=None):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "trace"
            path.write_text("".join(json.dumps(dict(e, engine=1, schema_version=1)) + "\n" for e in events))
            return observations.observe(result or driver.Result(exit_code=0),
                                        scenario or self.scenarios["next-no-goal"], path, False, 0)

    def test_suite_covers_three_intents_and_three_results(self):
        self.assertEqual(self.suite["dataset_revision"], 10)
        self.assertEqual({s["assistance"]["intent"] for s in self.suite["scenarios"]}, {"generate", "fix", "next"})
        self.assertEqual({s["assistance"]["result"] for s in self.suite["scenarios"]}, {"command", "clarify", "none"})
        self.assertTrue(self.scenarios["generate-query-help"]["assistance"]["require_query"])

    def test_only_host_accepted_result_counts_and_finish_is_not_execution(self):
        observed = self.observe(self.events())
        self.assertEqual(observed["answer"], "")
        self.assertEqual(observed["assistance"][0]["kind"], "none")
        self.assertEqual(observed["executions"], [])
        self.assertEqual(observed["metrics"]["task_status"], "completed")
        self.assertEqual(observed["metrics"]["prompt_tokens"], 200)
        self.assertEqual(observed["metrics"]["cached_tokens"], 10)
        with self.assertRaisesRegex(ValueError, "no host result"):
            self.observe([e for e in self.events() if e["ev"] != "observation"])

    def test_regression_suggestion_requires_host_acceptance_too(self):
        events = self.events(intent="generate", kind="command", text="echo ok")
        events[1]["label"] = "command_assist.generate.foreground"
        events = [event for event in events if event["ev"] != "observation"]
        with self.assertRaisesRegex(ValueError, "no host result"):
            self.observe(events, {"mode": "suggest", "check": "archive"},
                         driver.Result(exit_code=0, stdout="echo ok\n"))

    def test_wrong_identity_and_malformed_success_are_rejected(self):
        for field, value in [("kind", "other"), ("text", "unexpected"), ("intent", "fix"),
                             ("background", False), ("command_id", 2), ("command_id", True)]:
            events = self.events()
            events[-2]["value"][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                self.observe(events)

    def test_success_must_match_a_single_generated_finish(self):
        for change in ("text", "kind", "missing", "duplicate"):
            events = self.events(kind="command", text="echo ok")
            if change == "text":
                events[-2]["value"]["text"] = "echo wrong"
            elif change == "kind":
                events[-2]["value"].update(kind="clarify")
            elif change == "missing":
                events[3]["tool_calls"] = []
            else:
                events.insert(-1, copy.deepcopy(events[-2]))
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.observe(events)

    def test_cli_stdout_is_checked_instead_of_replaced_by_the_trace(self):
        events = self.events(intent="generate", kind="command", text="echo ok")
        events[1]["label"] = "command_assist.generate.foreground"
        events[-2]["value"].update(background=False, command_id=None)
        scenario = self.scenarios["generate-archive"]
        for stdout in ("echo wrong\n", "Here is your command:\necho ok\n", ""):
            with self.subTest(stdout=stdout), self.assertRaisesRegex(ValueError, "CLI stdout"):
                self.observe(events, scenario, driver.Result(exit_code=0, stdout=stdout))
        self.assertEqual(self.observe(events, scenario, driver.Result(exit_code=0, stdout="echo ok\n"))["answer"],
                         "echo ok")

    def test_clarification_and_none_do_not_emit_cli_commands(self):
        for kind, text in (("none", None), ("clarify", "Which directory?")):
            events = self.events(intent="generate", kind=kind, text=text)
            events[1]["label"] = "command_assist.generate.foreground"
            events[-2]["value"].update(background=False, command_id=None)
            scenario = self.scenarios["generate-clarify"]
            with self.assertRaisesRegex(ValueError, "CLI stdout"):
                self.observe(events, scenario, driver.Result(exit_code=1, stdout="echo unwanted"))
            self.observe(events, scenario, driver.Result(exit_code=1, stdout=""))

    def test_model_failure_is_not_no_suggestion(self):
        observed = self.observe(self.events(status="failed"))
        self.assertEqual(observed["metrics"]["task_status"], "failed")
        with tempfile.TemporaryDirectory() as temporary:
            verdict = checks.judge(self.scenarios["next-no-goal"], "", {"before": {}}, Path(temporary),
                                   {}, driver.Result(exit_code=0), observed["metrics"], observed)
            self.assertFalse(verdict.passed)

    def test_superseded_finish_is_cancellation_not_an_accepted_command(self):
        events = self.events(kind="command", text="echo stale", status="cancelled")
        events[-2]["value"].pop("kind")
        events[-2]["value"].pop("text")
        events[-2]["value"]["error"] = "command assistance cancelled"
        observed = self.observe(events)
        self.assertEqual(observed["metrics"]["task_status"], "cancelled")
        self.assertEqual(observed["answer"], "")
        with tempfile.TemporaryDirectory() as temporary:
            verdict = checks.judge(self.scenarios["next-no-goal"], "", {"before": {}},
                                   Path(temporary), {}, driver.Result(exit_code=0),
                                   observed["metrics"], observed)
            self.assertFalse(verdict.passed)

    def test_no_suggestion_requires_no_side_effects_or_execution_calls(self):
        observed = self.observe(self.events())
        scenario = self.scenarios["next-no-goal"]
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            result = driver.Result(exit_code=0)
            observed["metrics"]["confirmations"] = 0
            valid = checks.judge(scenario, "", {"before": {}}, root, {}, result, observed["metrics"], observed)
            self.assertTrue(valid.passed, valid.reasons)
            invalid = copy.deepcopy(observed)
            invalid["tool_calls"].append({"name": "run_command", "args": {"command": "true"}})
            self.assertFalse(checks.judge(scenario, "", {"before": {}}, root, {}, result,
                                          invalid["metrics"], invalid).passed)
            self.assertFalse(checks.judge(scenario, "", {"before": {}}, root, {"new": {}}, result,
                                          observed["metrics"], observed).passed)

    def test_token_aggregation_does_not_treat_missing_cost_as_zero(self):
        metrics = {"steps": 1, "confirmations": 0, "ttft_s": 0.1, "total_s": 1, "peak_rss_mib": 1}
        row = report.summarize([
            {"status": "pass", "metrics": dict(metrics, prompt_tokens=200, cached_tokens=0, completion_tokens=20)},
            {"status": "fail", "metrics": metrics},
        ], 2)
        self.assertEqual(row["prompt_tokens"], 200)
        self.assertEqual(row["prompt_tokens_samples"], 1)

    def test_failed_help_process_does_not_count_as_successful_query(self):
        scenario = copy.deepcopy(self.scenarios["next-no-goal"])
        scenario["assistance"]["require_query"] = True
        evidence = self.observe(self.events())
        evidence["metrics"]["confirmations"] = 0
        call = {"name": "command_info", "args": {"name": "tar", "query": "help"}}
        evidence["tool_calls"].insert(0, call)
        with tempfile.TemporaryDirectory() as temporary:
            for code in (0, 1, 10):
                evidence["executions"] = [{
                    "call": call, "state": "returned",
                    "result": f"[query program=/usr/bin/tar exit={code} truncated=false]\nusage",
                }]
                verdict = checks.judge(scenario, "", {"before": {}}, Path(temporary), {},
                                       driver.Result(exit_code=0), evidence["metrics"], evidence)
                self.assertEqual(verdict.passed, code == 0, verdict.reasons)

    def test_legacy_runs_do_not_send_new_configuration_keys_to_old_binaries(self):
        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary) / "home"
            home.mkdir()
            run.environment(home, 1, None, command_assist=None)
            config = (home / "nosh" / "config.toml").read_text()
            self.assertNotIn("command_assist", config)
            self.assertNotIn("[shell]", config)
