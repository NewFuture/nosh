from __future__ import annotations

import copy
import json
from pathlib import Path
import sys
import tempfile
import unittest

from eval import checks, driver, fixtures, observations, suite
from .support import SCENARIOS, SUITE


SCRIPT = r'''
import json, os, select, tty
tty.setraw(0)
def out(text):
    os.write(1, text.encode())
def line():
    data = b""
    while not data.endswith(b"\r"):
        byte = os.read(0, 1)
        if byte == b"\x03":
            return None
        if byte == b"\x15":
            data = b""
            continue
        data += byte
    return data[:-1].decode()
def event(kind, **fields):
    row = dict(schema_version=1, engine=1, sid=1, ev=kind, **fields)
    with open(os.environ["NOSH_EVAL_TRACE"], "a", encoding="utf-8") as stream:
        stream.write(json.dumps(row, ensure_ascii=False) + "\n")
def end(text="", calls=None):
    event("step_end", text=text, tool_calls=calls or [], stop="end_of_turn",
          errors=[], usage={"ttft_s":0.01})
out("__NOSH_EVAL_PROMPT__ ")
request = line()
event("engine", info={"load_s":0.01})
event("open", label="agent", sampling={"seed":0}, tools=[{"name":"ask_user"}])
event("step_start", messages=[{"role":"user","text":request}])
cancelled = False
for question in QUESTIONS:
    end(calls=[{"name":"ask_user","args":question}])
    out("\r\n| " + question["question"] + "\r\nanswer> ")
    answer = line()
    if answer is None:
        cancelled = True
        break
    if DUPLICATE_PROMPT:
        out("\r\nanswer> ")
        assert not select.select([0], [], [], 0.2)[0], "duplicate answer received"
    event("step_start", messages=[{"role":"tool","text":answer}])
if not cancelled:
    end(text="好的，已停止处理。")
event("close")
out("\r\n| " + ("!" if cancelled else "+") + " 2 steps | 0.1 s\r\n")
out("| stats: ttft 0.01s\r\n__NOSH_EVAL_PROMPT__ ")
assert line() == "exit 0"
'''


@unittest.skipUnless(sys.platform == "linux", "Linux PTY and wait4")
class UserQuestionTests(unittest.TestCase):
    def run_questions(self, questions, answers=None, duplicate=False):
        scenario = copy.deepcopy(SCENARIOS["zh-clarify-task"])
        completion = scenario["completions"][0]
        if answers is not None:
            completion["answers"] = answers
        script = "QUESTIONS = " + repr(questions) + "\nDUPLICATE_PROMPT = " + repr(duplicate) + "\n" + SCRIPT
        with tempfile.TemporaryDirectory(prefix="nosh-question-driver-") as temporary:
            root = Path(temporary)
            trace = root / "engine.jsonl"
            result = driver.run_repl(
                [sys.executable, "-c", script], root,
                {"PATH": "/usr/bin:/bin", "NOSH_EVAL_TRACE": str(trace)},
                5, scenario, lambda *_: self.fail("question must not request execution approval"),
            )
            observed = observations.observe(result, scenario, trace, seed=0)
        self.assertIsNone(result.error, result.transcript)
        self.assertIsNone(result.timeout_phase)
        self.assertEqual(result.approvals, [])
        self.assertEqual(result.exit_code, 0, result.transcript)
        self.assertEqual(observed["metrics"]["confirmations"], 0)
        return result, observed

    def test_native_question_is_answered_and_resumes_the_original_session(self):
        question = {"question": "你希望我处理什么具体任务？"}
        result, observed = self.run_questions([question])
        self.assertIsNone(result.failure)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(observed["metrics"]["steps"], 2)
        self.assertEqual(observed["metrics"]["task_status"], "completed")
        self.assertEqual(observed["answer"], "好的，已停止处理。")
        self.assertEqual(observed["questions"][0]["call"]["args"], question)
        self.assertEqual(observed["executions"][0]["result"], result.questions[0]["answer"])
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary) / "project"
            facts = fixtures.create(root, "python")
            verdict = checks.judge(SCENARIOS["zh-clarify-task"], observed["answer"], facts,
                                   root, fixtures.snapshot(root), result, observed["metrics"], observed)
            self.assertTrue(verdict.passed, verdict.reasons)
            for changed in ("missing", "wrong-answer", "wrong-question", "post-answer-tool"):
                invalid = copy.deepcopy(observed)
                if changed == "missing":
                    invalid["questions"] = []
                elif changed == "wrong-answer":
                    invalid["questions"][0]["answer"] = "different"
                elif changed == "wrong-question":
                    invalid["questions"][0]["call"]["args"]["question"] = "要不要继续？"
                else:
                    invalid["executions"].append({"call": {"name":"read_file", "args":{"path":"maths.py"}}})
                verdict = checks.judge(SCENARIOS["zh-clarify-task"], observed["answer"], facts,
                                       root, fixtures.snapshot(root), result, observed["metrics"], invalid)
                self.assertFalse(verdict.passed, (changed, verdict.reasons))

    def test_choices_accept_free_text_and_repeated_ready_text_does_not_repeat_answers(self):
        result, observed = self.run_questions(
            [{"question":"选择什么任务？", "choices":["构建", "检查"]},
             {"question":"使用什么格式？", "choices":["tar.gz", "zip"]}],
            ["自定义任务", "zip"], duplicate=True,
        )
        self.assertIsNone(result.failure)
        self.assertEqual([q["answer"] for q in observed["questions"]], ["自定义任务", "zip"])
        self.assertEqual([q["step"] for q in observed["questions"]], [1, 2])
        self.assertEqual(observed["metrics"]["steps"], 3)

    def test_unplanned_question_is_cancelled_and_fails_without_timing_out(self):
        result, observed = self.run_questions([{"question":"你希望处理什么任务？"}], [])
        self.assertIn("unexpected ask_user", result.failure)
        self.assertEqual(observed["questions"][0]["state"], "cancelled")
        self.assertIsNone(observed["questions"][0]["answer"])
        self.assertNotEqual(observed["metrics"]["task_status"], "completed")

    def test_unused_declared_answer_is_a_failure(self):
        result, observed = self.run_questions([])
        self.assertIn("expected 1 user questions, answered 0", result.failure)
        self.assertEqual(observed["questions"], [])

    def test_answers_must_match_native_call_and_returned_tool_message(self):
        result, observed = self.run_questions([{"question":"你希望完成什么具体任务？"}])
        for field, value in (("sid", 2), ("answer", "wrong"), ("step", 2), ("state", "unknown")):
            altered = copy.deepcopy(result)
            altered.questions[0][field] = value
            with self.subTest(field=field), self.assertRaises(ValueError):
                observations.question_evidence(altered, observed["executions"])
        unreturned = copy.deepcopy(observed["executions"])
        unreturned[0].update(state="unobserved", result=None)
        with self.assertRaisesRegex(ValueError, "original model session"):
            observations.question_evidence(result, unreturned)
        result.questions.append(copy.deepcopy(result.questions[0]))
        with self.assertRaisesRegex(ValueError, "native call"):
            observations.question_evidence(result, observed["executions"])


class QuestionContractTests(unittest.TestCase):
    def test_interaction_trace_rejects_invalid_session_identity(self):
        for identity in (None, True, -1, 2**64, "1"):
            reader = driver.InteractionTrace(None)
            with self.subTest(identity=identity), self.assertRaisesRegex(driver.DriverError, "identity"):
                reader.observe({"ev":"step_end", "engine":1, "sid":identity})

    def test_native_question_tracking_requires_an_advertised_single_completed_call(self):
        reader = driver.InteractionTrace(None)
        opening = {"ev":"open", "engine":1, "sid":1, "tools":[{"name":"ask_user"}]}
        start = {"ev":"step_start", "engine":1, "sid":1}
        call = {"name":"ask_user", "args":{"question":"Task?"}}
        end = {"ev":"step_end", "engine":1, "sid":1, "stop":"end_of_turn", "errors":[], "tool_calls":[call]}
        reader.observe(start)
        reader.observe(end)
        self.assertEqual(reader.pending_questions, {})
        reader.observe(opening)
        reader.observe(start)
        reader.observe(end)
        self.assertEqual(reader.pending_questions[(1, 1)]["call"], call)
        reader.observe(dict(end, stop="max_tokens"))
        self.assertEqual(reader.pending_questions[(1, 1)]["call"], call)
        for change in ({"stop":"cancelled"}, {"errors":[{}]}, {"tool_calls":[call, call]}):
            reader.observe(dict(end, **change))
            self.assertEqual(reader.pending_questions, {})
        reader.observe(end)
        reader.observe({"ev":"close", "engine":1, "sid":1})
        self.assertEqual(reader.pending_questions, {})

    def test_scripted_answers_are_bounded_printable_lines_for_agent_completions(self):
        for answers in ([], None, "text", [1], [""], ["\n"], ["x\r"], ["x\x03"],
                        ["x\u0085"], ["x" * 4097]):
            invalid = copy.deepcopy(SUITE)
            case = next(s for s in invalid["scenarios"] if s["id"] == "zh-clarify-task")
            case["completions"][0]["answers"] = answers
            with self.subTest(answers=answers), self.assertRaisesRegex(ValueError, "answers"):
                suite.validate_suite(invalid)
        valid = copy.deepcopy(SUITE)
        case = next(s for s in valid["scenarios"] if s["id"] == "zh-clarify-task")
        case["completions"][0]["answers"] = ["任意答案", "zip"]
        suite.validate_suite(valid)
        case["expect"]["final_question"] = "require"
        with self.assertRaisesRegex(ValueError, "final-question"):
            suite.validate_suite(valid)
