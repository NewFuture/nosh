from __future__ import annotations
import json
from pathlib import Path
import tempfile
import unittest
from eval import driver, observations
from .support import SCENARIOS, execution


class ObservationTests(unittest.TestCase):
    def test_native_suggestion_observes_actual_usage_not_stdout_latency(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 1.5}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 4}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "input"}]},
                {"ev": "step_end", "sid": 1, "text": "", "tool_calls": [],
                 "usage": {"ttft_s": 0.125}},
            ]
            trace.write_text("\n".join(json.dumps(dict(e, schema_version=1)) for e in events))
            result = driver.Result(stdout="tar -czf logs.tar.gz logs\n", exit_code=0, total_s=9)
            scenario = {"mode": "suggest", "check": "archive"}
            observed = observations.observe(result, scenario, trace, False, 4)
            self.assertEqual(observed["metrics"]["ttft_s"], 0.125)
            self.assertEqual(observed["metrics"]["total_s"], 9)
            self.assertEqual(observed["metrics"]["load_s"], 1.5)
            self.assertEqual(observed["answer"], result.stdout.strip())
            legacy = observations.observe(result, scenario, trace, True, 4)
            self.assertIsNone(legacy["metrics"]["ttft_s"])
            self.assertIsNone(legacy["inputs"])
            with self.assertRaises(ValueError):
                observations.observe(result, scenario, trace, False, 5)
            trace.write_text(json.dumps({"schema_version": 1, "ev": "step_error", "error": "context full"}) + "\n")
            with self.assertRaisesRegex(RuntimeError, "context full"):
                observations.observe(result, scenario, trace, False, 4)
            trace.unlink()
            with self.assertRaises(ValueError):
                observations.observe(result, scenario, trace, False, 4)

    def test_legacy_answer_excludes_echo_tools_and_intermediate_answers(self):
        text = (
            "__NOSH_EVAL_PROMPT__ # expected.py\n"
            "┃ Let me inspect.\n┃ ⚙ list_dir SAFE\n┃   expected.py\n┃   exit 0\n"
            "┃ Actual final answer.\n┃ ✔ 2 steps · 1.0 s\n┃ stats: ttft 0.12s\n"
        )
        self.assertEqual(observations.legacy_answer(text), "Actual final answer.")
        denied = ("┃ I will change it.\n┃ ╭─ run_command · MUTATING\n┃ │ $ mv a b\n"
                  "┃ ╰─ [y] run [n] deny › n\n┃ reason (optional, Enter to skip):\n"
                  "┃ I did not change it.\n┃ ⚠ 2 steps · 1.0 s\n")
        self.assertEqual(observations.legacy_answer(denied), "I did not change it.")

    def test_ascii_terminal_answers_and_metrics(self):
        text = (
            "| Let me inspect.\n| * list_dir  SAFE\n|   file.py\n"
            "| Actual final answer.\n| * a Markdown bullet\n| + another bullet\n| > a quote\n"
            "| + 2 steps | 1.0 s\n| stats: ttft 0.12s\n"
        )
        self.assertEqual(observations.legacy_answer(text), "Actual final answer.\n* a Markdown bullet\n+ another bullet\n> a quote")
        result = driver.Result(transcript=text)
        observed = observations.observe(result, {"mode": "repl", "check": "largest"}, Path("unused"), True, 0)
        self.assertEqual(observed["metrics"]["steps"], 2)
        self.assertEqual(observed["metrics"]["ttft_s"], 0.12)
        self.assertEqual(observed["metrics"]["task_status"], "completed")
        denied = ("| I will change it.\n| +- run_command - MUTATING\n| | $ mv a b\n"
                  "| +- [y] run [n] deny > n\n| reason (optional, Enter to skip):\n"
                  "| I did not change it.\n| ! 2 steps | 1.0 s\n")
        self.assertEqual(observations.legacy_answer(denied), "I did not change it.")

    def test_ascii_tool_headers_require_a_complete_risk_label(self):
        for bullet in ("* item  with details", "* item  SAFETY first", "* item  safe"):
            with self.subTest(bullet=bullet):
                text = f"| Previous answer.\n| {bullet}\n| Final answer.\n"
                self.assertEqual(observations.legacy_answer(text), f"Previous answer.\n{bullet}\nFinal answer.")
        for risk in ("SAFE", "MUTATING", "DANGEROUS", "FORBIDDEN"):
            for separator in (" · ", " | "):
                with self.subTest(risk=risk, separator=separator):
                    text = f"| Inspecting.\n| * run_command  {risk}{separator}auto\n|   details\n| Final answer.\n"
                    self.assertEqual(observations.legacy_answer(text), "Final answer.")

    def test_legacy_answer_excludes_multiline_proposal_explanations(self):
        for bar, arrow in (("┃", "↳"), ("|", "->")):
            with self.subTest(bar=bar):
                text = (
                    f"{bar} {arrow} printf hello\n"
                    f"{bar}   Suggested explanation.\n{bar}   More detail.\n"
                    f"{bar} Final answer.\n"
                )
                self.assertEqual(observations.legacy_answer(text), "Final answer.")

    def test_execution_results_are_correlated_by_engine_and_session(self):
        events = [
            {"ev": "step_end", "engine": 1, "sid": 1, "tool_calls": [execution("cargo build")["call"]]},
            {"ev": "step_end", "engine": 2, "sid": 1, "tool_calls": [execution("npm test")["call"]]},
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

    def test_multiple_agent_turns_do_not_hide_an_incomplete_task(self):
        text = ("| + 2 steps | 0.1 s\n| stats: ttft 0.01s\n"
                "| ! 3 steps | 0.2 s\n| stats: ttft 0.02s\n")
        observed = observations.observe(driver.Result(transcript=text), SCENARIOS["zh-rust-build"], Path("unused"), True, 0)
        self.assertEqual(observed["metrics"]["steps"], 5)
        self.assertEqual(observed["metrics"]["task_status"], "incomplete")

    def test_only_proven_inflight_model_timeout_is_a_task_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "engine.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
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
            observed = observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, deadline_timeout=True)
            self.assertEqual(observed["metrics"]["steps"], 2)
            self.assertEqual(observed["metrics"]["ttft_s"], 0.2)
            self.assertEqual(observed["metrics"]["task_status"], "timed_out")
            self.assertEqual(observed["answer"], "")
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0)
            for invalid in (events[:-1], events + [{"ev": "close", "sid": 1}], events + [events[-1]]):
                write(invalid)
                with self.assertRaises(ValueError):
                    observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, deadline_timeout=True)
            write(events[:3])
            observed = observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, deadline_timeout=True)
            self.assertIsNone(observed["metrics"]["ttft_s"])
            result.timeout_phase = "initial_prompt"
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, deadline_timeout=True)
            result.timeout_phase = "agent"
            result.exit_code = 0
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, deadline_timeout=True)

    def test_completed_final_generation_at_deadline_is_a_task_failure_with_evidence(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "engine.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "task"}]},
                {"ev": "step_end", "sid": 1, "text": "final answer", "tool_calls": [],
                 "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.2}},
            ]
            trace.write_text("\n".join(
                json.dumps(dict(event, schema_version=1, engine=1)) for event in events))
            result = driver.Result(exit_code=-9, error="deadline", timeout_phase="agent", total_s=60)
            observed = observations.observe(
                result, SCENARIOS["zh-node-test"], trace, False, 0, deadline_timeout=True)
            self.assertEqual(observed["deadline_state"], "after_generation")
            self.assertEqual(observed["metrics"]["task_status"], "timed_out")
            self.assertEqual(observed["answer"], "final answer")
            self.assertTrue(any("before the REPL completion marker" in note
                                for note in observed["metric_notes"]))
            for change in (
                {"stop": "max_tokens"},
                {"tool_calls": [{"name": "run_command", "args": {"command": "true"}}]},
                {"errors": ["bad call"]},
            ):
                broken = [dict(event) for event in events]
                broken[-1].update(change)
                trace.write_text("\n".join(
                    json.dumps(dict(event, schema_version=1, engine=1)) for event in broken))
                with self.subTest(change=change), self.assertRaises(ValueError):
                    observations.observe(
                        result, SCENARIOS["zh-node-test"], trace, False, 0,
                        deadline_timeout=True)
