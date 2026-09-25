"""Bounded PTY/pipe drivers. Completion is protocol-driven, not sleep-driven."""

from __future__ import annotations

import codecs
from dataclasses import dataclass, field
import errno
import os
from pathlib import Path
import re
import selectors
import signal
import struct
import subprocess
import time
import unicodedata
import uuid

PROMPT = "__NOSH_EVAL_PROMPT__ "
OUTPUT_LIMIT = 8 * 1024 * 1024
ANSI = re.compile(r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b\[[0-?]*[ -/]*[@-~]|\x1b[78=>]")
SUMMARY = re.compile(r"(?m)^[┃|] ([✔⚠✗+!x]) .*?(\d+) steps [·|] ([\d.]+) s")
STATS = re.compile(r"(?m)^[┃|] stats:")
SHELL_EXIT = re.compile(r"(?m)^\s*[✗x] exit (\d+) [·|] ")


def plain(text: str) -> str:
    return ANSI.sub("", text).replace("\r\n", "\n").replace("\r", "\n")


class DriverError(RuntimeError):
    pass


class Screen:
    """The small VT subset used by reedline, including cursor-query replies."""

    def __init__(self, height=40, width=160):
        self.height, self.width = height, width
        self.lines = [[" "] * width for _ in range(height)]
        self.row = self.col = 0
        self.saved = (0, 0)
        self.pending = ""

    def line(self) -> str:
        return "".join(self.lines[self.row]).rstrip()

    def newline(self):
        self.row += 1
        if self.row >= self.height:
            self.lines.pop(0)
            self.lines.append([" "] * self.width)
            self.row = self.height - 1

    def feed(self, text: str) -> bytes:
        text = self.pending + text
        self.pending = ""
        reply = bytearray()
        i = 0
        while i < len(text):
            ch = text[i]
            if ch == "\x1b":
                if i + 1 == len(text):
                    self.pending = text[i:]
                    break
                if text[i + 1] == "[":
                    m = re.match(r"\x1b\[([0-?]*)([ -/]*)([@-~])", text[i:])
                    if not m:
                        self.pending = text[i:]
                        break
                    params, _, code = m.groups()
                    values = [int(n) if n.isdigit() else 0 for n in params.lstrip("?").split(";")]
                    n = values[0] or 1
                    if code == "n" and params == "6":
                        reply.extend(f"\x1b[{self.row + 1};{min(self.col + 1, self.width)}R".encode())
                    elif code == "n" and params == "5":
                        reply.extend(b"\x1b[0n")
                    elif code == "c":
                        reply.extend(b"\x1b[?1;2c")
                    elif code in "Hf":
                        self.row = min(n - 1, self.height - 1)
                        self.col = min((values[1] if len(values) > 1 and values[1] else 1) - 1, self.width - 1)
                    elif code in "AB":
                        self.row = max(0, min(self.height - 1, self.row + (n if code == "B" else -n)))
                    elif code in "CD":
                        self.col = max(0, min(self.width - 1, self.col + (n if code == "C" else -n)))
                    elif code in "G`":
                        self.col = min(n - 1, self.width - 1)
                    elif code == "d":
                        self.row = min(n - 1, self.height - 1)
                    elif code == "J":
                        if values[0] in (2, 3):
                            self.lines = [[" "] * self.width for _ in range(self.height)]
                        elif values[0] == 0:
                            self.lines[self.row][self.col:] = [" "] * (self.width - self.col)
                            for r in range(self.row + 1, self.height):
                                self.lines[r] = [" "] * self.width
                    elif code == "K":
                        start = 0 if values[0] in (1, 2) else self.col
                        end = self.col + 1 if values[0] == 1 else self.width
                        self.lines[self.row][start:end] = [" "] * (end - start)
                    elif code == "s":
                        self.saved = (self.row, self.col)
                    elif code == "u":
                        self.row, self.col = self.saved
                    i += len(m.group())
                    continue
                if text[i + 1] == "]":
                    end = re.search(r"\x07|\x1b\\", text[i + 2:])
                    if not end:
                        self.pending = text[i:]
                        break
                    i += 2 + end.end()
                    continue
                if text[i + 1] == "7":
                    self.saved = (self.row, self.col)
                elif text[i + 1] == "8":
                    self.row, self.col = self.saved
                i += 2
                continue
            if ch == "\r":
                self.col = 0
            elif ch == "\n":
                self.newline()
            elif ch == "\b":
                self.col = max(0, self.col - 1)
            elif ch == "\t":
                self.col = min(self.width - 1, (self.col // 8 + 1) * 8)
            elif ch >= " " and not unicodedata.combining(ch):
                width = 2 if unicodedata.east_asian_width(ch) in "WF" else 1
                if self.col + width > self.width:
                    self.col = 0
                    self.newline()
                self.lines[self.row][self.col] = ch
                if width == 2:
                    self.lines[self.row][self.col + 1] = ""
                self.col += width
            i += 1
        return bytes(reply)


@dataclass
class Result:
    stdout: str = ""
    stderr: str = ""
    transcript: str = ""
    exit_code: int | None = None
    total_s: float = 0
    peak_rss_mib: float | None = None
    approvals: list[dict] = field(default_factory=list)
    turns: list[dict] = field(default_factory=list)
    pwd: str | None = None
    error: str | None = None
    failure: str | None = None


def input_contracts(scenario: dict) -> list[dict]:
    if "completions" in scenario:
        return scenario["completions"]
    if scenario.get("corrections"):
        return [{"kind": "correction"} for _ in scenario["inputs"]]
    return [
        {"kind": "shell", "exit_code": 1, "contains": ["FileNotFoundError"]}
        if scenario["check"] == "failure" and i == 0 else {"kind": "agent"}
        for i, _ in enumerate(scenario["inputs"])
    ]


def owned_pids(token: str) -> list[int]:
    marker = f"NOSH_EVAL_RUN={token}".encode()
    found = []
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            if entry.stat().st_uid == os.getuid() and marker in (entry / "environ").read_bytes().split(b"\0"):
                found.append(int(entry.name))
        except (FileNotFoundError, ProcessLookupError, PermissionError):
            continue
    return found


def stop_owned(token: str) -> None:
    marker = f"NOSH_EVAL_RUN={token}".encode()
    for sig in (signal.SIGTERM, signal.SIGKILL):
        pids = owned_pids(token)
        if not pids:
            return
        for pid in pids:
            fd = None
            try:
                fd = os.pidfd_open(pid)
                if marker in Path(f"/proc/{pid}/environ").read_bytes().split(b"\0"):
                    signal.pidfd_send_signal(fd, sig)
            except (FileNotFoundError, ProcessLookupError):
                pass
            finally:
                if fd is not None:
                    os.close(fd)
        if sig == signal.SIGTERM:
            time.sleep(0.2)


class Child:
    def __init__(self, argv: list[str], cwd: Path, env: dict, tty: bool, stdin: bytes = b""):
        self.result = Result()
        self.start = time.monotonic()
        self.token = uuid.uuid4().hex
        env = dict(env, NOSH_EVAL_RUN=self.token)
        self.selector = selectors.DefaultSelector()
        self.decoders = {name: codecs.getincrementaldecoder("utf-8")("replace") for name in ("stdout", "stderr", "transcript")}
        self.screen = Screen() if tty else None
        self.proc = None
        self.pid = None
        self.fd = None
        self.bytes = 0
        self.pending_input = memoryview(stdin)
        try:
            self.start_process(argv, cwd, env, tty, stdin)
        except (OSError, ValueError, KeyboardInterrupt):
            self.close()
            raise

    def start_process(self, argv, cwd, env, tty, stdin):
        if tty:
            import fcntl
            import pty
            import termios

            self.pid, self.fd = pty.fork()
            if self.pid == 0:
                try:
                    os.chdir(cwd)
                    os.execve(argv[0], argv, env)
                except OSError as exc:
                    os.write(2, f"eval: could not start nosh: {exc}\n".encode())
                    os._exit(127)
            fcntl.ioctl(self.fd, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
            os.set_blocking(self.fd, False)
            self.selector.register(self.fd, selectors.EVENT_READ, "transcript")
        else:
            self.proc = subprocess.Popen(argv, cwd=cwd, env=env, stdin=subprocess.PIPE,
                                         stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
            self.pid = self.proc.pid
            for stream, name in ((self.proc.stdout, "stdout"), (self.proc.stderr, "stderr")):
                os.set_blocking(stream.fileno(), False)
                self.selector.register(stream, selectors.EVENT_READ, name)
            if stdin:
                os.set_blocking(self.proc.stdin.fileno(), False)
                self.selector.register(self.proc.stdin, selectors.EVENT_WRITE, "stdin")
            else:
                self.proc.stdin.close()

    def send(self, data: bytes):
        if self.fd is None:
            raise DriverError("attempted terminal input on a pipe")
        try:
            sent = os.write(self.fd, data)
        except BlockingIOError as exc:
            raise DriverError("terminal input buffer is full") from exc
        if sent != len(data):
            raise DriverError("short terminal write")

    def reap(self):
        if self.pid is not None and self.result.exit_code is None:
            pid, status, usage = os.wait4(self.pid, os.WNOHANG)
            if pid:
                self.result.exit_code = os.waitstatus_to_exitcode(status)
                self.result.peak_rss_mib = usage.ru_maxrss / 1024
                self.result.total_s = time.monotonic() - self.start
                if self.proc:
                    self.proc.returncode = self.result.exit_code

    def pump(self):
        for key, _ in self.selector.select(0.05):
            name = key.data
            if name == "stdin":
                try:
                    sent = os.write(key.fd, self.pending_input[:4096])
                except BrokenPipeError:
                    sent = len(self.pending_input)
                self.pending_input = self.pending_input[sent:]
                if not self.pending_input:
                    self.selector.unregister(key.fileobj)
                    self.proc.stdin.close()
                continue
            try:
                chunk = os.read(key.fd, 65536)
            except BlockingIOError:
                continue
            except OSError as exc:
                if name != "transcript" or exc.errno != errno.EIO:
                    raise
                chunk = b""
            if not chunk:
                self.selector.unregister(key.fileobj)
                text = self.decoders[name].decode(b"", final=True)
            else:
                self.bytes += len(chunk)
                if self.bytes > OUTPUT_LIMIT:
                    raise DriverError(f"output exceeded {OUTPUT_LIMIT} bytes")
                text = self.decoders[name].decode(chunk)
            setattr(self.result, name, getattr(self.result, name) + text)
            if self.screen:
                reply = self.screen.feed(text)
                if reply:
                    self.send(reply)
        self.reap()

    def until(self, condition, deadline: float, label: str, on_output=None):
        while not condition():
            if time.monotonic() >= deadline:
                raise TimeoutError(f"timed out waiting for {label}")
            self.pump()
            if on_output:
                on_output()
            if self.result.exit_code is not None and not condition():
                raise DriverError(f"nosh exited {self.result.exit_code} before {label}")

    def close(self):
        stop_owned(self.token)
        self.reap()
        if self.pid is not None and self.result.exit_code is None:
            # The child may have been stopped before exec, before its environment
            # marker was installed. It is still our unreaped child, not a reused PID.
            os.kill(self.pid, signal.SIGKILL)
            _, status, usage = os.wait4(self.pid, 0)
            self.result.exit_code = os.waitstatus_to_exitcode(status)
            self.result.peak_rss_mib = usage.ru_maxrss / 1024
            self.result.total_s = time.monotonic() - self.start
            if self.proc:
                self.proc.returncode = self.result.exit_code
        self.selector.close()
        if self.fd is not None:
            os.close(self.fd)
        if self.proc:
            for stream in (self.proc.stdin, self.proc.stdout, self.proc.stderr):
                stream.close()


def run_cli(argv: list[str], cwd: Path, env: dict, timeout: float, stdin: bytes = b"") -> Result:
    child = Child(argv, cwd, env, False, stdin)
    try:
        deadline = child.start + timeout
        while child.result.exit_code is None or child.selector.get_map():
            if time.monotonic() >= deadline:
                raise TimeoutError("CLI process or its output pipes did not finish")
            child.pump()
    except (TimeoutError, DriverError, OSError) as exc:
        child.result.error = str(exc)
    finally:
        child.close()
    return child.result


def run_repl(argv: list[str], cwd: Path, env: dict, timeout: float, scenario: dict, approve) -> Result:
    child = Child(argv, cwd, dict(env, PS1=PROMPT), True)
    result = child.result
    deadline = child.start + timeout
    approval_offset = 0
    denial_pending = False

    def prompt():
        return child.screen.line().startswith(PROMPT.rstrip())

    def idle_prompt():
        line = child.screen.line()
        return line.startswith(PROMPT.rstrip()) and line[len(PROMPT.rstrip()):].strip() in ("", "confirm")

    def approvals():
        nonlocal approval_offset, denial_pending
        text = plain(result.transcript)
        fatal = re.search(r"(?m)^nosh: (?:[^\n]*config\.toml:|failed to load|AI is disabled)[^\n]*", text)
        if fatal:
            raise DriverError(fatal.group())
        tail = text[approval_offset:]
        if denial_pending and "reason (optional, Enter to skip):" in tail:
            child.send(b"\r")
            denial_pending = False
            approval_offset = len(text)
            return
        match = re.search(r"(?s)(?:╭─|\+-) (.*?)(?:╰─|\+-) ([^\n]*[›>])(?: |\n)", tail)
        if not match:
            return
        card, question = match.groups()
        command_lines = []
        for line in card.splitlines()[1:]:
            line = re.sub(r"^[┃|] ", "", line)
            if line.startswith(("│ $ ", "| $ ")):
                command_lines.append(line[4:])
            elif line.startswith(("│   ", "|   ")):
                command_lines.append(line[4:])
        command = "\n".join(command_lines)
        strong = "type yes" in question
        allowed = approve(command, card)
        answer = ("yes\r" if strong else "y") if allowed else ("no\r" if strong else "n")
        result.approvals.append({"command": command, "card": card, "strong": strong,
                                 "answer": answer.strip(), "allowed": allowed})
        child.send(answer.encode())
        denial_pending = not allowed
        approval_offset += match.end()

    try:
        child.until(prompt, deadline, "initial prompt")
        contracts = input_contracts(scenario)
        for i, line in enumerate(scenario["inputs"]):
            contract = contracts[i]
            start = len(result.transcript)
            child.send(b"\x15" + line.encode() + b"\r")
            correction = (scenario.get("corrections") or [])[i] if scenario.get("corrections") else None

            def completed():
                text = plain(result.transcript[start:])
                if correction:
                    return "press Enter to run" in text and child.screen.line().startswith(PROMPT + correction)
                returned = idle_prompt() and "\n" + PROMPT.rstrip() in text
                agent_done = bool(SUMMARY.search(text)) and bool(STATS.search(text)) and idle_prompt()
                return returned or agent_done or ("press Enter to run" in text and prompt())

            child.until(completed, deadline, f"completion of input {i + 1}", approvals)
            output = plain(result.transcript[start:])
            hint = SHELL_EXIT.search(output)
            result.turns.append({"input": line, "output": output, "kind": contract["kind"],
                                 "exit_code": int(hint[1]) if hint else None,
                                 "edit_line": child.screen.line() if correction else None})
            if contract["kind"] == "shell":
                if not hint or int(hint[1]) != contract["exit_code"] or SUMMARY.search(output):
                    result.failure = f"input {i + 1}: expected shell exit {contract['exit_code']}, did not observe it"
                elif any(fragment not in output for fragment in contract["contains"]):
                    result.failure = f"input {i + 1}: expected failure diagnostics were not observed"
            elif contract["kind"] == "agent" and not (SUMMARY.search(output) and STATS.search(output)):
                result.failure = f"input {i + 1}: returned without an agent task (routing/completion failure)"
            if result.failure:
                break
        if scenario["check"] == "cwd" and not result.failure:
            start = len(result.transcript)
            child.send(b"\x15printf '\\n__NOSH_EVAL_PWD_BEGIN__\\n'; pwd -P; printf '__NOSH_EVAL_PWD_END__\\n'\r")
            pattern = r"(?m)^__NOSH_EVAL_PWD_BEGIN__\n([^\n]+)\n__NOSH_EVAL_PWD_END__"
            child.until(lambda: bool(re.search(pattern, plain(result.transcript[start:]))) and prompt(),
                        deadline, "physical working directory")
            result.pwd = re.search(pattern, plain(result.transcript[start:])).group(1)
        child.send(b"\x15exit 0\r")
        while child.result.exit_code is None or child.selector.get_map():
            if time.monotonic() >= deadline:
                raise TimeoutError("REPL did not exit")
            child.pump()
    except (TimeoutError, DriverError, OSError) as exc:
        result.error = str(exc)
    finally:
        child.close()
    return result
