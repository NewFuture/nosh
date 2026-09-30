"""Model-free resident protocol, lifecycle, accounting and production CLI probes."""

import contextlib
import copy
import io
import json
import multiprocessing
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from types import SimpleNamespace
from unittest.mock import patch

from eval import campaign, driver, fixtures, observations, report, resident, runtime, trial
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
        data["trials"][0]["engines"] = [{"load_s": 1}]
        with self.assertRaisesRegex(ValueError, "mixed cold/resident"):
            report.validate(data)


WORKER_SCRIPT = r"""
import json, os, socket, sys, time
path, mode = sys.argv[1:]
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

    def case(self, number, seed):
        root, home = self.base / f"case{number}", self.base / f"home{number}"
        root.mkdir()
        home.mkdir()
        trace = home / "engine.jsonl"
        env = runtime.environment(home, 1, trace)
        env["NOSH_EVAL_WORKER"] = str(self.socket)
        argv = [str(Path(BINARY).resolve()), "--offline", "--no-download", "--norc",
                "--model-path", str(self.weights), "--seed", str(seed)]
        return root, home, trace, env, argv

    def test_fresh_shell_home_cwd_environment_approvals_and_seed(self):
        commands = [
            "export RESIDENT_TEST=leak; cd data; printf '%s' \"$HOME\" > home.txt",
            "printf '%s' \"${RESIDENT_TEST-unset}\" > state.txt; printf '%s' \"$HOME\" > home.txt; pwd > cwd.txt",
            "touch denied.txt",
        ]
        turns = [turn for command in commands for turn in (
            {"calls": [{"name": "run_command", "args": {"command": command}}]}, {"text": "Complete."})]
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

    def test_native_query_and_finish_command_assist_are_preserved(self):
        command = "tar -czf logs.tar.gz logs"
        turns = [{"calls": [{"name": "command_info", "args": {"name": "tar", "query": "help"}}]},
                 {"calls": [{"name": "finish", "args": {"kind": "command", "text": command}}]}]
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
            self.assertEqual([t["name"] for t in specs[-1]["tools"]], ["command_info", "read_file", "grep", "finish"])
            self.assertEqual([r["choice"] for r in server.requests if r["op"] == "choice"],
                             [{"type": "required"}, {"type": "required"}])
            tool_results = [message["Tool"] for r in server.requests if r["op"] == "step"
                            for message in r["append"] if "Tool" in message]
            self.assertEqual(len(tool_results), 1)
            self.assertRegex(tool_results[0], r"^\[query program=.+/tar exit=0 truncated=")

    def test_unavailable_worker_is_not_a_local_fallback(self):
        root, _, _, env, argv = self.case(0, 0)
        result = driver.run_cli(argv + ["-a", "--json", "Inspect"], root, env, 5)
        self.assertNotEqual(result.exit_code, 0)
        self.assertIn("evaluation worker", result.stderr)
        self.assertNotIn("failed to load", result.stderr)

    def test_trial_pipeline_preserves_native_clarification_grading(self):
        turns = [{"text": "你希望我具体处理什么任务？"}]
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
        self.assertEqual(row["status"], "pass", row["reasons"])
        self.assertEqual(row["metrics"]["load_s"], 0)
        self.assertEqual(row["engines"][0]["worker_config"], config)
        self.assertEqual(row["metrics"]["steps"], 1)
        self.assertFalse((self.base / "work" / scenario["id"]).exists())
