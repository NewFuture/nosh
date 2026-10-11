"""Model-free resident protocol, lifecycle, accounting and production CLI probes."""

import contextlib
import copy
import io
import json
import multiprocessing
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from eval import campaign, checks, driver, fixtures, observations, report, resident, runtime, suite, trial
from .support import SCENARIOS
from . import test_observations


class AccountingTests(unittest.TestCase):
    def test_native_timing_keeps_initialization_and_completed_step_costs_separate(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "engine.jsonl"
            test_observations.ObservationTests().agent_trace(trace)
            events = [json.loads(line) for line in trace.read_text().splitlines()]
            worker = {"pid": 42, "config": {"context_length": 8192}}
            events[0]["info"].update(load_s=0, execution_mode="resident",
                                     worker_pid=42, worker_config=worker["config"])
            events[-1]["usage"].update(prefill_s=.4, decode_s=.1, cached_tokens=700, prompt_tokens=20)
            trace.write_text("\n".join(map(json.dumps, events)))
            result = driver.Result(total_s=2, transcript="| + 1 steps | 1.0 s\n")
            row = observations.observe(result, SCENARIOS["largest-files"], trace, seed=0, expected_worker=worker)
            self.assertEqual(row["metrics"]["load_s"], 0)
            self.assertAlmostEqual(row["metrics"]["case_other_s"], 1.5)
            self.assertEqual(row["metrics"]["first_step_cached_tokens"], 700)
            self.assertTrue(row["metrics"]["timing_complete"])
            for wrong in (None, dict(worker, pid=43), dict(worker, config={})):
                with self.assertRaisesRegex(ValueError, "resident"):
                    observations.observe(result, SCENARIOS["largest-files"], trace, seed=0, expected_worker=wrong)
            for total in (.5, 2):
                result.total_s = total
                observed = observations.observe(result, SCENARIOS["largest-files"], trace, seed=0, expected_worker=worker)
                self.assertEqual(observed["metrics"]["case_other_s"], total - .5)
            result.total_s = .4
            with self.assertRaisesRegex(ValueError, "case_other_s"):
                observations.observe(result, SCENARIOS["largest-files"], trace, seed=0, expected_worker=worker)
            events.append({"ev": "step_start", "schema_version": 1, "engine": 1, "sid": 1, "messages": []})
            trace.write_text("\n".join(map(json.dumps, events)))
            result = driver.Result(total_s=2, exit_code=-9, timeout_phase="agent")
            row = observations.observe(result, SCENARIOS["largest-files"], trace, seed=0,
                                       expected_worker=worker, deadline_timeout=True)
            self.assertFalse(row["metrics"]["timing_complete"])
            self.assertIsNone(row["metrics"]["case_other_s"])
            self.assertEqual(row["metrics"]["prefill_s"], .4)

    @unittest.skipUnless(sys.platform == "linux", "Linux planning entry")
    def test_resident_plan_is_bounded_and_does_not_start_worker(self):
        with patch("eval.resident.Worker", side_effect=AssertionError("started worker")), \
                contextlib.redirect_stdout(io.StringIO()) as stdout:
            code = campaign.main(["--suite", "smoke", "--seeds", "0", "--execution-mode", "resident",
                                  "--worker-start-timeout", "20", "--plan", "--budget", "700"])
        self.assertEqual(code, 0)
        plan = json.loads(stdout.getvalue())
        self.assertEqual(plan["execution_mode"], "resident")
        self.assertEqual(plan["warmup_generations"], 0)
        self.assertEqual(plan["trials"], 5)
        self.assertEqual(plan["maximum_worker_seconds"], 100)
        with contextlib.redirect_stderr(io.StringIO()):
            self.assertEqual(campaign.main(["--suite", "smoke", "--execution-mode", "resident",
                                            "--plan", "--worker-start-timeout", "1e308",
                                            "--timeout", "2e307"]), 2)

    def test_reports_warn_about_mixed_cost_scopes(self):
        from .test_report import ReportTests
        data = ReportTests().sample()
        old = copy.deepcopy(data)
        data["metadata"]["settings"] = {"execution_mode": "resident"}
        comparison = report.compare(data, old)
        self.assertTrue(any("lifecycles differ" in note for note in comparison["warnings"]))
        self.assertIn("**resident**", report.markdown(data))
        data["trials"][0]["engines"] = [{"load_s": 1, "device": "cpu"}]
        with self.assertRaisesRegex(ValueError, "mixed cold/resident"):
            report.validate(data)

    def test_residual_duration_validation_rejects_negative_and_nonfinite_values(self):
        from .test_report import ReportTests
        data = ReportTests().sample()
        for value in (-.001, float("nan"), float("inf"), "0", False):
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "case_other_s"):
                data["trials"][0]["metrics"]["case_other_s"] = value
                report.validate(data)
        for value in (None, 0, 1.25):
            data["trials"][0]["metrics"]["case_other_s"] = value
            report.validate(data)

    @unittest.skipUnless(sys.platform == "linux", "Linux planning entry")
    def test_typo_only_resident_plan_has_no_worker_budget(self):
        arguments = ["--suite", "smoke", "--scenario", "typo-correction", "--seeds", "0",
                     "--execution-mode", "resident", "--timeout", "7", "--budget", "7", "--plan"]
        with patch("eval.resident.Worker", side_effect=AssertionError("started worker")), \
                contextlib.redirect_stdout(io.StringIO()) as stdout:
            self.assertEqual(campaign.main(arguments), 0)
        plan = json.loads(stdout.getvalue())
        self.assertEqual(plan["trials"], 1)
        self.assertEqual(plan["maximum_trial_seconds"], 7)
        self.assertEqual(plan["maximum_worker_seconds"], 0)
        with contextlib.redirect_stderr(io.StringIO()) as stderr:
            self.assertEqual(campaign.main(arguments + ["--scenario", "largest-files", "--budget", "14"]), 2)
        self.assertIn("resident startup/cleanup", stderr.getvalue())


WORKER_SCRIPT = r"""
import json, os, socket, sys, threading, time
path, mode = sys.argv[1:]
if mode == "supervised":
    def watch_parent():
        assert os.read(0, 1) == b""
        os._exit(2)
    threading.Thread(target=watch_parent, daemon=True).start()
if mode == "stall":
    time.sleep(30)
listener = socket.socket(socket.AF_UNIX)
listener.bind(path)
listener.listen()
print(json.dumps({"ev":"ready", "version":1, "pid":os.getpid(), "warmup_generations":0,
                  "info":{"load_s":.25}, "config":{}}), flush=True)
while True:
    connection, _ = listener.accept()
    with connection, connection.makefile("rb") as stream:
        assert json.loads(stream.readline()) == {"op":"status"}
        connection.sendall((json.dumps({"Status":{"pid":os.getpid(), "connections":0,
                             "closed_sessions":0, "active_sessions":0, "rss_mib":1}}) + "\n").encode())
"""


@unittest.skipUnless(sys.platform == "linux", "Unix socket lifecycle")
class WorkerTests(unittest.TestCase):
    def test_one_start_no_generation_and_cleanup_after_worker_failure(self):
        popen = subprocess.Popen
        with tempfile.TemporaryDirectory() as temporary:
            output = Path(temporary)
            meta = {}
            args = SimpleNamespace(device="cpu", threads=1, worker_start_timeout=3)
            launched = []

            def launch(argv, **kwargs):
                launched.append(argv)
                return popen([sys.executable, "-c", WORKER_SCRIPT, argv[-1], "ready"], **kwargs)

            with patch("eval.resident.subprocess.Popen", side_effect=launch):
                with resident.Worker(args, Path("unused"), Path("unused"), output, meta) as worker:
                    self.assertEqual(worker.checkpoint()["active_sessions"], 0)
                    self.assertEqual(meta["worker"]["warmup_generations"], 0)
                    self.assertEqual(meta["worker"]["info"]["load_s"], .25)
                    path, proc = worker.path, worker.proc
            self.assertEqual(len(launched), 1)
            self.assertEqual(meta["worker"]["status"], "completed")
            self.assertFalse(path.exists())
            self.assertIsNotNone(proc.poll())
        with tempfile.TemporaryDirectory() as temporary:
            meta = {}
            with patch("eval.resident.subprocess.Popen", side_effect=launch), \
                    self.assertRaisesRegex(RuntimeError, "no restart or fallback"):
                with resident.Worker(args, Path("unused"), Path("unused"), Path(temporary), meta) as worker:
                    worker.proc.kill()
                    worker.proc.wait(timeout=3)
                    worker.checkpoint()
            self.assertEqual(meta["worker"]["status"], "aborted")
            self.assertEqual(len(launched), 2)

    def test_startup_deadline_reaps_only_its_worker(self):
        popen = subprocess.Popen
        started = []
        def launch(argv, **kwargs):
            proc = popen([sys.executable, "-c", WORKER_SCRIPT, argv[-1], "stall"], **kwargs)
            started.append(proc)
            return proc
        with tempfile.TemporaryDirectory() as temporary, patch("eval.resident.subprocess.Popen", side_effect=launch):
            args = SimpleNamespace(device="cpu", threads=1, worker_start_timeout=.1)
            meta = {}
            with self.assertRaisesRegex(TimeoutError, "startup"):
                with resident.Worker(args, Path("unused"), Path("unused"), Path(temporary), meta):
                    self.fail("unready worker was used")
            self.assertEqual(meta["worker"]["status"], "startup_failed")
            self.assertIsNotNone(started[0].poll())

    def test_campaign_sigterm_and_sigkill_close_the_worker_lifetime_pipe(self):
        supervisor = r"""
import json, os, subprocess, sys, tempfile, time
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch
from eval.resident import Worker
root = Path(sys.argv[1])
tempfile.tempdir = str(root)
popen = subprocess.Popen
def launch(argv, **kwargs):
    assert kwargs["stdin"] == subprocess.PIPE
    return popen([sys.executable, "-c", sys.argv[2], argv[-1], "supervised"], **kwargs)
with patch("eval.resident.subprocess.Popen", side_effect=launch):
    with Worker(SimpleNamespace(device="cpu", threads=1, worker_start_timeout=3),
                Path("unused"), Path("unused"), root, {}) as worker:
        print(json.dumps({"worker_pid": worker.proc.pid}), flush=True)
        while True:
            time.sleep(1)
"""
        for sig in (signal.SIGTERM, signal.SIGKILL):
            with self.subTest(signal=sig), tempfile.TemporaryDirectory() as temporary:
                parent = subprocess.Popen([sys.executable, "-c", supervisor, temporary, WORKER_SCRIPT],
                                          cwd=runtime.ROOT, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                                          text=True, start_new_session=True)
                pidfd = None
                try:
                    self.assertTrue(select.select([parent.stdout], [], [], 5)[0], "supervisor never became ready")
                    message = parent.stdout.readline()
                    self.assertTrue(message, parent.stderr.read() if parent.poll() is not None else "no worker receipt")
                    worker_pid = json.loads(message)["worker_pid"]
                    self.assertNotEqual(os.getpgid(worker_pid), os.getpgid(parent.pid))
                    pidfd = os.pidfd_open(worker_pid)
                    self.assertFalse(select.select([pidfd], [], [], 0)[0], "worker exited before parent")
                    parent.send_signal(sig)
                    self.assertEqual(parent.wait(timeout=5), -sig)
                    self.assertTrue(select.select([pidfd], [], [], 5)[0], "worker survived abrupt campaign death")
                finally:
                    if parent.poll() is None:
                        parent.kill()
                    parent.wait(timeout=5)
                    if pidfd is not None:
                        if not select.select([pidfd], [], [], 0)[0]:
                            signal.pidfd_send_signal(pidfd, signal.SIGKILL)
                        os.close(pidfd)
                    parent.stdout.close()
                    parent.stderr.close()

    def test_typo_only_campaign_never_starts_a_worker(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            weights, tokenizer = root / "model.gguf", root / "tokenizer.json"
            weights.write_bytes(b"unused")
            tokenizer.write_text("{}")

            def metadata(args, suite, *_):
                return {"run_id": "local-only", "observation": "native-v1",
                        "dataset_revision": suite["dataset_revision"], "build": {"binary_sha256": "a" * 64},
                        "settings": {"execution_mode": "resident"}, "scenarios": suite["scenarios"],
                        "seeds": [0], "repeat": 1}

            row = {"scenario_id": "typo-correction", "seed": 0, "repeat": 0, "status": "fail",
                   "metrics": {key: None for key in report.METRICS},
                   "answer": "", "final_state": None, "reasons": ["scripted"]}
            with patch("eval.resident.Worker", side_effect=AssertionError("unneeded worker")), \
                    patch("eval.runtime.discover_tools", return_value={}), \
                    patch("eval.runtime.metadata", side_effect=metadata), \
                    patch("eval.campaign.run_trial", return_value=row) as run, \
                    contextlib.redirect_stdout(io.StringIO()):
                code = campaign.main(["--suite", "smoke", "--scenario", "typo-correction", "--seeds", "0",
                                      "--execution-mode", "resident", "--timeout", "7", "--budget", "7",
                                      "--binary", sys.executable, "--model-path", str(weights),
                                      "--output", str(root / "report"), "--work-dir", str(root / "work")])
            self.assertEqual(code, 1)
            self.assertEqual(run.call_count, 1)
            self.assertNotIn("worker", run.call_args.kwargs)

    def test_campaign_preserves_row_and_missing_denominator_on_worker_loss(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            weights, tokenizer = root / "model.gguf", root / "tokenizer.json"
            weights.write_bytes(b"stub")
            tokenizer.write_text("{}")
            def metadata(args, suite, *_):
                return {
                    "run_id": "resident-loss", "observation": "native-v1",
                    "dataset_revision": suite["dataset_revision"],
                    "build": {"binary_sha256": "a" * 64}, "settings": {"execution_mode": "resident"},
                    "scenarios": suite["scenarios"], "seeds": [0, 1], "repeat": 1,
                }
            def run_trial(args, meta, scenario, seed, repeat, *_, **kwargs):
                return {"scenario_id": scenario["id"], "seed": seed, "repeat": repeat,
                        "status": "fail", "metrics": {name: None for name in report.METRICS},
                        "answer": "", "final_state": None, "reasons": ["task failed"]}
            worker = unittest.mock.MagicMock()
            worker.__enter__.return_value = worker
            worker.checkpoint.side_effect = RuntimeError("worker disconnected")
            with patch("eval.resident.Worker", return_value=worker), \
                    patch("eval.runtime.discover_tools", return_value={}), \
                    patch("eval.runtime.metadata", side_effect=metadata), \
                    patch("eval.campaign.run_trial", side_effect=run_trial) as run, \
                    contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                code = campaign.main(["--suite", "regression", "--scenario", "largest-files", "--seeds", "0", "1",
                                      "--execution-mode", "resident", "--binary", sys.executable,
                                      "--model-path", str(weights), "--output", str(root / "report"),
                                      "--work-dir", str(root / "work")])
            self.assertEqual(code, 2)
            self.assertEqual(run.call_count, 1, "worker loss must not start another trial")
            data = json.loads((root / "report" / "report.json").read_text())
            self.assertEqual(len(data["trials"]), 1)
            self.assertEqual(data["trials"][0]["status"], "error")
            self.assertIn("worker disconnected", data["error"])
            self.assertEqual(report.aggregate(data)[0]["missing"], 1)


class ScriptedWorker:
    """Only the inference boundary is scripted; the real CLI executes all tools."""
    def __init__(self, path, turns):
        self.path, self.turns = path, iter(turns)
        context = multiprocessing.get_context("fork")
        self.manager = context.Manager()
        self.requests, self.errors = self.manager.list(), self.manager.list()
        self.stop = context.Event()
        self.listener = socket.socket(socket.AF_UNIX)
        self.listener.bind(str(path))
        self.listener.listen()
        self.listener.settimeout(.1)
        self.process = context.Process(target=self.run)

    def __enter__(self):
        self.process.start()
        self.listener.close()
        return self

    def __exit__(self, *_):
        self.stop.set()
        self.process.join(timeout=5)
        errors = list(self.errors)
        self.manager.shutdown()
        if self.process.is_alive():
            self.process.kill()
            self.process.join(timeout=5)
            raise AssertionError("mock worker did not stop")
        if errors:
            raise errors[0]

    def run(self):
        try:
            while not self.stop.is_set():
                try:
                    connection, _ = self.listener.accept()
                except TimeoutError:
                    continue
                with connection, connection.makefile("rb") as stream:
                    connection.settimeout(5)
                    sessions, next_id = {}, 1
                    while line := stream.readline(8 * 1024 * 1024):
                        request = json.loads(line)
                        self.requests.append(request)
                        op = request["op"]
                        if op == "hello":
                            self.send(connection, {"Hello": {"version": 1, "info": {
                                "load_s": 0, "device": "cpu", "execution_mode": "resident",
                                "worker_pid": os.getpid(), "worker_config": request["config"]}, "description": "scripted"}})
                            continue
                        if op == "cancel":
                            continue
                        sid = request.get("sid", next_id)
                        outcome = None
                        if op == "open":
                            sessions[sid] = 0
                            next_id += 1
                        elif op == "step":
                            sessions[sid] += len(request["append"]) + 1
                            turn = next(self.turns)
                            text, calls = turn.get("text", ""), turn.get("calls", [])
                            for call in calls:
                                self.send(connection, {"Event": {"ToolCall": call}})
                            if text:
                                self.send(connection, {"Event": {"Text": text}})
                            outcome = {
                                "text": text, "think": "", "tool_calls": calls, "errors": [], "stop": "EndOfTurn",
                                "usage": {"prompt_tokens": 10, "cached_tokens": 20, "completion_tokens": 2,
                                          "prefill_secs": .001, "decode_secs": .001, "ttft_secs": .001,
                                          "context_used": 32, "context_max": 8192}}
                        elif op == "close":
                            del sessions[sid]
                        elif op not in ("compact", "choice", "rewind"):
                            raise AssertionError(f"unexpected operation {op}")
                        self.send(connection, {"Done": {"Ok": {
                            "sid": sid, "state": {"messages": sessions.get(sid, 0), "context": [32, 8192]},
                            "outcome": outcome, "changed": 0}}})
        except BaseException as error:
            self.errors.append(error)

    @staticmethod
    def send(connection, value):
        connection.sendall((json.dumps(value) + "\n").encode())


BINARY = os.environ.get("NOSH_TEST_BINARY")


@unittest.skipUnless(sys.platform == "linux" and BINARY, "set NOSH_TEST_BINARY to run real CLI/PTY without a model")
class ProductionProxyTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="nosh-proxy-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.weights = self.base / "model.gguf"
        self.weights.write_bytes(b"not a model; any local fallback must fail")
        (self.base / "tokenizer.json").write_text("{}")
        self.socket = self.base / "worker.sock"

    def case(self, number, seed, capture_output=None, command_assist=False):
        root, home = self.base / f"case{number}", self.base / f"home{number}"
        root.mkdir()
        home.mkdir()
        trace = home / "engine.jsonl"
        env = runtime.environment(home, 1, trace, capture_output=capture_output,
                                  command_assist=command_assist)
        env["NOSH_EVAL_WORKER"] = str(self.socket)
        argv = [str(Path(BINARY).resolve()), "--offline", "--no-download", "--norc",
                "--model-path", str(self.weights), "--seed", str(seed)]
        return root, home, trace, env, argv

    def test_typing_prefix_opens_local_completion_before_submission(self):
        names = {"auto", "clear", "ctx", "fix", "help", "mode", "status", "think"}
        for number, mode in enumerate(("emacs", "vi")):
            with self.subTest(mode=mode), ScriptedWorker(
                    self.socket, [{"text": "PREFIX_TASK_DONE"}]) as server:
                root, home, _, env, argv = self.case(number, 0)
                config = home / "nosh" / "config.toml"
                config.write_text(config.read_text().replace(
                    "[shell]\n", f'[shell]\nedit_mode = "{mode}"\n'), encoding="utf-8")
                child = driver.Child(
                    argv, root, dict(env, PS1=driver.PROMPT, PS2=driver.CONTINUATION), True)
                deadline = child.start + 20

                def screen_text():
                    return "\n".join("".join(row) for row in child.screen.lines)

                def selected(command):
                    return any(row.strip().startswith(f">{command} ")
                               for row in screen_text().splitlines())

                def wait_draft(expected):
                    child.until(
                        lambda: driver.prompt_matches(child.screen.line(), expected)
                        or mode == "vi" and driver.prompt_matches(
                            child.screen.line(), f"[I] {expected}".rstrip()),
                        deadline, f"draft {expected!r}")

                def menu_visible():
                    return names.issubset(
                        words[0] for row in screen_text().splitlines()
                        if (words := row.strip().lstrip(">").split()))

                try:
                    child.until(child.screen.at_prompt, deadline, "initial prompt")
                    for partial, command in (("s", "status"), ("f", "fix")):
                        child.send(b"#")
                        child.until(menu_visible, deadline, "automatic command menu")
                        self.assertTrue(selected("# <task>"), screen_text())
                        self.assertEqual(len(server.requests), 0, child.result.transcript)
                        self.assertNotIn("#out", driver.plain(child.result.transcript))

                        for text, expected in (
                                (partial.encode(), f"#{partial}"),
                                (b"\x7f", "#"),
                                (partial.encode(), f"#{partial}"),
                                (b"q", f"#{partial}q"),
                                (b"\x7f", f"#{partial}"),
                                (b"q", f"#{partial}q"),
                                (b"\x1b[D\x1b[3~", f"#{partial}"),
                                (command[1:].encode(), f"#{command}"),
                                (b"\x7f", f"#{command[:-1]}"),
                                (b"\x7f" * len(command), "")):
                            child.send(text)
                            wait_draft(expected)
                            if expected == "#":
                                child.until(menu_visible, deadline, "menu retained after backspace")
                            elif expected in (f"#{partial}", f"#{command[:-1]}"):
                                child.until(
                                    lambda: selected(command),
                                    deadline, "menu restored for the edited command prefix")
                            if not expected.endswith("q"):
                                self.assertNotIn("unknown command", screen_text())
                        child.send(b"#")
                        child.until(menu_visible, deadline, "menu reopened after deleting the draft")
                        child.send(partial.encode())
                        wait_draft(f"#{partial}")
                        child.until(lambda: selected(command), deadline, "filtered selection")
                        self.assertNotIn("Selection unavailable", screen_text())
                        child.send(b"\r")
                        wait_draft(f"#{command}")
                        self.assertFalse(menu_visible(), screen_text())
                        self.assertEqual(len(server.requests), 0, child.result.transcript)
                        child.send(b"\r")
                        wait_draft("")
                        self.assertEqual(len(server.requests), 0, child.result.transcript)

                    start = len(child.result.transcript)
                    child.send(b"#help\r")
                    child.until(
                        lambda: "# <task>" in driver.plain(child.result.transcript[start:])
                        and child.screen.at_prompt() and "#help" not in child.screen.line(),
                        deadline, "fully typed help submitted directly")
                    self.assertEqual(len(server.requests), 0, child.result.transcript)

                    child.send(b"#")
                    child.until(menu_visible, deadline, "second automatic command menu")
                    self.assertTrue(selected("# <task>"), screen_text())
                    child.send(b"\r")
                    child.until(lambda: not menu_visible(), deadline, "task entry accepted")
                    wait_draft("#")
                    self.assertEqual(len(server.requests), 0, child.result.transcript)
                    child.send(b"explain this\r")
                    child.until(
                        lambda: "PREFIX_TASK_DONE" in child.result.transcript
                        and driver.prompt_matches(
                            child.screen.line(), "[I]" if mode == "vi" else ""),
                        deadline, "task submitted after the prefix menu")
                    specs = [request["spec"] for request in server.requests
                             if request["op"] == "open"]
                    self.assertEqual([spec["label"] for spec in specs], ["agent"])
                    self.assertTrue(any(
                        message.get("User") == "explain this"
                        for request in server.requests if request["op"] == "step"
                        for message in request["append"]))
                    child.send(b"exit\r")
                    child.until(lambda: child.result.exit_code is not None, deadline, "shell exit")
                    self.assertEqual(child.result.exit_code, 0, child.result.transcript)
                except (driver.DriverError, TimeoutError) as error:
                    self.fail(f"{error}\n{driver.plain(child.result.transcript)}")
                finally:
                    child.close()
            self.socket.unlink()

    def test_fresh_shell_home_cwd_environment_approvals_and_seed(self):
        commands = [
            "export RESIDENT_TEST=leak; cd data; printf '%s' \"$HOME\" > home.txt",
            "printf '%s' \"${RESIDENT_TEST-unset}\" > state.txt; printf '%s' \"$HOME\" > home.txt; pwd > cwd.txt",
            "touch denied.txt",
        ]
        turns = [turn for command in commands for turn in (
            {"calls": [{"name": "exec", "args": {"command": command}}]}, {"text": "Complete."})]
        with ScriptedWorker(self.socket, turns) as server:
            for number in range(3):
                root, home, trace, env, argv = self.case(number, number)
                (root / "data").mkdir()
                scenario = {"inputs": ["# isolated task"], "completions": [{"kind": "agent"}], "check": "cwd"}
                result = driver.run_repl(argv, root, env, 15, scenario, lambda *_: number != 2)
                self.assertIsNone(result.error, result.transcript)
                self.assertIsNone(result.failure, result.transcript)
                self.assertEqual(len(result.approvals), 1)
                self.assertEqual(result.approvals[0]["allowed"], number != 2)
                if number == 0:
                    self.assertEqual(result.pwd, str(root / "data"))
                    self.assertEqual((root / "data" / "home.txt").read_text(), str(home))
                elif number == 1:
                    self.assertEqual((root / "state.txt").read_text(), "unset")
                    self.assertEqual((root / "home.txt").read_text(), str(home))
                    self.assertEqual((root / "cwd.txt").read_text().strip(), str(root))
                else:
                    self.assertFalse((root / "denied.txt").exists())
                self.assertTrue(trace.is_file())
            specs = [r["spec"] for r in server.requests if r["op"] == "open"]
            self.assertEqual([s["sampling"]["seed"] for s in specs], [0, 1, 2])
            starts = [r for r in server.requests if r["op"] == "step" and r["append"]
                      and "User" in r["append"][-1]]
            self.assertEqual(len(starts), 3)
            self.assertTrue(all(r["sid"] == 1 for r in starts))

    def test_native_query_and_direct_final_command_assist_are_preserved(self):
        command = "tar -czf logs.tar.gz logs"
        turns = [{"calls": [{"name": "command_help", "args": {"name": "tar", "query": "gz"}}]},
                 {"text": command}]
        with ScriptedWorker(self.socket, turns) as server:
            root, _, trace, env, argv = self.case(0, 0)
            (root / "logs").mkdir()
            result = driver.run_cli(argv + ["-s", "Archive logs to logs.tar.gz"], root, env, 15)
            self.assertIsNone(result.error, result.stderr)
            self.assertEqual(result.exit_code, 0, result.stderr)
            self.assertEqual(result.stdout.strip(), command)
            self.assertFalse((root / "logs.tar.gz").exists())
            specs = [r["spec"] for r in server.requests if r["op"] == "open"]
            self.assertEqual(specs[-1]["label"], "command_assist.generate.foreground")
            self.assertEqual([t["name"] for t in specs[-1]["tools"]], ["command_help", "read_file", "grep"])
            self.assertEqual([r["choice"] for r in server.requests if r["op"] == "choice"],
                             [{"type": "auto"}, {"type": "auto"}])
            tool_results = [message["Tool"] for r in server.requests if r["op"] == "step"
                            for message in r["append"] if "Tool" in message]
            self.assertEqual(len(tool_results), 1)
            header, metadata, body = tool_results[0].split("\n", 2)
            self.assertEqual(header, "[command_help]")
            self.assertEqual(json.loads(metadata)["exit_code"], 0)
            self.assertEqual(json.loads(metadata)["query"], "gz")
            self.assertIn("gzip", body)

    def test_generate_default_filename_uses_the_real_user_task_and_full_archive_judge(self):
        scenario = next(s for s in suite.load_suite("command-assist")["scenarios"]
                        if s["id"] == "generate-archive-default-name")
        for number, command in enumerate(("tar -czf logs.tar.gz logs", "tar -czf log-backup.tgz logs")):
            with self.subTest(command=command), ScriptedWorker(self.socket, [{"text": command}]) as server:
                base, _, trace, env, argv = self.case(number, 0)
                root = base / "files"
                facts = fixtures.create(root, scenario["fixture"])
                result = driver.run_cli(argv + ["-s", scenario["input"]], root, env, 15)
                self.assertIsNone(result.error, result.stderr)
                self.assertEqual(result.exit_code, 0, result.stderr)
                self.assertEqual(result.stdout.strip(), command)
                self.assertEqual(fixtures.snapshot(root), facts["before"])
                requests = [r["append"] for r in server.requests if r["op"] == "step"]
                self.assertEqual(len(requests), 1)
                self.assertEqual(requests[0], [{"User": (
                    f"Give a shell command for:\n```text\n{scenario['input']}\n```\n\n"
                    f"Environment:\ncwd: {json.dumps(str(root))}\n\n"
                    "Return the shell input itself, without wrapping the response in inline backticks or Markdown fences."
                )}])
                events = [json.loads(line) for line in trace.read_text().splitlines()]
                info = next(e["info"] for e in events if e["ev"] == "engine")
                observed = observations.observe(
                    result, scenario, trace, seed=0,
                    expected_worker={"pid": info["worker_pid"], "config": info["worker_config"]},
                )
                verdict = checks.judge(scenario, observed["answer"], facts, root, fixtures.snapshot(root),
                                       result, observed["metrics"], observed)
                self.assertTrue(verdict.passed, verdict.reasons)
                self.assertEqual(fixtures.snapshot(root), facts["before"])
            self.socket.unlink()

    def test_all_assist_scenarios_match_real_context_and_reference_outcomes(self):
        scenarios = {}
        for name in suite.BUILTIN_SUITES:
            for scenario in suite.load_suite(name)["scenarios"]:
                if scenario.get("assistance"):
                    if scenario["id"] in scenarios:
                        self.assertEqual(scenarios[scenario["id"]], scenario)
                    scenarios[scenario["id"]] = scenario
        references = {
            "suggest-archive": "tar -czf logs.tar.gz logs",
            "generate-archive": "tar -czf logs.tar.gz logs",
            "generate-help": "tar --help",
            "generate-archive-default-name": "tar -czf log-backup.tgz logs",
            "generate-natural-clarification": "[None]",
            "auto-fix-archive": "tar -czf logs.tar.gz logs",
            "fix-partially-completed-archive": "mkdir -p backups",
            "next-no-goal": "[None]",
            "next-retry-after-prerequisite": "ls -ld archive backups",
        }
        self.assertEqual(scenarios.keys(), references.keys())
        systems = set()
        for number, (sid, scenario) in enumerate(scenarios.items()):
            contract = scenario["assistance"]
            for variant, expected_pass in enumerate((True, False)):
                answer = references[sid] if expected_pass else "echo unrelated"
                turns = [{"text": answer}]
                with self.subTest(scenario=sid, expected_pass=expected_pass), ScriptedWorker(self.socket, turns) as server:
                    base, _, trace, env, argv = self.case(
                        number * 2 + variant, 0, scenario.get("capture_output"), contract["automatic"],
                    )
                    root = base / "files"
                    facts = fixtures.create(root, scenario["fixture"])
                    if scenario["mode"] == "suggest":
                        result = driver.run_cli(argv + ["-s", scenario["input"]], root, env, 20)
                    else:
                        result = driver.run_repl(argv, root, env, 20, scenario, lambda *_: False)
                    self.assertIsNone(result.error, result.transcript or result.stderr)
                    self.assertIsNone(result.failure, result.transcript or result.stderr)
                    self.assertEqual(result.approvals, [])
                    events = [json.loads(line) for line in trace.read_text().splitlines()]
                    opens = [event for event in events if event["ev"] == "open"]
                    self.assertEqual(len(opens), 1)
                    spec = opens[0]
                    systems.add(spec["system"])
                    suffix = "background" if contract["automatic"] else "foreground"
                    self.assertEqual(spec["label"], f"command_assist.{contract['intent']}.{suffix}")
                    self.assertEqual([tool["name"] for tool in spec["tools"]], ["command_help", "read_file", "grep"])
                    self.assertFalse(spec["thinking"])
                    self.assertEqual(spec["max_new_tokens"], 512)
                    starts = [event for event in events if event["ev"] == "step_start"]
                    messages = starts[0]["messages"]
                    self.assertEqual(len(messages), 1)
                    self.assertEqual(messages[0]["role"], "user")
                    body = messages[0]["text"]
                    self.assertIn("Environment:\ncwd: " + json.dumps(str(root)), body)
                    self.assertEqual(
                        "Return the shell input itself, without wrapping the response in inline backticks or Markdown fences." in body,
                        contract["intent"] == "generate",
                    )
                    self.assertNotIn(sid, body)
                    self.assertNotIn("require_query", body)
                    info = next(event["info"] for event in events if event["ev"] == "engine")
                    observed = observations.observe(
                        result, scenario, trace, seed=0,
                        expected_worker={"pid": info["worker_pid"], "config": info["worker_config"]},
                    )
                    self.assertEqual(len(observed["assistance"]), 1)
                    accepted = observed["assistance"][0]
                    self.assertEqual(accepted["kind"], "none" if answer == "[None]" else "command")
                    if contract["intent"] == "generate":
                        self.assertEqual(body, (
                            f"Give a shell command for:\n```text\n{scenario['input']}\n```\n\n"
                            f"Environment:\ncwd: {json.dumps(str(root))}\n\n"
                            "Return the shell input itself, without wrapping the response in inline backticks or Markdown fences."
                        ))
                        self.assertIsNone(accepted["execution"])
                    else:
                        execution = accepted["execution"]
                        command = scenario["inputs"][-1]
                        self.assertEqual(execution["command"], command)
                        self.assertEqual(execution["execution_cwd"], str(root))
                        self.assertEqual(execution["exit"] == 0, contract["intent"] == "next")
                        self.assertIn(f"```bash\n{command}\n```", body)
                        self.assertIn(f"Execution:\nexit_code: {execution['exit']}", body)
                        if contract["intent"] == "fix":
                            self.assertIn("Terminal output (stdout/stderr not separated):", body)
                            self.assertNotIn("Recent user commands", body)
                            capture = accepted["captured_output"]
                            self.assertEqual(capture["command"], command)
                            self.assertEqual(capture["exit"], execution["exit"])
                            self.assertEqual(capture["state"], "captured")
                            if sid == "auto-fix-archive":
                                self.assertIn("unrecognized option", body)
                                self.assertIn("--gizp", body)
                            else:
                                self.assertIn("Cannot open: No such file or directory", body)
                                self.assertFalse((root / "incoming" / "report.csv").exists())
                                self.assertEqual(
                                    fixtures.snapshot(root)["archive/report.csv"],
                                    facts["before"]["incoming/report.csv"],
                                )
                        else:
                            self.assertNotIn("Terminal output", body)
                            history = accepted.get("recent_executions", [])
                            if sid == "next-no-goal":
                                self.assertEqual(history, [])
                                self.assertNotIn("Recent user commands", body)
                            else:
                                self.assertEqual(len(history), 1)
                                self.assertEqual(history[0]["command"], scenario["inputs"][1])
                                self.assertNotEqual(history[0]["exit"], 0)
                                self.assertLess(history[0]["command_id"], execution["command_id"])
                                self.assertLess(body.index(scenario["inputs"][1]), body.index("Latest completed command:"))
                                self.assertTrue((root / "backups").is_dir())
                    after = fixtures.snapshot(root)
                    verdict = checks.judge(scenario, observed["answer"], facts, root, after,
                                           result, observed["metrics"], observed)
                    self.assertEqual(verdict.passed, expected_pass, verdict.reasons)
                    self.assertEqual(fixtures.snapshot(root), after)
                    self.assertFalse((root / "logs.tar.gz").exists())
                    self.assertFalse((root / "log-backup.tgz").exists())
                    self.assertFalse((root / "backups" / "reports.tar.gz").exists())
                    self.assertEqual(len([r for r in server.requests if r["op"] == "open"]), 1)
                self.socket.unlink()
        self.assertEqual(len(systems), 1)

    def test_automatic_fix_packet_keeps_error_in_user_and_binding_in_host_observation(self):
        original = "sh -c 'printf \"actual-error\\n\" >&2; exit 7'"
        turns = [{"text": "Here is the repaired command:\n```bash\ntouch not-created\n```"},
                 {"text": "touch not-created"}]
        with ScriptedWorker(self.socket, turns) as server:
            root, _, trace, env, argv = self.case(0, 0, capture_output="last", command_assist=True)
            scenario = {"inputs": [original],
                        "completions": [{"kind": "assist"}],
                        "check": "fix-packet"}
            result = driver.run_repl(argv + ["-i"], root, env, 15, scenario, lambda *_: False)
            self.assertIsNone(result.error, result.transcript)
            self.assertIsNone(result.failure, result.transcript)
            specs = [r["spec"] for r in server.requests if r["op"] == "open"]
            self.assertEqual([s["label"] for s in specs], ["command_assist.fix.background"])
            steps = [r["append"] for r in server.requests if r["op"] == "step"]
            self.assertEqual(len(steps), 2)
            self.assertEqual([r["choice"] for r in server.requests if r["op"] == "choice"],
                             [{"type": "auto"}, {"type": "none"}])
            first = steps[0]
            self.assertEqual(len(first), 1)
            self.assertEqual(list(first[0]), ["User"])
            packet = first[0]["User"]
            for rule in ("Preserve existing data.",
                         "create placeholder input files to bypass an error."):
                self.assertIn(rule, packet)
                self.assertNotIn(rule, specs[0]["system"])
            self.assertEqual(len(steps[-1]), 1)
            self.assertEqual(
                steps[-1][0]["User"],
                "Previous response rejected: command assistance: reply contains Markdown fences\n"
                "Return only shell code for the original repair task. No explanation or Markdown fences. "
                "Return exactly [None] if no repair is supported. Do not call tools.",
            )
            self.assertNotIn(original, steps[-1][0]["User"])
            self.assertTrue(packet.startswith(
                "Previous command (already executed):\n```bash\n" + original + "\n```"
            ))
            self.assertLess(
                packet.index("Terminal output (stdout/stderr not separated):"),
                packet.index("Give a shell command to fix the failure shown above."),
            )
            self.assertEqual(packet.count("Give a shell command to fix the failure shown above."), 1)
            self.assertIn(
                "Fixing the error's cause is sufficient. Preserve the intended result and output format.",
                packet,
            )
            self.assertNotIn(
                "Return the shell input itself, without wrapping the response in inline backticks or Markdown fences.",
                packet,
            )
            self.assertIn("Execution:\nexit_code: 7", packet)
            self.assertIn("Terminal output (stdout/stderr not separated):", packet)
            self.assertNotIn("command_id:", packet)
            self.assertNotIn("duration_ms:", packet)
            self.assertNotIn('state: "captured"', packet)
            self.assertIn("actual-error", packet)
            events = [json.loads(line) for line in trace.read_text().splitlines()]
            observed = observations.assistance_observations(events)
            self.assertEqual(len(observed), 1)
            self.assertTrue(observed[0]["background"])
            self.assertEqual(observed[0]["input_format"], "command_assist_v1")
            self.assertEqual(observed[0]["execution"]["command"], original)
            self.assertEqual(observed[0]["execution"]["execution_cwd"], str(root))
            capture = observed[0]["captured_output"]
            self.assertEqual(capture["command_id"], observed[0]["command_id"])
            self.assertEqual(capture["state"], "captured")
            self.assertEqual(capture["source"], "terminal")
            self.assertGreater(capture["observed_bytes"], 0)
            self.assertGreater(capture["retained_bytes"], 0)
            self.assertIn("duration_ms", capture)
            self.assertFalse(capture["truncated"])
            self.assertFalse((root / "not-created").exists())

    def test_suggest_accepts_complete_programs_without_executing_or_rewriting_them(self):
        cases = [
            ("printf first\nprintf second", 0),
            ("false; printf continued > existing", 0),
            ("false\nprintf continued > existing", 0),
            ("false && printf continued > existing", 0),
            ("git diff --cached\ngit diff", 0),
            ("find . -type f\nwc -l existing", 0),
            ("nosh_h0_f() { printf function > existing; }\nnosh_h0_f", 0),
            ("printf command > existing\n: \"$(printf substitution > marker)\"", 0),
            ("printf '%s\\n' 'a quoted argument' \"a path with spaces\"", 0),
            ("printf '%s' \"`printf substitution > marker`\"", 0),
            ("printf '%s' '`literal backticks`'", 0),
            ("printf single", 0),
            ("[None]", 1),
            ("", 2),
            (" \n\t", 2),
            ("# comment only\n# still no command", 2),
            ("```sh\nprintf ok\n```", 2),
            ("Here is a command:\nprintf ok", 2),
            ("printf ok\nThis prints ok.", 2),
            ("printf ok; nosh_h0_missing", 2),
            ("printf ok\nif true; then", 2),
            ("printf ok; )", 2),
            ("nosh_h0_f\nnosh_h0_f() { :; }", 2),
            ("printf ok\n: \"$(nosh_h0_missing)\"", 2),
        ]
        turns = [{"text": program} for program, code in cases
                 for _ in range(2 if code == 2 else 1)]
        with ScriptedWorker(self.socket, turns) as server:
            for number, (program, code) in enumerate(cases):
                with self.subTest(program=program):
                    root, _, trace, env, argv = self.case(number, 0)
                    (root / "existing").write_text("unchanged")
                    before = len(server.requests)
                    result = driver.run_cli(argv + ["-s"], root, env, 15,
                                            stdin=b"Suggest a complete shell program, without running it.\n")
                    self.assertIsNone(result.error, result.stderr)
                    self.assertEqual(result.exit_code, code, result.stderr)
                    self.assertEqual(result.stdout, program + "\n" if code == 0 else "")
                    self.assertEqual((root / "existing").read_text(), "unchanged")
                    self.assertEqual(sorted(path.name for path in root.iterdir()), ["existing"])
                    requests = list(server.requests)[before:]
                    specs = [request["spec"] for request in requests if request["op"] == "open"]
                    self.assertEqual(len(specs), 1)
                    self.assertEqual(specs[0]["label"], "command_assist.generate.foreground")
                    self.assertEqual([tool["name"] for tool in specs[0]["tools"]],
                                     ["command_help", "read_file", "grep"])
                    self.assertEqual(sum(request["op"] == "step" for request in requests),
                                     2 if code == 2 else 1)
                    self.assertTrue(trace.is_file())
                    if code == 0 and program.startswith("false"):
                        executed = driver.run_cli(argv + ["-c", result.stdout], root, env, 15)
                        conditional = "&&" in program
                        self.assertIsNone(executed.error, executed.stderr)
                        self.assertEqual(executed.exit_code, 1 if conditional else 0)
                        self.assertEqual((root / "existing").read_text(),
                                         "unchanged" if conditional else "continued")

    def test_unavailable_worker_is_not_a_local_fallback(self):
        root, _, _, env, argv = self.case(0, 0)
        result = driver.run_cli(argv + ["-a", "--json", "Inspect"], root, env, 5)
        self.assertNotEqual(result.exit_code, 0)
        self.assertIn("evaluation worker", result.stderr)
        self.assertNotIn("failed to load", result.stderr)

    def test_driver_handles_wrapped_and_multiline_automatic_fix_suggestions_without_execution(self):
        commands = [
            "printf repaired",
            "printf '%s\\n' '" + "a" * 180 + "'",
            "printf '%s\\n' '" + "a" * 7000 + "' > not-executed",
            "printf first\nprintf second",
            "printf '%s\\n' '" + "中文" * 100 + "'\nprintf done",
            "printf '%s\\n' '" + "中文" * 1800 + "'\nprintf done > not-executed",
        ]
        for number, command in enumerate(commands):
            with self.subTest(number=number), ScriptedWorker(self.socket, [{"text": command}]) as server:
                root, _, trace, env, argv = self.case(number, 0, command_assist=True)
                scenario = {"check": "fix-readiness", "inputs": ["sh -c 'exit 7'"],
                            "completions": [{"kind": "assist"}]}
                result = driver.run_repl(argv + ["-i"], root, env, 15, scenario, lambda *_: False)
                self.assertIsNone(result.error, result.transcript)
                self.assertEqual(result.exit_code, 0, result.transcript)
                events = [json.loads(line) for line in trace.read_text().splitlines()]
                observed = observations.assistance_observations(events)
                self.assertEqual(observed[0]["text"], command)
                self.assertTrue(observed[0]["background"])
                self.assertEqual(observed[0]["intent"], "fix")
                self.assertEqual(list(root.iterdir()), [])
            self.socket.unlink()

    def test_manual_fix_with_optional_context_uses_agent_and_captured_failure(self):
        for number, additional in enumerate(("", "The deployment target is eu-west-1; do not change files.")):
            with self.subTest(additional=additional), ScriptedWorker(
                self.socket, [{"text": "The command failed with exit code 7."}]
            ) as server:
                root, _, trace, env, argv = self.case(number, 0, capture_output="last")
                original = "sh -c 'printf \"[execution]\\nnot-json\\n\" >&2; exit 7'"
                scenario = {"mode": "repl", "check": "agent-log",
                            "inputs": [original, "#fix" + (" " + additional if additional else "")],
                            "completions": [{"kind": "shell", "exit_code": 7, "contains": ["not-json"]},
                                            {"kind": "agent"}]}
                result = driver.run_repl(argv + ["-i"], root, env, 15, scenario, lambda *_: False)
                self.assertIsNone(result.error, result.transcript)
                self.assertIsNone(result.failure, result.transcript)
                events = [json.loads(line) for line in trace.read_text().splitlines()]
                info = next(e["info"] for e in events if e["ev"] == "engine")
                observed = observations.observe(
                    result, scenario, trace, seed=0,
                    expected_worker={"pid": info["worker_pid"], "config": info["worker_config"]},
                )
                self.assertEqual(observed["metrics"]["task_status"], "completed")
                self.assertEqual(observed["assistance"], [])
                requests = list(server.requests)
                specs = [request["spec"] for request in requests if request["op"] == "open"]
                self.assertEqual([spec["label"] for spec in specs], ["agent"])
                self.assertEqual([tool["name"] for tool in specs[0]["tools"]],
                                 ["exec", "read_file", "grep", "ask_user"])
                first = next(request["append"] for request in requests if request["op"] == "step")
                expected = "Explain why the command failed and how to fix it."
                if additional:
                    expected += "\n\nAdditional context from the user:\n" + additional
                self.assertEqual([message["User"] for message in first if "User" in message], [expected])
                context = next(message["System"] for message in first if "System" in message)
                self.assertIn("\nexit: 7", context)
                self.assertIn("\nfailed_command:", context)
                self.assertIn("\n[user_output ", context)
                self.assertIn("not-json", context)
            self.socket.unlink()

    def test_next_is_automatic_after_success_and_does_not_execute_its_suggestion(self):
        with ScriptedWorker(self.socket, [{"text": "touch not-created"}]) as server:
            root, _, trace, env, argv = self.case(0, 0, command_assist=True)
            scenario = {"check": "automatic-next", "inputs": ["printf completed"],
                        "completions": [{"kind": "assist"}]}
            result = driver.run_repl(argv + ["-i"], root, env, 15, scenario, lambda *_: False)
            self.assertIsNone(result.error, result.transcript)
            self.assertIsNone(result.failure, result.transcript)
            self.assertEqual(result.exit_code, 0, result.transcript)
            requests = list(server.requests)
            specs = [r["spec"] for r in requests if r["op"] == "open"]
            self.assertEqual([s["label"] for s in specs], ["command_assist.next.background"])
            self.assertEqual([t["name"] for t in specs[0]["tools"]],
                             ["command_help", "read_file", "grep"])
            first = next(r["append"] for r in requests if r["op"] == "step")
            self.assertEqual(len(first), 1)
            self.assertTrue(first[0]["User"].startswith("Suggest a continuation of the same task"))
            events = [json.loads(line) for line in trace.read_text().splitlines()]
            observed = observations.assistance_observations(events)
            self.assertEqual(len(observed), 1)
            self.assertTrue(observed[0]["background"])
            self.assertEqual(observed[0]["input_format"], "command_assist_v1")
            self.assertEqual(observed[0]["execution"]["command"], "printf completed")
            self.assertEqual(observed[0]["execution"]["exit"], 0)
            self.assertEqual(list(root.iterdir()), [])

    def test_next_text_is_an_ordinary_agent_request_not_a_management_command(self):
        with ScriptedWorker(self.socket, [{"text": "Task received."}]) as server:
            root, _, trace, env, argv = self.case(0, 0)
            scenario = {"check": "ordinary-task", "inputs": ["# next"],
                        "completions": [{"kind": "agent"}]}
            result = driver.run_repl(argv + ["-i"], root, env, 15, scenario, lambda *_: False)
            self.assertIsNone(result.error, result.transcript)
            self.assertIsNone(result.failure, result.transcript)
            specs = [r["spec"] for r in server.requests if r["op"] == "open"]
            self.assertEqual([s["label"] for s in specs], ["agent"])
            users = [m["User"] for r in server.requests if r["op"] == "step"
                     for m in r["append"] if "User" in m]
            self.assertEqual(users, ["next"])
            events = [json.loads(line) for line in trace.read_text().splitlines()]
            self.assertEqual(observations.assistance_observations(events), [])
            self.assertEqual(list(root.iterdir()), [])

    def test_command_assist_cannot_prompt_even_with_a_terminal(self):
        question = {"calls": [{"name": "ask_user", "args": {"question": "Which destination?"}}]}
        with ScriptedWorker(self.socket, [question, {"text": "[None]"}]) as server:
            root, _, trace, env, argv = self.case(0, 0)
            child = driver.Child(argv + ["-s", "Ask me for a destination before suggesting."],
                                 root, env, True)
            try:
                child.until(lambda: child.result.exit_code is not None, child.start + 15, "exit")
                self.assertEqual(child.result.exit_code, 2, child.result.transcript)
                self.assertNotIn("answer>", child.result.transcript)
            finally:
                child.close()
            requests = list(server.requests)
            specs = [r["spec"] for r in requests if r["op"] == "open"]
            self.assertTrue(all(t["name"] != "ask_user" for s in specs for t in s["tools"]))
            self.assertFalse(any("UserAnswer" in m for r in requests if r["op"] == "step"
                                 for m in r["append"]))
            events = [json.loads(line) for line in trace.read_text().splitlines()]
            outcomes = [e["value"] for e in events if e["ev"] == "observation"]
            self.assertEqual([o["status"] for o in outcomes], ["failed"])
            self.assertTrue(all("kind" not in o for o in outcomes))
            self.assertEqual(list(root.iterdir()), [])

    def test_trial_pipeline_preserves_native_clarification_grading(self):
        turns = [{"calls": [{"name": "ask_user", "args": {"question": "你希望我具体处理什么任务？"}}]},
                 {"text": "已停止本次任务，未执行任何操作。"}]
        config = {"model": "minicpm5-2b:q4_k_m", "weights": str(self.weights),
                  "tokenizer": str(self.base / "tokenizer.json"), "device": "cpu",
                  "context_length": 8192, "kv_dtype": "F16", "prefill_chunk": 512, "prepack_weights": True,
                  "threads": "1", "rayon_threads": "1", "cuda_visible_devices": None, "cuda_device_order": None}
        worker = SimpleNamespace(path=self.socket, record={"config": config})
        scenario = SCENARIOS["zh-clarify-task"]
        with ScriptedWorker(self.socket, turns) as server, fixtures.Workspace(self.base / "work") as workspace:
            worker.record["pid"] = server.process.pid
            row = trial.run_trial(SimpleNamespace(threads=1, device="cpu"), {"settings": {"timeout_s": 15}},
                                  scenario, 0, 0, workspace, self.base / "report",
                                  Path(BINARY).resolve(), self.weights, worker=worker)
            answers = [message["UserAnswer"] for request in server.requests if request["op"] == "step"
                       for message in request["append"] if "UserAnswer" in message]
            self.assertEqual([json.loads(answer) for answer in answers], [{
                "question": turns[0]["calls"][0]["args"]["question"],
                "choices": [],
                "answer": scenario["completions"][0]["answers"][0],
            }])
        self.assertEqual(row["status"], "pass", row["reasons"])
        self.assertEqual(row["metrics"]["load_s"], 0)
        self.assertEqual(row["engines"][0]["worker_config"], config)
        self.assertEqual(row["metrics"]["steps"], 2)
        self.assertFalse(workspace.case_path(scenario["id"]).exists())

    def test_automatic_next_receives_history_after_a_prerequisite_is_fixed(self):
        from eval.suite import load_suite
        scenario = next(s for s in load_suite("command-assist")["scenarios"]
                        if s["id"] == "next-retry-after-prerequisite")
        config = {"model": "minicpm5-2b:q4_k_m", "weights": str(self.weights),
                  "tokenizer": str(self.base / "tokenizer.json"), "device": "cpu",
                  "context_length": 8192, "kv_dtype": "F16", "prefill_chunk": 512, "prepack_weights": True,
                  "threads": "1", "rayon_threads": "1", "cuda_visible_devices": None, "cuda_device_order": None}
        worker = SimpleNamespace(path=self.socket, record={"config": config})
        with ScriptedWorker(self.socket, [{"text": "tar -czf backups/reports.tar.gz archive"}]) as server, \
                fixtures.Workspace(self.base / "work") as workspace:
            worker.record["pid"] = server.process.pid
            row = trial.run_trial(SimpleNamespace(threads=1, device="cpu"), {"settings": {"timeout_s": 15}},
                                  scenario, 0, 0, workspace, self.base / "report",
                                  Path(BINARY).resolve(), self.weights, worker=worker)
            self.assertEqual(row["status"], "pass", row["reasons"])
            self.assertEqual(len(row["assistance"]), 1)
            observation = row["assistance"][0]
            self.assertEqual(observation["execution"]["command"], "mkdir -p backups")
            self.assertEqual([item["command"] for item in observation["recent_executions"]],
                             [scenario["inputs"][1]])
            self.assertNotEqual(observation["recent_executions"][0]["exit"], 0)
            task = next(r["append"][0]["User"] for r in server.requests if r["op"] == "step")
            self.assertIn("Recent user commands", task)
            self.assertLess(task.index("Recent user commands"), task.index("Latest completed command:"))
            self.assertLess(task.index(scenario["inputs"][1]), task.index("Latest completed command:"))
            self.assertEqual(task.count(scenario["inputs"][1]), 1)
            self.assertNotIn(scenario["id"], task)
        self.assertFalse(workspace.case_path(scenario["id"]).exists())

    def test_observe_rejects_an_unexpected_model_task_during_setup(self):
        with ScriptedWorker(self.socket, [{"text": "Ready."}]):
            root, _, _, env, argv = self.case(0, 0)
            scenario = {"check": "setup-contract", "inputs": ["# Say ready."],
                        "completions": [{"kind": "observe"}]}
            result = driver.run_repl(argv + ["-i"], root, env, 15, scenario, lambda *_: False)
            self.assertIsNone(result.error, result.transcript)
            self.assertIn("setup unexpectedly started a model task", result.failure)
