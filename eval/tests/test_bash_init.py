"""Real Bash -> production nosh -s; only the inference boundary is scripted."""

from contextlib import contextmanager
import json
import multiprocessing
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

from eval import driver, observations, runtime
from eval.tests.test_resident import ScriptedWorker


BINARY = os.environ.get("NOSH_TEST_BINARY")


class PausedWorker(ScriptedWorker):
    def __init__(self, path, turns):
        context = multiprocessing.get_context("fork")
        self.waiting = context.Event()
        self.resume = context.Event()
        super().__init__(path, turns)

    def send(self, connection, value):
        if "Text" in value.get("Event", {}):
            self.waiting.set()
            if not self.resume.wait(5):
                raise AssertionError("test did not release inference response")
        super().send(connection, value)


@unittest.skipUnless(sys.platform == "linux" and BINARY,
                     "set NOSH_TEST_BINARY to run real Bash/CLI without a model")
class BashInitTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory(prefix="nosh-bash-cli-")
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.home = self.base / "home"
        self.home.mkdir()
        self.root = self.base / "work"
        self.root.mkdir()
        self.trace = self.home / "trace.jsonl"
        self.env = runtime.environment(self.home, 1, self.trace)
        self.weights = self.base / "model.gguf"
        self.weights.write_bytes(b"not a model; a local inference fallback must fail")
        (self.base / "tokenizer.json").write_text("{}")
        self.socket = self.base / "worker.sock"
        self.env.update(NOSH_EVAL_WORKER=str(self.socket),
                        NOSH_MODEL_PATH=str(self.weights), INPUTRC=os.devnull)
        self.argv = [str(Path(BINARY).resolve()), "--offline", "--no-download"]

    @contextmanager
    def shell(self):
        result = subprocess.run(self.argv + ["init", "bash"], cwd=self.root, env=self.env,
                                capture_output=True, check=True, timeout=10)
        self.assertEqual(result.stderr, b"")
        self.assertFalse(self.trace.exists(), "init must not open the inference boundary")
        (self.root / "integration.sh").write_bytes(result.stdout)
        (self.root / "rc").write_text(r"""
PS1='BASH> '
PS2='MORE> '
source ./integration.sh
private_value=not-exported
__test_snapshot() {
    printf '%s\0%s\0' "$READLINE_LINE" "$READLINE_POINT" > snapshot
    printf '\nSNAPSHOT_DONE\n'
}
bind -x '"\e[21~":__test_snapshot'
printf '\nINIT_DONE\n'
""")
        child = driver.Child(
            [shutil.which("bash"), "--noprofile", "--rcfile", "rc", "-i"],
            self.root, self.env, True)
        try:
            self.wait(child, lambda: "\nINIT_DONE\r" in child.result.transcript, "Bash init")
            self.wait(child, lambda: "BASH> " in child.result.transcript, "Bash prompt")
            self.assertNotIn("no bindings installed", child.result.transcript)
            self.assertNotIn("keeping existing binding", child.result.transcript)
            yield child
        finally:
            child.close()

    def wait(self, child, condition, label):
        try:
            child.until(condition, time.monotonic() + 15, label)
        except (TimeoutError, driver.DriverError) as error:
            self.fail(f"{error}\n{child.result.transcript!r}")

    def snapshot(self, child):
        count = child.result.transcript.count("\nSNAPSHOT_DONE\r")
        child.send(b"\x1b[21~")
        self.wait(child, lambda: child.result.transcript.count("\nSNAPSHOT_DONE\r") > count,
                  "Readline snapshot")
        marker = child.result.transcript.rfind("\nSNAPSHOT_DONE\r")
        self.wait(child, lambda: "BASH> " in child.result.transcript[marker:], "snapshot redraw")
        line, point, _ = (self.root / "snapshot").read_bytes().split(b"\0")
        return line.decode(), int(point)

    def suggest(self, child, draft):
        child.send(draft.replace("\n", "\x16\n").encode())
        child.send(b"\x02")
        before = self.snapshot(child)
        child.send(b"\x1bOQ")
        return before

    def test_real_suggest_preserves_description_and_queries_without_executing_result(self):
        command = "printf '%s\\n' '\u4e2d\u6587' > created"
        turns = [
            {"calls": [{"name": "read_file", "args": {"path": "evidence.txt"}}]},
            {"text": command},
        ]
        (self.root / "evidence.txt").write_text("A plain text output is wanted.")
        with ScriptedWorker(self.socket, turns) as server, self.shell() as child:
            self.assertEqual(list(server.requests), [])
            draft = "-n \u4e2d\u6587 'quotes' \"double\" $(touch injection)\nsecond line"
            self.suggest(child, draft)
            self.wait(child, lambda: " > created" in child.result.transcript, "suggestion")
            self.assertEqual(self.snapshot(child)[0], command)
            self.assertFalse((self.root / "created").exists())
            self.assertFalse((self.root / "injection").exists())
            requests = list(server.requests)
            specs = [request["spec"] for request in requests if request["op"] == "open"]
            self.assertEqual([spec["label"] for spec in specs],
                             ["command_assist.generate.foreground"])
            self.assertEqual([tool["name"] for tool in specs[0]["tools"]],
                             ["command_help", "read_file", "grep"])
            first = next(request["append"] for request in requests if request["op"] == "step")
            self.assertIn(draft, first[0]["User"])
            self.assertIn(str(self.root), first[0]["User"])
            self.assertNotIn("private_value", first[0]["User"])
            self.assertFalse(any(message.get("System", "").startswith("command_completed")
                                 for request in requests for message in request.get("append", [])))
            child.send(b" # checked")
            self.assertEqual(self.snapshot(child)[0], command + " # checked")
            child.send(b"\r")
            self.wait(child, lambda: (self.root / "created").exists(), "manual execution")
            self.assertEqual((self.root / "created").read_text(), "\u4e2d\u6587\n")

    def test_invalid_responses_are_rejected_by_production_validator(self):
        for response in [
            "```bash\ntouch forbidden\n```",
            "if true; then",
            "noshsurelymissingcommand",
            "printf '\\n'\x1b[31m",
            "x" * 16385,
            "",
        ]:
            with self.subTest(response=response[:40]):
                with ScriptedWorker(self.socket, [{"text": response}, {"text": response}]), self.shell() as child:
                    before = self.suggest(child, "\u4e2d\u6587 original draft")
                    self.wait(child, lambda: "input unchanged" in child.result.transcript, "rejection")
                    self.assertEqual(self.snapshot(child), before)
                    self.assertFalse((self.root / "forbidden").exists())
                    events = [json.loads(line) for line in self.trace.read_text().splitlines()]
                    assistance = observations.assistance_observations(events)
                    self.assertEqual(len(assistance), 1)
                    self.assertEqual(assistance[0]["status"], "failed")
                self.socket.unlink()
                self.trace.unlink()

    def test_no_suggestion_and_disabled_ai_preserve_input(self):
        with ScriptedWorker(self.socket, [{"text": "[None]"}]) as server, self.shell() as child:
            before = self.suggest(child, "describe something")
            self.wait(child, lambda: "input unchanged" in child.result.transcript, "no suggestion")
            self.assertEqual(self.snapshot(child), before)
            prompts = child.result.transcript.count("BASH> ")
            child.send(b"\x03")
            self.wait(child, lambda: child.result.transcript.count("BASH> ") > prompts, "cancel draft")
            child.send(b"export NOSH_DISABLE_AI=1; printf '\\nDISABLED\\n'\r")
            self.wait(child, lambda: "\nDISABLED\r" in child.result.transcript, "AI disabled")
            count = len([request for request in server.requests if request["op"] == "open"])
            child.result.transcript = ""
            before = self.suggest(child, "must not load AI")
            self.wait(child, lambda: "AI is disabled" in child.result.transcript, "disabled suggestion")
            self.wait(child, lambda: "input unchanged" in child.result.transcript, "unchanged input")
            self.assertEqual(self.snapshot(child), before)
            self.assertEqual(len([request for request in server.requests if request["op"] == "open"]), count)

    def test_multiline_program_is_not_flattened_or_executed(self):
        command = "cat > created <<'NOSH_END'\nfirst\n\u4e2d\u6587\nNOSH_END"
        with ScriptedWorker(self.socket, [{"text": command}]), self.shell() as child:
            self.suggest(child, "prepare multiline content")
            self.wait(child, lambda: "NOSH_END" in child.result.transcript, "heredoc suggestion")
            self.assertEqual(self.snapshot(child)[0], command)
            self.assertFalse((self.root / "created").exists())
            child.send(b"\r")
            self.wait(child, lambda: (self.root / "created").exists(), "manual heredoc")
            self.assertEqual((self.root / "created").read_text(), "first\n\u4e2d\u6587\n")

    def test_ctrl_c_cancels_real_suggest_and_restores_subsequent_bash_interrupts(self):
        with PausedWorker(self.socket, [{"text": "touch cancelled-result"}]) as server, self.shell() as child:
            try:
                before = self.suggest(child, "\u4e2d\u6587 cancelled request")
                self.wait(child, server.waiting.is_set, "inference response")
                child.send(b"\x03")
                server.resume.set()
                self.wait(child, lambda: "suggestion cancelled" in child.result.transcript, "cancellation")
                self.assertEqual(self.snapshot(child), before)
                self.assertFalse((self.root / "cancelled-result").exists())
                prompts = child.result.transcript.count("BASH> ")
                child.send(b"\x03")
                self.wait(child, lambda: child.result.transcript.count("BASH> ") > prompts,
                          "ordinary Bash Ctrl-C after suggestion")
                self.assertEqual(self.snapshot(child)[0], "")
            finally:
                server.resume.set()

    def test_existing_protected_paths_still_block_query_tools(self):
        protected = self.root / "protected.txt"
        protected.write_text("PRIVATE_FIXTURE_DO_NOT_DISCLOSE")
        with (self.home / "nosh" / "config.toml").open("a") as config:
            config.write("\n[safety]\nprotected_paths = [" + json.dumps(str(protected)) + "]\n")
        turns = [
            {"calls": [{"name": "read_file", "args": {"path": str(protected)}}]},
            {"text": "[None]"},
        ]
        with ScriptedWorker(self.socket, turns) as server, self.shell() as child:
            before = self.suggest(child, "inspect the protected input")
            self.wait(child, lambda: "input unchanged" in child.result.transcript, "denied query")
            self.assertEqual(self.snapshot(child), before)
            results = [message["Tool"] for request in server.requests if request["op"] == "step"
                       for message in request["append"] if "Tool" in message]
            self.assertEqual(len(results), 1)
            self.assertNotIn("PRIVATE_FIXTURE_DO_NOT_DISCLOSE", results[0])
            self.assertIn("permission", results[0].lower())

    def test_agent_pipeline_still_accepts_init_as_task_text_without_execution_tools(self):
        with ScriptedWorker(self.socket, [{"text": "pipeline summary"}]) as server:
            result = subprocess.run(self.argv + ["-a", "init"], cwd=self.root, env=self.env,
                                    input=b"piped context", capture_output=True, timeout=15)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertIn(b"pipeline summary", result.stdout)
            specs = [request["spec"] for request in server.requests if request["op"] == "open"]
            self.assertEqual([spec["label"] for spec in specs], ["agent"])
            self.assertNotIn("exec", [tool["name"] for tool in specs[0]["tools"]])
            self.assertNotIn("ask_user", [tool["name"] for tool in specs[0]["tools"]])
