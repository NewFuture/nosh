from __future__ import annotations
import json
import os
from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
from eval import driver, fixtures, observations, report, trial
from eval.checks import experience as experience_checks
from types import SimpleNamespace
from .support import SCENARIOS


@unittest.skipUnless(sys.platform == "linux", "Linux PTY and wait4")
class DriverTests(unittest.TestCase):
    def test_assistance_trace_parses_appended_records_only_once(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "trace.jsonl"
            reader = driver.InteractionTrace(str(path))
            reader.refresh()
            self.assertEqual(reader.assistance, [])
            observation = {"workflow": "command_assist", "kind": "command", "text": "echo 中文"}
            first = json.dumps({"ev": "step_start", "engine": 1, "sid": 1, "messages": []}).encode() + b"\n"
            second = json.dumps({"ev": "observation", "engine": 1, "sid": 1, "value": observation}, ensure_ascii=False).encode() + b"\n"
            split = second.index("中".encode()) + 1
            path.write_bytes(first + second[:split])
            with patch("eval.driver.json.loads", wraps=json.loads) as decode:
                reader.refresh()
                self.assertEqual(reader.assistance, [])
                for _ in range(10):
                    reader.refresh()
                    self.assertEqual(reader.assistance, [])
                self.assertEqual(decode.call_count, 1)
                with path.open("ab") as stream:
                    stream.write(second[split:])
                reader.refresh()
                self.assertEqual(reader.assistance, [observation])
                for _ in range(10):
                    reader.refresh()
                    self.assertEqual(reader.assistance, [observation])
                self.assertEqual(decode.call_count, 2)

    def test_assistance_trace_reports_corruption_instead_of_hiding_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "trace.jsonl"
            for payload in (b"broken\n", b"[]\n", b'{"ev":"observation","value":null}\n'):
                path.write_bytes(payload)
                with self.assertRaises(driver.DriverError):
                    driver.InteractionTrace(str(path)).refresh()
            path.write_bytes(b'{"ev":"step_start","engine":1,"sid":1}\n')
            reader = driver.InteractionTrace(str(path))
            reader.refresh()
            path.write_bytes(b"")
            with self.assertRaisesRegex(driver.DriverError, "truncated"):
                reader.refresh()
            path.unlink()
            with self.assertRaisesRegex(driver.DriverError, "disappeared"):
                reader.refresh()

    def test_partial_pty_setup_failure_reaps_the_started_child(self):
        import pty

        started = []
        fork = pty.fork
        def track_fork():
            pid, fd = fork()
            if pid:
                started.append(pid)
            return pid, fd
        with patch("pty.fork", side_effect=track_fork), patch("fcntl.ioctl", side_effect=PermissionError("blocked ioctl")):
            with self.assertRaisesRegex(PermissionError, "blocked ioctl"):
                driver.Child([sys.executable, "-c", "import time; time.sleep(60)"],
                             Path.cwd(), {"PATH": "/usr/bin:/bin"}, True)
        self.assertEqual(len(started), 1)
        with self.assertRaises(ChildProcessError):
            os.waitpid(started[0], os.WNOHANG)

    def test_screen_fragmented_queries_and_saved_cursor(self):
        screen = driver.Screen()
        self.assertEqual(screen.feed("abc\x1b["), b"")
        self.assertEqual(screen.feed("6n"), b"\x1b[1;4R")
        screen.feed("\x1b7\x1b[1;100Hconfirm\x1b8")
        self.assertEqual(screen.feed("\x1b[6n"), b"\x1b[1;4R")
        screen.feed("\r\x1b[K" + driver.PROMPT + "git status")
        self.assertEqual(screen.line(), driver.PROMPT + "git status")

    def test_prompt_recognition_accepts_only_the_configured_approval_badge(self):
        self.assertTrue(driver.prompt_matches(driver.PROMPT.rstrip()))
        for content in ("", "git status"):
            for badge in ("", "confirm", "Approval: Confirm"):
                self.assertTrue(driver.prompt_matches(driver.PROMPT + content + "  " + badge, content))
            for extra in ("unexpected", "Approval: Auto", "Approval: YOLO"):
                self.assertFalse(driver.prompt_matches(driver.PROMPT + content + "  " + extra, content))

    def test_cli_drains_both_pipes_and_has_no_controlling_tty(self):
        script = (
            "import os,sys; data=sys.stdin.buffer.read(); print(len(data)); "
            "sys.stderr.write('stderr\\n'); "
            "print(os.isatty(0), os.isatty(1)); "
            "\ntry: os.open('/dev/tty', os.O_RDWR)\nexcept OSError: print('no-tty')"
        )
        result = driver.run_cli([sys.executable, "-c", script], Path.cwd(), {"PATH": "/usr/bin:/bin"}, 5, b"a" * 100_000)
        self.assertIsNone(result.error)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(result.stdout, "100000\nFalse False\nno-tty\n")
        self.assertEqual(result.stderr, "stderr\n")
        self.assertGreater(result.peak_rss_mib, 0)

    def test_timeout_escalates_and_keeps_partial_output(self):
        code = "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(60)"
        result = driver.run_cli([sys.executable, "-c", code], Path.cwd(), {"PATH": "/usr/bin:/bin"}, .3)
        self.assertIn("did not finish", result.error)
        self.assertEqual(result.stdout, "ready\n")
        self.assertEqual(result.exit_code, -9)

    def test_output_limit_is_an_explicit_failure(self):
        with patch.object(driver, "OUTPUT_LIMIT", 4096):
            result = driver.run_cli([sys.executable, "-c", "print('x' * 10000)"],
                                    Path.cwd(), {"PATH": "/usr/bin:/bin"}, 5)
        self.assertIn("output exceeded", result.error)

    def test_cleanup_finds_a_detached_owned_descendant(self):
        code = (
            "import subprocess,sys; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],"
            "stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True);"
            "print(p.pid,flush=True)"
        )
        result = driver.run_cli([sys.executable, "-c", code], Path.cwd(), {"PATH": "/usr/bin:/bin"}, 5)
        self.assertIsNone(result.error)
        status = Path(f"/proc/{int(result.stdout.strip())}/status")
        for _ in range(100):
            try:
                state = status.read_text()
            except FileNotFoundError:
                return
            if "State:\tZ" in state:
                return
            time.sleep(.01)
        self.fail("owned detached child survived cleanup")

    def test_pty_waits_for_split_approval_and_task_completion(self):
        script = r'''
import os, tty, time
tty.setraw(0)
def out(text):
    os.write(1, text.encode())
def line():
    data = b""
    while not data.endswith(b"\r"):
        data += os.read(0, 1)
    return data
out("__NOSH_EVAL_PROMPT__ ")
line()
out("\r\n┃ inspecting\r\n┃ ╭─ exec · MUTATING\r\n┃ │ $ touch fixture\r\n┃ ╰─ [y] run")
time.sleep(.01)
out("  [n] deny  [e] edit › ")
assert os.read(0, 1) == b"y"
out("y\r\n┃ answer\r\n┃ ✔ 1 steps · 0.1 s\r\n┃ stats: ttft 0.01s\r\n__NOSH_EVAL_PROMPT__ ")
assert b"exit 0" in line()
'''
        scenario = {"inputs": ["# task"], "check": "largest", "completions": [{"kind": "agent"}]}
        ascii_script = script.translate(str.maketrans({
            "┃": "|", "╭": "+", "╰": "+", "─": "-", "│": "|",
            "·": "|", "›": ">", "✔": "+",
        }))
        badge_script = script.replace("__NOSH_EVAL_PROMPT__ ", r"__NOSH_EVAL_PROMPT__ \x1b7Approval: Confirm\x1b8")
        for variant in (script, ascii_script, badge_script):
            with self.subTest(ascii=variant == ascii_script):
                result = driver.run_repl([sys.executable, "-c", variant], Path.cwd(),
                                         {"PATH": "/usr/bin:/bin"}, 5, scenario,
                                         lambda command, card: command == "touch fixture")
                self.assertIsNone(result.error, result.transcript)
                self.assertEqual(result.exit_code, 0)
                self.assertEqual(len(result.approvals), 1)
                self.assertEqual(result.approvals[0]["answer"], "y")


@unittest.skipUnless(sys.platform == "linux", "PTY process interfaces")
class ExpandedDriverTests(unittest.TestCase):
    def script(self, first_output, second=False):
        return """
import os, tty
tty.setraw(0)
def out(text):
    os.write(1, text.encode())
def line():
    data = b""
    while not data.endswith(b"\\r"):
        data += os.read(0, 1)
    return data
out("__NOSH_EVAL_PROMPT__ ")
line()
out(%r)
%s
assert b"exit 0" in line()
""" % (first_output, """
assert "为什么" in line().decode()
out("\\r\\n| 这是类型错误，改成整数即可。\\r\\n| + 3 steps | 0.1 s\\r\\n| stats: ttft 0.01s\\r\\n__NOSH_EVAL_PROMPT__ ")
""" if second else "")

    def test_compiler_exit_101_then_chinese_help(self):
        output = "\r\nerror[E0308]: mismatched types\r\nx exit 101 | Ctrl+G or # to ask AI\r\n__NOSH_EVAL_PROMPT__ "
        scenario = SCENARIOS["zh-build-failure"]
        for text in (output, output.replace("x exit", "✗ exit").replace(" | Ctrl", " · Ctrl")):
            with self.subTest(unicode=text != output):
                result = driver.run_repl([sys.executable, "-c", self.script(text, True)], Path.cwd(),
                                         {"PATH": "/usr/bin:/bin"}, 5, scenario, lambda *_: False)
                self.assertIsNone(result.error)
                self.assertIsNone(result.failure)
                self.assertEqual(result.turns[0]["exit_code"], 101)
                self.assertEqual(len(result.turns), 2)
                self.assertEqual(result.exit_code, 0)

    def test_wrong_route_and_unexpected_success_are_failures_not_timeouts(self):
        for scenario in (SCENARIOS["zh-rust-build"], SCENARIOS["zh-build-failure"]):
            with self.subTest(scenario=scenario["id"]):
                output = "\r\nordinary shell returned\r\n__NOSH_EVAL_PROMPT__ "
                result = driver.run_repl([sys.executable, "-c", self.script(output)], Path.cwd(),
                                         {"PATH": "/usr/bin:/bin"}, 5, scenario, lambda *_: False)
                self.assertIsNone(result.error)
                self.assertIsNotNone(result.failure)
                self.assertEqual(len(result.turns), 1)
                self.assertEqual(result.exit_code, 0)

    def test_trial_pipeline_records_grades_raw_state_and_cleanup(self):
        scenario = SCENARIOS["zh-clarify-task"]
        question = {"name":"ask_user", "args":{"question":"你希望我完成什么具体任务？"}}
        answer = "好的，已停止处理。"
        reply = scenario["completions"][0]["answers"][0]
        meta = {"run_id": "unit-test", "observation": "native-v1", "dataset_revision": 16,
                "build": {"binary_sha256": "a" * 64},
                "settings": {"timeout_s": 5}, "scenarios": [scenario], "seeds": [0], "repeat": 1}
        args = SimpleNamespace(threads=1)

        def child(argv, cwd, env, timeout, case, approve):
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}, "tools":[{"name":"ask_user"}]},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": case["inputs"][0]}]},
                {"ev": "step_end", "sid": 1, "tool_calls": [question], "text": "", "usage": {"ttft_s": 0.01},
                 "stop":"end_of_turn", "errors":[]},
                {"ev": "step_start", "sid": 1, "messages":[{"role":"tool", "text":reply}]},
                {"ev": "step_end", "sid": 1, "tool_calls": [], "text": answer, "usage": {"ttft_s": 0.01},
                 "stop":"end_of_turn", "errors":[]},
            ]
            Path(env["NOSH_EVAL_TRACE"]).write_text("\n".join(
                json.dumps(dict(e, schema_version=1, engine=1)) for e in events), encoding="utf-8")
            return driver.Result(exit_code=0, transcript=f"| {answer}\n| + 2 steps | 0.1 s\n| stats: ttft 0.01s\n",
                                 questions=[{"engine":1, "sid":1, "step":1, "input_index":0, "call":question,
                                             "answer":reply, "state":"answered"}])

        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            output = base / "output"
            output.mkdir()
            with fixtures.Workspace(base / "work") as workspace, patch("eval.trial.driver.run_repl", side_effect=child):
                row = trial.run_trial(args, meta, scenario, 0, 0, workspace, output, Path(sys.executable), base / "unused-model")
                self.assertEqual(row["status"], "pass", row["reasons"])
                self.assertTrue(row["grading"]["facts"]["passed"])
                self.assertTrue(row["grading"]["experience"]["final_question"]["passed"])
                self.assertEqual(row["questions"][0]["answer"], reply)
                self.assertEqual(row["metrics"]["confirmations"], 0)
                self.assertIn("maths.py", row["file_snapshot"])
                self.assertFalse((workspace.root / scenario["id"]).exists())
                self.assertTrue((output / row["logs"] / "engine.jsonl").is_file())
                report.save({"schema_version": 2, "metadata": meta, "trials": [row]}, output)

    def test_rejected_requests_still_count_as_confirmations(self):
        script = """
import os, tty
tty.setraw(0)
def out(text):
    os.write(1, text.encode())
def line():
    data = b""
    while not data.endswith(b"\\r"):
        data += os.read(0, 1)
    return data
out("__NOSH_EVAL_PROMPT__ ")
line()
for _ in range(2):
    out("\\r\\n| +- exec - MUTATING\\r\\n| | $ cargo build\\r\\n| +- [y] run [n] deny > ")
    assert os.read(0, 1) == b"n"
    out("\\r\\n| reason (optional, Enter to skip): ")
    assert line() == b"\\r"
out("\\r\\n| 未执行命令。\\r\\n| ! 3 steps | 0.1 s\\r\\n| stats: ttft 0.01s\\r\\n__NOSH_EVAL_PROMPT__ ")
assert b"exit 0" in line()
"""
        scenario = SCENARIOS["zh-rust-build"]
        result = driver.run_repl([sys.executable, "-c", script], Path.cwd(),
                                 {"PATH": "/usr/bin:/bin"}, 5, scenario, lambda *_: False)
        self.assertIsNone(result.error)
        self.assertEqual(len(result.approvals), 2)
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
            ]
            for index in range(3):
                events.extend([
                    {"ev": "step_start", "sid": 1, "messages": [
                        {"role": "user" if index == 0 else "tool",
                         "text": "编译" if index == 0 else "[denied] command was not run"}]},
                    {"ev": "step_end", "sid": 1, "text": "未执行命令。" if index == 2 else "",
                     "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.01},
                     "tool_calls": [{"name": "exec", "args": {"command": "cargo build"}}] if index < 2 else []},
                ])
            trace.write_text("\n".join(json.dumps(dict(e, schema_version=1, engine=1)) for e in events))
            observed = observations.observe(result, scenario, trace, seed=0)
        self.assertEqual(observed["metrics"]["confirmations"], 2)
        self.assertFalse(experience_checks.experience(scenario, observed["answer"], observed["metrics"])["confirmations"]["passed"])

    def test_trial_records_proven_generation_deadline_as_failure(self):
        scenario = SCENARIOS["zh-rust-build"]
        args = SimpleNamespace(threads=1)
        meta = {"settings": {"timeout_s": 5}}
        def child(argv, cwd, env, timeout, case, approve):
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": case["inputs"][0]}]},
            ]
            Path(env["NOSH_EVAL_TRACE"]).write_text("\n".join(
                json.dumps(dict(e, schema_version=1, engine=1)) for e in events), encoding="utf-8")
            return driver.Result(exit_code=-9, total_s=5, error="deadline", timeout_phase="agent")
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            output = base / "output"
            output.mkdir()
            with fixtures.Workspace(base / "work") as workspace, patch("eval.trial.driver.run_repl", side_effect=child):
                row = trial.run_trial(args, meta, scenario, 0, 0, workspace, output, Path(sys.executable), base / "unused-model")
                self.assertEqual(row["status"], "fail", row["reasons"])
                self.assertEqual(row["metrics"]["task_status"], "timed_out")
                self.assertEqual(row["metrics"]["steps"], 1)
                self.assertIsNone(row["metrics"]["ttft_s"])
                self.assertIsNone(row["grading"]["experience"])
                self.assertEqual(row["answer"], "")
                self.assertFalse((workspace.root / scenario["id"]).exists())

    def test_trial_records_post_generation_deadline_as_failure(self):
        scenario = SCENARIOS["zh-node-test"]
        args = SimpleNamespace(threads=1)
        meta = {"settings": {"timeout_s": 60}}

        def child(argv, cwd, env, timeout, case, approve):
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": case["inputs"][0]}]},
                {"ev": "step_end", "sid": 1, "text": "tests passed", "tool_calls": [],
                 "errors": [], "stop": "end_of_turn", "usage": {"ttft_s": 0.2}},
            ]
            Path(env["NOSH_EVAL_TRACE"]).write_text("\n".join(
                json.dumps(dict(event, schema_version=1, engine=1)) for event in events))
            return driver.Result(exit_code=-9, total_s=60, error="deadline", timeout_phase="agent")

        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            output = base / "output"
            output.mkdir()
            with fixtures.Workspace(base / "work") as workspace, patch(
                "eval.trial.driver.run_repl", side_effect=child
            ):
                row = trial.run_trial(
                    args, meta, scenario, 0, 0, workspace, output,
                    Path(sys.executable), base / "unused-model")
                self.assertEqual(row["status"], "fail", row["reasons"])
                self.assertEqual(row["deadline_state"], "after_generation")
                self.assertIn("after final generation completed", row["reasons"][0])
                self.assertEqual(row["answer"], "tests passed")
                self.assertIsNone(row["grading"]["experience"])
