from __future__ import annotations
import copy
import json
from pathlib import Path
import tempfile
import unittest
from eval import driver, observations
from .support import SCENARIOS, execution, tool_outcome


class ObservationTests(unittest.TestCase):
    def test_agent_diagnostics_cannot_be_parsed_as_assistance_metadata(self):
        for diagnostic in ("[execution]\nnot-json", '[execution]\n{"command_id":999,"exit":0}'):
            with self.subTest(diagnostic=diagnostic), tempfile.TemporaryDirectory() as temporary:
                trace = Path(temporary) / "trace.jsonl"
                self.agent_trace(trace)
                events = [json.loads(line) for line in trace.read_text().splitlines()]
                events[1]["label"] = "agent"
                context = "[context]\ncwd: /work\n[user_output {}]\n" + diagnostic + "\n[/user_output]"
                events[2]["messages"].insert(0, {"role": "system", "text": context})
                trace.write_text("\n".join(map(json.dumps, events)))
                result = driver.Result(transcript="| + 1 steps | 0.1 s\n| stats: ttft 0.01s\n")
                observed = observations.observe(result, {"mode": "repl", "check": "largest"}, trace, seed=0)
                self.assertEqual(observed["metrics"]["task_status"], "completed")
                self.assertEqual(observed["assistance"], [])
                self.assertEqual(observed["inputs"][1]["messages"][0]["text"], context)

    def agent_trace(self, path, answer="Actual final answer.", steps=1):
        events = [
            {"ev": "engine", "info": {"load_s": 1.5, "device": "cpu"}},
            {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
        ]
        for number in range(steps):
            events.extend([
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "task"}]},
                {"ev": "step_end", "sid": 1, "text": answer if number == steps - 1 else "intermediate",
                 "tool_calls": [], "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.125}},
            ])
        path.write_text("\n".join(json.dumps(dict(event, schema_version=1, engine=1)) for event in events))

    def test_native_suggestion_observes_actual_usage_not_stdout_latency(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            command = "tar -czf logs.tar.gz logs"
            events = [
                {"ev": "engine", "info": {"load_s": 1.5, "device": "cpu"}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 4}, "label": "command_assist.generate.foreground"},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "input"}]},
                {"ev": "step_end", "sid": 1, "text": command, "errors": [], "stop": "end_of_turn",
                 "tool_calls": [],
                 "usage": {"ttft_s": 0.125}},
                {"ev": "observation", "sid": 1, "value": {
                    "workflow": "command_assist", "intent": "generate", "background": False,
                    "command_id": None, "status": "completed", "kind": "command", "text": command,
                    "response_format": "command_or_none", "input_format": "command_assist_v1"}},
            ]
            trace.write_text("\n".join(json.dumps(dict(e, schema_version=1, engine=1)) for e in events))
            result = driver.Result(stdout="tar -czf logs.tar.gz logs\n", exit_code=0, total_s=9)
            scenario = {"mode": "suggest", "check": "archive"}
            observed = observations.observe(result, scenario, trace, seed=4)
            self.assertEqual(observed["metrics"]["ttft_s"], 0.125)
            self.assertEqual(observed["metrics"]["total_s"], 9)
            self.assertEqual(observed["metrics"]["load_s"], 1.5)
            self.assertEqual(observed["answer"], result.stdout.strip())
            self.assertIsNotNone(observed["inputs"])
            with self.assertRaises(ValueError):
                observations.observe(result, scenario, trace, seed=5)
            for seed in (False, -1, 2**64):
                with self.assertRaisesRegex(ValueError, "u64 seed"):
                    observations.observe(result, scenario, trace, seed=seed)
            events[1].pop("label")
            trace.write_text("\n".join(json.dumps(dict(e, schema_version=1, engine=1)) for e in events[:-1]))
            with self.assertRaisesRegex(ValueError, "no host result"):
                observations.observe(result, scenario, trace, seed=4)
            trace.write_text(json.dumps({"schema_version": 1, "ev": "step_error", "error": "context full"}) + "\n")
            with self.assertRaisesRegex(RuntimeError, "context full"):
                observations.observe(result, scenario, trace, seed=4)
            trace.unlink()
            with self.assertRaises(ValueError):
                observations.observe(result, scenario, trace, seed=4)


    def test_ascii_completion_uses_native_answers_and_usage(self):
        text = (
            "| Let me inspect.\n| * read_file  SAFE\n|   file.py\n"
            "| Actual final answer.\n| * a Markdown bullet\n| + another bullet\n| > a quote\n"
            "| + 2 steps | 1.0 s\n| stats: ttft 99.00s\n"
        )
        result = driver.Result(transcript=text)
        answer = "Native final answer.\n* a Markdown bullet\n+ another bullet\n> a quote"
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            self.agent_trace(trace, answer, steps=2)
            observed = observations.observe(result, {"mode": "repl", "check": "largest"}, trace, seed=0)
        self.assertEqual(observed["answer"], answer)
        self.assertEqual(observed["metrics"]["steps"], 2)
        self.assertEqual(observed["metrics"]["ttft_s"], 0.125)
        self.assertEqual(observed["metrics"]["task_status"], "completed")


    def test_execution_results_are_correlated_by_engine_and_session(self):
        build = execution("cargo build")
        test = execution("npm test")
        events = [
            {"ev": "step_end", "engine": 1, "sid": 1, "tool_calls": [build["call"]]},
            {"ev": "step_end", "engine": 2, "sid": 1, "tool_calls": [test["call"]]},
            tool_outcome(test["call"], "[denied] command was not run", status="denied", engine=2),
            tool_outcome(build["call"], build["result"], status="executed"),
            {"ev": "step_start", "engine": 2, "sid": 1, "messages": [
                {"role": "tool", "text": "[denied] command was not run"}]},
            {"ev": "step_start", "engine": 1, "sid": 1, "messages": [
                {"role": "user", "text": "not a result"},
                {"role": "tool", "text": execution("cargo build")["result"]}]},
        ]
        rows = observations.execution_evidence(events)
        self.assertEqual([r["state"] for r in rows], ["executed", "not_executed"])
        self.assertEqual([r["exit_code"] for r in rows], [0, None])
        self.assertEqual(observations.execution_evidence(events[:1])[0]["state"], "unobserved")
        for bad in (
            {"ev": "step_end", "sid": [], "tool_calls": []},
            {"ev": "step_end", "tool_calls": [None]},
            {"ev": "step_start", "messages": [None]},
        ):
            with self.subTest(event=bad), self.assertRaises(ValueError):
                observations.execution_evidence([bad])

    def test_native_outcomes_do_not_need_another_model_turn_or_a_text_header(self):
        call = execution("cargo build")["call"]
        proposed = {"ev": "step_end", "engine": 1, "sid": 1, "tool_calls": [call]}
        result = tool_outcome(call, "user edited command; arbitrary display text", status="executed",
                              command="cargo test", code=1)
        rows = observations.execution_evidence([proposed, result], require_complete=True)
        self.assertEqual(rows[0]["call"], call)
        self.assertEqual(rows[0]["command"], "cargo test")
        self.assertEqual(rows[0]["exit_code"], 1)
        self.assertEqual(rows[0]["state"], "executed")
        self.assertNotIn("submitted_to_model", rows[0])
        for status, expected in (("denied", "not_executed"), ("execution_error", "unknown")):
            event = tool_outcome(call, execution("cargo build")["result"], status=status)
            row = observations.execution_evidence([proposed, event], require_complete=True)[0]
            self.assertEqual(row["state"], expected)
            self.assertIsNone(row["exit_code"])
        with self.assertRaisesRegex(ValueError, "missing"):
            observations.execution_evidence([proposed], require_complete=True)
        with self.assertRaisesRegex(ValueError, "native Agent"):
            observations.execution_evidence([proposed, {"ev": "step_start", "engine": 1, "sid": 1,
                "messages": [{"role": "tool", "text": execution("cargo build")["result"]}]}])
        for field, value in (
            ("call_index", True), ("call_index", 1), ("turn", 2), ("turn", True), ("version", 2), ("status", []),
            ("status", "read"), ("current_cwd", "relative"), ("result", None),
            ("call", {"name": "exec", "args": {"command": "different"}}),
        ):
            changed = copy.deepcopy(result)
            changed["value"][field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                observations.execution_evidence([proposed, changed])
        with self.assertRaisesRegex(ValueError, "unique"):
            observations.execution_evidence([proposed, result, result])
        with self.assertRaisesRegex(ValueError, "unique"):
            observations.execution_evidence([proposed, result, proposed, result])
        changed = copy.deepcopy(result)
        changed["value"]["execution"]["edited"] = False
        with self.assertRaisesRegex(ValueError, "differs"):
            observations.execution_evidence([proposed, changed])
        for field, value in (("exit_code", True), ("duration_s", -1), ("duration_s", float("nan")),
                             ("truncated", "no"), ("cwd", "relative")):
            changed = copy.deepcopy(result)
            changed["value"]["execution"][field] = value
            with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                observations.execution_evidence([proposed, changed])
        unknown = tool_outcome(call, "effects unknown", status="execution_error")
        unknown["value"]["execution"]["exit_code"] = 0
        with self.assertRaisesRegex(ValueError, "unknown execution"):
            observations.execution_evidence([proposed, unknown])

    def test_completed_tasks_reject_broken_trace_lifecycles(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            self.agent_trace(trace, steps=2)
            events = [json.loads(line) for line in trace.read_text().splitlines()]
            wrong_session = copy.deepcopy(events)
            wrong_session[3]["sid"] = 2
            wrong_engine = copy.deepcopy(events)
            wrong_engine[3]["engine"] = 2
            invalid_identity = copy.deepcopy(events)
            invalid_identity[1]["sid"] = True
            closed = {"ev": "close", "engine": 1, "sid": 1, "schema_version": 1}
            cases = (
                wrong_session, wrong_engine, invalid_identity, events[1:],
                events[:1] + events[2:], events[:2] + [events[1]] + events[2:],
                events[:2] + [events[3], events[2]] + events[4:],
                events[:2] + [events[2], events[4], events[3], events[5]],
                events[:4] + [closed] + events[4:],
                events[:3] + [closed] + events[3:],
            )
            result = driver.Result(transcript="| + 2 steps | 1.0 s\n")
            for index, broken in enumerate(cases):
                trace.write_text("\n".join(json.dumps(event) for event in broken))
                with self.subTest(case=index), self.assertRaises(ValueError):
                    observations.observe(result, SCENARIOS["largest-files"], trace, seed=0)
            trace.write_text("\n".join(json.dumps(event) for event in events + [closed]))
            observed = observations.observe(result, SCENARIOS["largest-files"], trace, seed=0)
            self.assertEqual(observed["metrics"]["steps"], 2)

    def test_proven_context_recovery_preserves_native_results_and_failed_attempts(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "trace.jsonl"
            call = execution("printf result")["call"]
            feedback = "actual result"
            messages = [{"role": "tool", "text": feedback}]
            events = [
                {"ev": "engine", "info": {"load_s": 0, "device": "cpu"}},
                {"ev": "open", "sid": 1, "label": "agent", "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "inspect"}]},
                {"ev": "step_end", "sid": 1, "text": "", "tool_calls": [call],
                 "stop": "end_of_turn", "errors": [], "usage": {"ttft_s": 0.1}},
                tool_outcome(call, feedback, status="executed"),
                {"ev": "step_start", "sid": 1, "messages": messages},
                {"ev": "step_error", "sid": 1, "error": "context is full"},
                {"ev": "rewind", "sid": 1, "keep": 2},
                {"ev": "compact", "sid": 1, "changed": 1, "keep_recent": 0},
                {"ev": "step_start", "sid": 1, "messages": messages},
                {"ev": "step_end", "sid": 1, "text": "done", "tool_calls": [],
                 "stop": "end_of_turn", "errors": [], "usage": {"ttft_s": 0.1}},
            ]
            def write(rows):
                path.write_text("\n".join(json.dumps({"engine": 1, "schema_version": 1, **event}) for event in rows))
            write(events)
            result = driver.Result(exit_code=0, transcript="| + 2 steps | 1.0 s\n")
            observed = observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            self.assertEqual(observed["metrics"]["steps"], 3)
            self.assertFalse(observed["metrics"]["timing_complete"])
            self.assertIsNone(observed["metrics"]["case_other_s"])
            self.assertEqual(observed["executions"][0]["state"], "executed")
            self.assertEqual(observed["engine_recoveries"], [
                {"engine": 1, "sid": 1, "error": "context is full", "status": "completed"},
            ])
            for index, field, value in ((7, "keep", None), (8, "changed", True), (8, "keep_recent", -1)):
                wrong = copy.deepcopy(events)
                wrong[index][field] = value
                write(wrong)
                with self.subTest(field=field), self.assertRaisesRegex(ValueError, "recovery operation"):
                    observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            finalization = events[:9] + [{"ev": "tool_choice", "sid": 1, "choice": "none"}] + events[9:]
            write(finalization)
            observed = observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            self.assertEqual(observed["engine_recoveries"][0]["status"], "completed")
            for removed in (7, 8):
                write([event for index, event in enumerate(events) if index != removed])
                with self.assertRaisesRegex(RuntimeError, "context is full"):
                    observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            for index, field, value in (
                (9, "messages", [{"role": "user", "text": "different task"}]),
                (8, "sid", 2),
                (9, "engine", 2),
            ):
                wrong = copy.deepcopy(events)
                wrong[index][field] = value
                write(wrong)
                with self.subTest(field=field), self.assertRaises(RuntimeError):
                    observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            for error in ("context is full", "engine unavailable"):
                wrong = copy.deepcopy(events[:7])
                wrong[-1]["error"] = error
                write(wrong)
                with self.subTest(error=error), self.assertRaisesRegex(RuntimeError, error):
                    observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            write(events[:8] + [events[10]])
            with self.assertRaises(RuntimeError):
                observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            write(events[:-1])
            timed = driver.Result(exit_code=-9, error="deadline", timeout_phase="agent", total_s=240)
            observed = observations.observe(timed, SCENARIOS["largest-files"], path, seed=0, deadline_timeout=True)
            self.assertEqual(observed["metrics"]["task_status"], "timed_out")
            self.assertEqual(observed["deadline_state"], "during_generation")
            self.assertEqual(observed["engine_recoveries"][0]["status"], "retrying")
            with self.assertRaises(RuntimeError):
                observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            write(events[:-1] + [{"ev": "step_error", "sid": 1, "error": "retry failed"}])
            with self.assertRaisesRegex(RuntimeError, "retry failed"):
                observations.observe(result, SCENARIOS["largest-files"], path, seed=0)

            question = {"name": "ask_user", "args": {"question": "Which target?", "choices": ["debug", "release"]}}
            reply = json.dumps({**question["args"], "answer": "release"})
            continued = copy.deepcopy(events)
            continued[-1].update(text="", tool_calls=[question])
            continued.extend([
                tool_outcome(question, reply, status="answered", turn=2),
                {"ev": "step_start", "sid": 1, "messages": [{"role": "tool", "source": "user", "text": reply}]},
                events[-1],
            ])
            write(continued)
            result = driver.Result(transcript="| + 3 steps | 1.0 s\n", questions=[{
                "engine": 1, "sid": 1, "step": 3, "call": question, "input_index": 0,
                "state": "answered", "answer": "release",
            }])
            observed = observations.observe(result, SCENARIOS["largest-files"], path, seed=0)
            self.assertEqual(observed["metrics"]["steps"], 4)
            self.assertEqual(observed["executions"][1]["native_status"], "answered")
            self.assertTrue(observed["executions"][1]["submitted_to_model"])
            self.assertEqual(observed["questions"][0]["step"], 3)

    def test_interleaved_engines_keep_the_first_started_step_ttft(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            events = [
                {"ev": "engine", "engine": 1, "info": {"load_s": 0.2, "device": "cpu"}},
                {"ev": "engine", "engine": 2, "info": {"load_s": 0.3, "device": "cpu"}},
                {"ev": "open", "engine": 1, "sid": 1, "sampling": {"seed": 0}},
                {"ev": "open", "engine": 2, "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "engine": 1, "sid": 1, "messages": []},
                {"ev": "step_start", "engine": 2, "sid": 1, "messages": []},
                {"ev": "step_end", "engine": 2, "sid": 1, "text": "second", "tool_calls": [],
                 "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.05}},
                {"ev": "step_end", "engine": 1, "sid": 1, "text": "first", "tool_calls": [],
                 "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.7}},
            ]
            def write(rows):
                trace.write_text("\n".join(json.dumps(dict(event, schema_version=1)) for event in rows))
            write(events)
            result = driver.Result(transcript="| + 2 steps | 1.0 s\n")
            observed = observations.observe(result, SCENARIOS["largest-files"], trace, seed=0)
            self.assertEqual(observed["metrics"]["ttft_s"], 0.7)
            self.assertAlmostEqual(observed["metrics"]["load_s"], 0.5)
            write(events[:-1])
            timed_out = driver.Result(exit_code=-9, error="deadline", timeout_phase="agent")
            observed = observations.observe(timed_out, SCENARIOS["largest-files"], trace, seed=0, deadline_timeout=True)
            self.assertIsNone(observed["metrics"]["ttft_s"])

    def test_malformed_cli_completion_is_an_observation_error(self):
        for events in (
            [[]],
            [{"ev": "done", "status": True, "secs": 1}],
            [{"ev": "done", "status": "completed", "secs": 10**400}],
            [{"ev": "done", "status": "completed", "secs": 1}] * 2,
        ):
            result = driver.Result(stdout="\n".join(json.dumps(event) for event in events))
            with self.subTest(events=events), self.assertRaisesRegex(ValueError, "CLI"):
                observations.observe(result, {"mode": "agent", "check": "history"}, Path("unused"), seed=0)

    def test_cli_completion_status_matches_the_producer_enum(self):
        scenario = {"mode": "agent", "check": "history"}
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            self.agent_trace(trace)
            for status, exit_code in (("completed", 0), ("incomplete", 1), ("cancelled", 130), ("failed", 2)):
                result = driver.Result(exit_code=exit_code, stdout="\n".join(json.dumps(event) for event in (
                    {"ev": "text", "text": "Actual final answer."},
                    {"ev": "done", "status": status, "secs": 1},
                )))
                with self.subTest(status=status):
                    observed = observations.observe(result, scenario, trace, seed=0)
                    self.assertEqual(observed["metrics"]["task_status"], status)
            for status in (None, "", " ", "unknown", "Completed", "completed ", "timed_out", 1, [], {}):
                result = driver.Result(stdout=json.dumps({"ev": "done", "status": status, "secs": 1}))
                with self.subTest(status=status), self.assertRaisesRegex(ValueError, "CLI completion"):
                    observations.observe(result, scenario, trace, seed=0)

    def test_multiple_agent_turns_do_not_hide_an_incomplete_task(self):
        text = ("| + 2 steps | 0.1 s\n| stats: ttft 0.01s\n"
                "| ! 3 steps | 0.2 s\n| stats: ttft 0.02s\n")
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            self.agent_trace(trace, steps=5)
            observed = observations.observe(driver.Result(transcript=text), SCENARIOS["zh-rust-build"], trace, seed=0)
        self.assertEqual(observed["metrics"]["steps"], 5)
        self.assertEqual(observed["metrics"]["task_status"], "incomplete")

    def test_only_proven_inflight_model_timeout_is_a_task_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "engine.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 0.1, "device": "cpu"}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "task"}]},
                {"ev": "step_end", "sid": 1, "text": "intermediate, not final", "tool_calls": [],
                 "usage": {"ttft_s": 0.2}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "continue"}]},
            ]
            def write(rows):
                trace.write_text("\n".join(json.dumps(dict(e, schema_version=1, engine=1)) for e in rows))
            result = driver.Result(exit_code=-9, error="deadline", timeout_phase="agent", total_s=240)
            write(events)
            observed = observations.observe(result, SCENARIOS["zh-clean-build"], trace, seed=0, deadline_timeout=True)
            self.assertEqual(observed["metrics"]["steps"], 2)
            self.assertEqual(observed["metrics"]["ttft_s"], 0.2)
            self.assertEqual(observed["metrics"]["task_status"], "timed_out")
            self.assertEqual(observed["answer"], "")
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, seed=0)
            for invalid in (events[:-1], events + [{"ev": "close", "sid": 1}], events + [events[-1]]):
                write(invalid)
                with self.assertRaises(ValueError):
                    observations.observe(result, SCENARIOS["zh-clean-build"], trace, seed=0, deadline_timeout=True)
            write(events[:3])
            observed = observations.observe(result, SCENARIOS["zh-clean-build"], trace, seed=0, deadline_timeout=True)
            self.assertIsNone(observed["metrics"]["ttft_s"])
            result.timeout_phase = "initial_prompt"
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, seed=0, deadline_timeout=True)
            result.timeout_phase = "agent"
            result.exit_code = 0
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, seed=0, deadline_timeout=True)

    def test_completed_final_generation_at_deadline_is_a_task_failure_with_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "engine.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 0.1, "device": "cpu"}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "task"}]},
                {"ev": "step_end", "sid": 1, "text": "final answer", "tool_calls": [],
                 "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.2}},
            ]
            trace.write_text("\n".join(
                json.dumps(dict(event, schema_version=1, engine=1)) for event in events))
            result = driver.Result(exit_code=-9, error="deadline", timeout_phase="agent", total_s=60)
            observed = observations.observe(
                result, SCENARIOS["zh-node-test"], trace, seed=0, deadline_timeout=True)
            self.assertEqual(observed["deadline_state"], "after_generation")
            self.assertEqual(observed["metrics"]["task_status"], "timed_out")
            self.assertEqual(observed["answer"], "final answer")
            self.assertTrue(any("before the REPL completion marker" in note
                                for note in observed["metric_notes"]))
            for change in (
                {"stop": "max_tokens"},
                {"tool_calls": [{"name": "exec", "args": {"command": "true"}}]},
                {"errors": ["bad call"]},
            ):
                broken = [dict(event) for event in events]
                broken[-1].update(change)
                trace.write_text("\n".join(
                    json.dumps(dict(event, schema_version=1, engine=1)) for event in broken))
                with self.subTest(change=change), self.assertRaises(ValueError):
                    observations.observe(
                        result, SCENARIOS["zh-node-test"], trace, seed=0,
                        deadline_timeout=True)
