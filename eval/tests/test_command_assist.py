import copy
import json
from pathlib import Path
import tempfile
import unittest

from eval import checks, driver, observations, report, runtime, suite


class CommandAssistTests(unittest.TestCase):
    def setUp(self):
        self.suite = suite.load_suite(runtime.HERE / "suites" / "command-assist.json")
        self.scenarios = {s["id"]: s for s in self.suite["scenarios"]}

    def events(self, intent="next", kind="none", text=None, status="completed", *, raw=None):
        background = intent != "generate"
        value = {
            "workflow": "command_assist", "intent": intent, "background": background,
            "command_id": 1 if background else None, "status": status,
            "response_format": "command_or_none",
        }
        if status == "completed":
            value.update(kind=kind, text=text)
        else:
            value["error"] = status
        messages = [{"role": "user", "text": "generate a command"}]
        if background:
            messages = [{"role": "system", "text": "[execution]\n" + json.dumps({
                "command_id": 1, "exit": 0 if intent == "next" else 7,
            })}]
        return [
            {"ev": "engine", "info": {"load_s": 0.1}},
            {"ev": "open", "sid": 1, "label": f"command_assist.{intent}.{'background' if background else 'foreground'}",
             "sampling": {"seed": 0}},
            {"ev": "step_start", "sid": 1, "messages": messages},
            {"ev": "step_end", "sid": 1,
             "text": raw if raw is not None else "[None]" if kind == "none" else text,
             "think": "", "tool_calls": [], "errors": [],
             "stop": "end_of_turn",
             "usage": {"ttft_s": 0.1, "prompt_tokens": 200, "cached_tokens": 10, "completion_tokens": 15}},
            {"ev": "observation", "sid": 1, "value": value},
            {"ev": "close", "sid": 1},
        ]

    def observe(self, events, scenario=None, result=None):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "trace"
            path.write_text("".join(json.dumps(dict(e, engine=1, schema_version=1)) + "\n" for e in events))
            return observations.observe(result or driver.Result(exit_code=0),
                                        scenario or self.scenarios["next-no-goal"], path, seed=0)

    def help_execution(self, args=None, **changes):
        args = args if args is not None else {"name": "tar"}
        header = {
            "name": args["name"], "program": "/usr/bin/tar", "executable": "/usr/bin/tar",
            "argument": "--help", "subcommands": [], "exit_code": 0, "signal": None,
            "capture_complete": True, "stdout_bytes": 100, "stderr_bytes": 0,
            "query": args.get("query"),
            "matched_blocks": 1 if args.get("query") is not None else None,
            "excerpt_truncated": False,
        }
        header.update(changes)
        stream = "stderr" if header["stderr_bytes"] else "stdout"
        return {
            "call": {"name": "command_help", "args": args}, "state": "returned",
            "result": "[command_help]\n" + json.dumps(header) + f"\n[{stream}]\nusage",
        }

    def test_suite_covers_three_intents_and_noninteractive_results(self):
        self.assertEqual(self.suite["dataset_revision"], 16)
        self.assertEqual({s["assistance"]["intent"] for s in self.suite["scenarios"]}, {"generate", "fix", "next"})
        self.assertEqual({s["assistance"]["result"] for s in self.suite["scenarios"]}, {"command", "none"})
        self.assertTrue(self.scenarios["generate-query-help"]["assistance"]["require_query"])

    def test_only_host_accepted_result_counts_and_final_is_not_execution(self):
        observed = self.observe(self.events())
        self.assertEqual(observed["answer"], "")
        self.assertEqual(observed["assistance"][0]["kind"], "none")
        self.assertEqual(observed["executions"], [])
        self.assertEqual(observed["metrics"]["task_status"], "completed")
        self.assertEqual(observed["metrics"]["prompt_tokens"], 200)
        self.assertEqual(observed["metrics"]["cached_tokens"], 10)
        with self.assertRaisesRegex(ValueError, "no host result"):
            self.observe([e for e in self.events() if e["ev"] != "observation"])

    def test_compact_execution_cwd_is_resolved_without_changing_raw_input(self):
        for cwd in ("/work/project", '/work/name "quoted"', "/work/path\nwith newline"):
            events = self.events()
            execution = {"command_id": 1, "command": "true", "execution_cwd": ".",
                         "exit": 0, "status": "succeeded"}
            background = "[context]\ncwd: " + json.dumps(cwd) + "\n[execution]\n" + json.dumps(execution)
            events[2]["messages"][0]["text"] = background
            observed = self.observe(events)
            self.assertEqual(observed["assistance"][0]["execution"]["execution_cwd"], cwd)
            self.assertEqual(observed["inputs"][1]["messages"][0]["text"], background)
        events = self.events()
        events[2]["messages"][0]["text"] = (
            '[context]\ncwd: /work/now\n[execution]\n'
            '{"command_id":1,"execution_cwd":"/work/before","exit":0}'
        )
        self.assertEqual(self.observe(events)["assistance"][0]["execution"]["execution_cwd"],
                         "/work/before")
        events = self.events(intent="fix", kind="command", text="echo ok")
        events[2]["messages"][0]["text"] = (
            '[context]\ncwd: /work/project\n[execution]\n'
            '{"command_id":1,"execution_cwd":".","exit":7,"status":"failed"}'
        )
        observed = self.observe(events, self.scenarios["auto-fix-archive"])
        self.assertEqual(observed["assistance"][0]["execution"]["execution_cwd"], "/work/project")
        self.assertEqual(observed["assistance"][0]["execution"]["status"], "failed")

    def test_relative_execution_cwd_and_status_must_have_consistent_evidence(self):
        for prefix in (
            "",
            "[context]\ncwd: relative\n",
            '[context]\ncwd: ""\n',
            '[context]\nproject: none\n[README reference "README.md"]\ncwd: /invented\n',
            "[context]\ncwd: ../outside\n",
        ):
            events = self.events()
            events[2]["messages"][0]["text"] = (
                prefix + '[execution]\n{"command_id":1,"execution_cwd":".","exit":0}'
            )
            with self.subTest(prefix=prefix), self.assertRaisesRegex(ValueError, "absolute context cwd"):
                self.observe(events)
        for status in ("failed", "unknown", None, True):
            events = self.events()
            events[2]["messages"][0]["text"] = "[execution]\n" + json.dumps({
                "command_id": 1, "exit": 0, "status": status,
            })
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "contradicts"):
                self.observe(events)

    def test_regression_suggestion_requires_host_acceptance_too(self):
        events = self.events(intent="generate", kind="command", text="echo ok")
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

    def test_success_must_match_the_generated_final_response(self):
        for change in ("text", "kind", "missing", "duplicate"):
            events = self.events(kind="command", text="echo ok")
            if change == "text":
                events[-2]["value"]["text"] = "echo wrong"
            elif change == "kind":
                events[-2]["value"].update(kind="clarify")
            elif change == "missing":
                events[3]["text"] = ""
            else:
                events.insert(-1, copy.deepcopy(events[-2]))
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.observe(events)

    def test_direct_final_result_matches_the_entire_normal_reply(self):
        for raw, kind, text in (
            (" \n[None]\t", "none", None),
            ("echo ok", "command", "echo ok"),
            (" \necho ok\n", "command", "echo ok"),
        ):
            with self.subTest(raw=raw):
                observed = self.observe(self.events(raw=raw, kind=kind, text=text))
                self.assertEqual(observed["answer"], text or "")
                self.assertEqual(observed["assistance"][0]["response_format"], "command_or_none")
                self.assertEqual(observed["executions"], [])

    def test_direct_final_rejects_extracted_text_or_implicit_none(self):
        for raw, kind, text in (
            ("", "none", None),
            ("NONE", "none", None),
            ("[none]", "none", None),
            ("[None] explanation", "none", None),
            ("Here is your command:\necho ok", "command", "echo ok"),
            ("echo wrong", "command", "echo ok"),
            ("[None]", "command", "[None]"),
            ("Which directory?", "clarify", "Which directory?"),
        ):
            with self.subTest(raw=raw), self.assertRaises(ValueError):
                self.observe(self.events(raw=raw, kind=kind, text=text))

    def test_direct_final_requires_normal_end_without_calls_or_errors(self):
        for change in ("max_tokens", "cancelled", "error", "call", "old_finish"):
            events = self.events(kind="command", text="echo ok")
            if change in ("max_tokens", "cancelled"):
                events[3]["stop"] = change
            elif change == "error":
                events[3]["errors"] = [{"kind": "malformed", "message": "invalid call"}]
            else:
                events[3]["tool_calls"] = [{
                    "name": "finish" if change == "old_finish" else "read_file",
                    "args": {"kind": "command", "text": "echo ok"} if change == "old_finish" else {"path": "file"},
                }]
            with self.subTest(change=change), self.assertRaises(ValueError):
                self.observe(events)
        events = self.events()
        events[-2]["value"].pop("response_format")
        with self.assertRaisesRegex(ValueError, "response format"):
            self.observe(events)

    def test_assistance_response_format_is_explicit_and_generate_accepts_direct_final(self):
        for value in (None, True, {}, "unknown", "finish"):
            events = self.events()
            events[-2]["value"]["response_format"] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "response format"):
                self.observe(events)
        events = self.events(intent="generate", kind="command", text="echo ok")
        observed = self.observe(events, self.scenarios["generate-archive"],
                                driver.Result(exit_code=0, stdout="echo ok\n"))
        self.assertEqual(observed["answer"], "echo ok")
        events = self.events(raw="unfinished", kind="command", text="echo ok")
        events[-2]["value"].update(status="cancelled", error="superseded")
        events[-2]["value"].pop("kind")
        events[-2]["value"].pop("text")
        observed = self.observe(events)
        self.assertEqual(observed["metrics"]["task_status"], "cancelled")
        self.assertEqual(observed["answer"], "")

    def test_cli_stdout_is_checked_instead_of_replaced_by_the_trace(self):
        events = self.events(intent="generate", kind="command", text="echo ok")
        scenario = self.scenarios["generate-archive"]
        for stdout in ("echo wrong\n", "Here is your command:\necho ok\n", ""):
            with self.subTest(stdout=stdout), self.assertRaisesRegex(ValueError, "CLI stdout"):
                self.observe(events, scenario, driver.Result(exit_code=0, stdout=stdout))
        self.assertEqual(self.observe(events, scenario, driver.Result(exit_code=0, stdout="echo ok\n"))["answer"],
                         "echo ok")

    def test_question_answer_remains_tool_evidence_before_generate_direct_final(self):
        events = self.events(intent="generate", kind="command", text="echo ok")
        call = {"name": "ask_user", "args": {"question": "Which format?", "choices": ["tar.gz", "zip"]}}
        events[3].update(text="", tool_calls=[call])
        ending = copy.deepcopy(events[3])
        ending.update(text="echo ok", tool_calls=[])
        events[4:4] = [{"ev": "step_start", "sid": 1,
                        "messages": [{"role": "tool", "text": "custom format"}]}, ending]
        observed = self.observe(events, self.scenarios["generate-archive"],
                                driver.Result(exit_code=0, stdout="echo ok\n"))
        self.assertEqual(observed["metrics"]["steps"], 2)
        self.assertEqual(observed["answer"], "echo ok")
        self.assertEqual(observed["executions"][0]["call"], call)
        self.assertEqual(observed["executions"][0]["state"], "returned")
        self.assertEqual(observed["executions"][0]["result"], "custom format")

    def test_noninteractive_missing_inputs_require_no_command_or_prose(self):
        scenario = self.scenarios["generate-clarify"]
        self.assertEqual(scenario["check"], "assist-none")
        self.assertEqual(scenario["assistance"]["result"], "none")
        events = self.events(intent="generate", kind="none")
        observed = self.observe(events, scenario, driver.Result(exit_code=1))
        observed["metrics"]["confirmations"] = 0
        with tempfile.TemporaryDirectory() as temporary:
            verdict = checks.judge(scenario, "", {"before": {}}, Path(temporary), {},
                                   driver.Result(exit_code=1), observed["metrics"], observed)
            self.assertTrue(verdict.passed, verdict.reasons)

    def test_none_does_not_emit_cli_commands(self):
        events = self.events(intent="generate")
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

    def test_superseded_response_is_cancellation_not_an_accepted_command(self):
        events = self.events(kind="command", text="echo stale", status="cancelled")
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
            invalid["tool_calls"].append({"name": "exec", "args": {"command": "true"}})
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
        call = {"name": "command_help", "args": {"name": "tar"}}
        evidence["tool_calls"].insert(0, call)
        with tempfile.TemporaryDirectory() as temporary:
            for code in (0, 1, 10):
                evidence["executions"] = [self.help_execution(exit_code=code)]
                verdict = checks.judge(scenario, "", {"before": {}}, Path(temporary), {},
                                       driver.Result(exit_code=0), evidence["metrics"], evidence)
                self.assertEqual(verdict.passed, code == 0, verdict.reasons)

    def test_help_query_must_be_for_the_requested_program(self):
        scenario = copy.deepcopy(self.scenarios["next-no-goal"])
        scenario["assistance"]["require_query"] = True
        evidence = self.observe(self.events())
        evidence["metrics"]["confirmations"] = 0
        with tempfile.TemporaryDirectory() as temporary:
            for name, program, passed in (
                ("tar", "/usr/bin/tar", True), ("/bin/tar", "/bin/tar", True),
                ("ls", "/usr/bin/ls", False), ("tar", "/usr/bin/ls", False),
                ("ls", "/usr/bin/tar", False), ("tar", "/usr/bin/gtar", False),
            ):
                evidence["executions"] = [self.help_execution({"name": name}, program=program)]
                verdict = checks.judge(scenario, "", {"before": {}}, Path(temporary), {},
                                       driver.Result(exit_code=0), evidence["metrics"], evidence)
                self.assertEqual(verdict.passed, passed, (name, program, verdict.reasons))

    def test_structured_help_requires_real_complete_successful_matching_evidence(self):
        scenario = copy.deepcopy(self.scenarios["next-no-goal"])
        scenario["assistance"]["require_query"] = True
        evidence = self.observe(self.events())
        evidence["metrics"]["confirmations"] = 0
        args = {"name": "tar", "query": "-c"}
        source = self.help_execution(args)
        header = json.loads(source["result"].split("\n", 2)[1])
        changes = [
            ({}, True), ({"exit_code": 1}, False), ({"exit_code": None}, False),
            ({"exit_code": False}, False), ({"signal": 15}, False),
            ({"capture_complete": False}, False), ({"matched_blocks": 0}, False),
            ({"matched_blocks": True}, False), ({"query": "-C"}, False),
            ({"name": "ls"}, False), ({"program": "/bin/ls"}, False),
            ({"argument": "-h"}, False), ({"executable": "tar"}, False),
            ({"executable": "/usr/bin/bsdtar"}, True),
            ({"subcommands": ["commit"]}, False), ({"subcommands": []}, True),
            ({"stdout_bytes": 0}, False), ({"stdout_bytes": True}, False),
            ({"excerpt_truncated": True}, True),
            ({"stdout_bytes": 0, "stderr_bytes": 100}, True),
        ]
        with tempfile.TemporaryDirectory() as temporary:
            evidence["tool_calls"] = [source["call"]]
            for change, passed in changes:
                evidence["executions"] = [self.help_execution(args, **change)]
                verdict = checks.judge(scenario, "", {"before": {}}, Path(temporary), {},
                                       driver.Result(exit_code=0), evidence["metrics"], evidence)
                self.assertEqual(verdict.passed, passed, (change, verdict.reasons))
            for body in ("[command_help]\ninvalid\nusage", "[command_help]\n[]\nusage",
                         "[command_help]\n" + json.dumps(header) + "\n[no matching help text]",
                         "[query program=/usr/bin/tar exit=0 truncated=false]\nusage"):
                evidence["executions"] = [dict(source, result=body)]
                verdict = checks.judge(scenario, "", {"before": {}}, Path(temporary), {},
                                       driver.Result(exit_code=0), evidence["metrics"], evidence)
                self.assertFalse(verdict.passed, body)

    def test_help_query_is_search_text_not_an_action_or_a_topic_alias(self):
        from eval.checks.command_assist import successful_tar_help
        for query in (None, "gzip", "-C", "version"):
            args = {"name": "tar"}
            if query is not None:
                args["query"] = query
            item = self.help_execution(args)
            self.assertTrue(successful_tar_help(item))
            item["call"]["args"]["topic"] = "gzip"
            self.assertFalse(successful_tar_help(item))

    def test_incomplete_observations_cannot_carry_accepted_results(self):
        for status in ("failed", "cancelled"):
            events = self.events(status=status)
            events[-2]["value"].update(kind="command", text="echo unaccepted")
            with self.subTest(status=status), self.assertRaisesRegex(ValueError, "accepted result"):
                self.observe(events)
