"""Own one serial inference worker; never restart or retry a trial."""

from __future__ import annotations

import json
from pathlib import Path
import socket
import subprocess
import tempfile
import time

from . import runtime
from .contracts import finite_number


class Worker:
    def __init__(self, args, binary: Path, weights: Path, output: Path, metadata: dict):
        self.args, self.binary, self.weights = args, binary, weights
        self.output, self.metadata = output, metadata
        self.proc = self.temporary = self.stdout = self.stderr = None
        self.path = None
        self.record = {"status": "starting", "warmup_generations": 0}
        metadata["worker"] = self.record

    def __enter__(self):
        started = time.monotonic()
        try:
            self.temporary = tempfile.TemporaryDirectory(prefix="nosh-engine-")
            home = Path(self.temporary.name)
            self.path = home / "engine.sock"
            env = runtime.environment(home, self.args.threads, None, device=self.args.device)
            self.stdout = (self.output / "worker.jsonl").open("xb")
            self.stderr = (self.output / "worker.stderr.txt").open("xb")
            self.proc = subprocess.Popen(
                [str(self.binary), "--offline", "--no-download", "--norc",
                 "--model-path", str(self.weights), "debug", "eval-worker", "--socket", str(self.path)],
                cwd=home, env=env, stdin=subprocess.PIPE, stdout=self.stdout, stderr=self.stderr,
                start_new_session=True,
            )
            deadline = started + self.args.worker_start_timeout
            while True:
                self.alive()
                with (self.output / "worker.jsonl").open("rb") as stream:
                    line = stream.readline(1024 * 1024)
                if line.endswith(b"\n"):
                    ready = json.loads(line)
                    if (not isinstance(ready, dict) or ready.get("ev") != "ready" or ready.get("version") != 1
                            or ready.get("pid") != self.proc.pid or ready.get("warmup_generations") != 0
                            or not isinstance(ready.get("info"), dict) or not isinstance(ready.get("config"), dict)
                            or not finite_number(ready["info"].get("load_s")) or ready["info"]["load_s"] < 0):
                        raise RuntimeError("invalid evaluation worker readiness")
                    self.record.update(ready, status="ready", startup_s=time.monotonic() - started)
                    self.record["initial"] = self.checkpoint()
                    return self
                if time.monotonic() >= deadline:
                    raise TimeoutError("evaluation worker startup exceeded its deadline")
                time.sleep(.02)
        except BaseException:
            self.record.update(status="startup_failed", startup_s=time.monotonic() - started)
            self.close()
            raise

    def alive(self):
        if self.proc is None or self.proc.poll() is not None:
            raise RuntimeError("evaluation worker exited; see worker.stderr.txt (no restart or fallback)")

    def checkpoint(self):
        """A status reply is sent only after previous inference and session cleanup."""
        self.alive()
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(10)
            connection.connect(str(self.path))
            connection.sendall(b'{"op":"status"}\n')
            with connection.makefile("rb") as stream:
                line = stream.readline(1024 * 1024)
        if not line.endswith(b"\n"):
            raise RuntimeError("evaluation worker disconnected or returned an oversized status")
        reply = json.loads(line)
        state = reply.get("Status") if isinstance(reply, dict) else None
        if (not isinstance(state, dict) or state.get("pid") != self.proc.pid
                or state.get("active_sessions") != 0
                or any(type(state.get(key)) is not int or state[key] < 0
                       for key in ("connections", "closed_sessions"))):
            raise RuntimeError("invalid evaluation worker cleanup receipt")
        self.alive()
        return state

    def close(self):
        started = time.monotonic()
        if self.proc is not None:
            if self.proc.poll() is None:
                self.proc.terminate()
                try:
                    self.proc.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    self.record["forced_shutdown"] = True
                    self.proc.kill()
                    self.proc.wait(timeout=5)
            self.record["exit_code"] = self.proc.returncode
            if self.proc.stdin is not None:
                self.proc.stdin.close()
        for stream in (self.stdout, self.stderr):
            if stream is not None:
                stream.close()
        if self.temporary is not None:
            self.temporary.cleanup()
        self.record["shutdown_s"] = time.monotonic() - started

    def __exit__(self, exc_type, *_):
        try:
            if exc_type is None:
                self.record["final"] = self.checkpoint()
                self.record["status"] = "completed"
            else:
                self.record["status"] = "aborted"
        except (OSError, ValueError, RuntimeError):
            self.record["status"] = "cleanup_failed"
            raise
        finally:
            self.close()
