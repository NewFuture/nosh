"""Usage (Linux/WSL, Python >= 3.11): python3 -m eval.run --model-path MODEL_DIR."""

from __future__ import annotations

import argparse
import contextlib
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile

from . import checks, driver, fixtures, report

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent
LEGACY_CHECKS = {"largest", "port", "lines", "rename", "python", "typos", "failure", "history", "archive", "cwd"}
CHECK_FIXTURES = {
    "largest": "big", "port": "port", "lines": "project", "rename": "rename",
    "python": "project", "typos": "typo", "failure": "failure", "history": "history",
    "archive": "logs", "cwd": "big",
    "rust-build": "rust", "rust-test": "rust", "rust-clean": "rust-built",
    "node-build": "node", "node-test": "node", "python-test": "python",
    "git-diff": "dirty-git", "git-commit": "dirty-git", "recent-history": "history",
    "versions": "python", "clarification": "python",
    "build-failure": "rust-broken", "test-failure": "python-broken", "port-failure": "port",
}
CHECKS = set(CHECK_FIXTURES)
NATIVE_CHECKS = CHECKS - LEGACY_CHECKS
APPROVAL_CHECKS = {
    "rename": {"rename"}, "cwd": {"cwd"},
    "rust-build": {"rust-build", "build-failure"}, "rust-test": {"rust-test"},
    "rust-clean": {"rust-clean"}, "node-build": {"node-build"}, "node-test": {"node-test"},
    "python-test": {"python-test", "test-failure"}, "git-commit": {"git-commit"},
}


def seeds(value: list) -> list[int]:
    if not isinstance(value, list) or not value or any(type(n) is not int or not 0 <= n < 2**64 for n in value):
        raise ValueError("seeds must be a nonempty list of u64 integers")
    if len(value) != len(set(value)):
        raise ValueError("duplicate seeds")
    return value


def load_suite(path: Path) -> dict:
    suite = json.loads(path.read_text(encoding="utf-8"))
    if not isinstance(suite, dict) or type(suite.get("schema_version")) is not int or suite["schema_version"] not in (1, 2):
        raise ValueError("unsupported scenario schema")
    if set(suite) - {"schema_version", "seeds", "timeout_s", "scenarios"}:
        raise ValueError("unknown suite fields")
    seeds(suite.get("seeds"))
    timeout = suite.get("timeout_s")
    if type(timeout) not in (int, float) or not math.isfinite(timeout) or timeout <= 0:
        raise ValueError("timeout_s must be finite and positive")
    scenarios = suite.get("scenarios")
    if not isinstance(scenarios, list) or not scenarios:
        raise ValueError("scenarios must be a nonempty list")
    ids = set()
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            raise ValueError("scenario must be an object")
        fields = {"id", "title", "mode", "fixture", "inputs", "input",
                  "corrections", "stdin_command", "approval", "check"}
        if suite["schema_version"] == 2:
            fields |= {"group", "expect", "completions"}
        if set(scenario) - fields:
            raise ValueError("unknown scenario fields")
        sid = scenario.get("id")
        if not isinstance(sid, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", sid) or sid in ids:
            raise ValueError(f"invalid/duplicate scenario id: {sid}")
        ids.add(sid)
        if (not isinstance(scenario.get("fixture"), str) or not isinstance(scenario.get("check"), str)
                or scenario["fixture"] not in fixtures.FIXTURES or scenario["check"] not in CHECKS):
            raise ValueError(f"unknown fixture or check: {sid}")
        if suite["schema_version"] == 1 and scenario["check"] not in LEGACY_CHECKS:
            raise ValueError(f"new checks require scenario schema v2: {sid}")
        if scenario["fixture"] != CHECK_FIXTURES[scenario["check"]]:
            raise ValueError(f"fixture does not supply the check's required facts: {sid}")
        if not isinstance(scenario.get("title"), str) or not scenario["title"].strip():
            raise ValueError(f"missing scenario title: {sid}")
        policy = scenario.get("approval")
        if not isinstance(policy, str) or (policy != "deny" and scenario["check"] not in APPROVAL_CHECKS.get(policy, set())):
            raise ValueError(f"unknown approval policy: {sid}")
        if scenario.get("mode") == "repl":
            inputs = scenario.get("inputs")
            if not isinstance(inputs, list) or not inputs:
                raise ValueError(f"missing REPL inputs: {sid}")
            corrections = scenario.get("corrections")
            if corrections is not None and (
                not isinstance(corrections, list) or len(corrections) != len(inputs)
                or not all(isinstance(c, str) and c for c in corrections)
            ):
                raise ValueError(f"invalid corrections: {sid}")
            if (scenario["check"] == "typos") != (corrections is not None):
                raise ValueError(f"only local correction cases must specify corrections: {sid}")
            if suite["schema_version"] == 2:
                completions = scenario.get("completions")
                if not isinstance(completions, list) or len(completions) != len(inputs):
                    raise ValueError(f"each REPL input requires a completion contract: {sid}")
                for completion in completions:
                    if not isinstance(completion, dict):
                        raise ValueError(f"invalid completion contract: {sid}")
                    kind = completion.get("kind")
                    if kind not in ("agent", "shell", "correction"):
                        raise ValueError(f"unknown input completion: {sid}")
                    if kind == "shell":
                        code = completion.get("exit_code")
                        contains = completion.get("contains")
                        if (set(completion) != {"kind", "exit_code", "contains"}
                                or type(code) is not int or not 1 <= code <= 255
                                or not isinstance(contains, list) or not contains
                                or not all(isinstance(s, str) and s and "\n" not in s for s in contains)):
                            raise ValueError(f"invalid failed-command completion: {sid}")
                    elif set(completion) != {"kind"}:
                        raise ValueError(f"unknown completion fields: {sid}")
                    if (kind == "correction") != (corrections is not None):
                        raise ValueError(f"correction completion does not match inputs: {sid}")
                if corrections is None and completions[-1]["kind"] != "agent":
                    raise ValueError(f"the final REPL input must ask the agent: {sid}")
        elif scenario.get("mode") in ("agent", "suggest"):
            if any(key in scenario for key in ("inputs", "corrections", "completions")):
                raise ValueError(f"REPL fields in a CLI scenario: {sid}")
            inputs = [scenario.get("input")]
            command = scenario.get("stdin_command")
            if command is not None and command != ["git", "log", "--stat", "-8"]:
                raise ValueError(f"unsupported stdin producer: {sid}")
            if command is not None and scenario["mode"] != "agent":
                raise ValueError(f"stdin attachments require agent mode: {sid}")
        else:
            raise ValueError(f"unknown mode: {sid}")
        if scenario["check"] in NATIVE_CHECKS | {"typos", "failure", "cwd"} and scenario["mode"] != "repl":
            raise ValueError(f"this check requires a shared interactive session: {sid}")
        if not all(isinstance(s, str) and s and "\0" not in s and "\r" not in s and "\n" not in s for s in inputs):
            raise ValueError(f"inputs must be nonempty single lines: {sid}")
        if suite["schema_version"] == 2:
            if scenario.get("group") not in ("mvp", "expanded"):
                raise ValueError(f"invalid scenario group: {sid}")
            expect = scenario.get("expect")
            if not isinstance(expect, dict) or set(expect) != {
                "max_steps", "max_confirmations", "response_language", "final_question",
            }:
                raise ValueError(f"all experience expectations must be declared: {sid}")
            if any(type(expect[k]) is not int or expect[k] < 0 for k in ("max_steps", "max_confirmations")):
                raise ValueError(f"experience limits must be nonnegative integers: {sid}")
            nonprose = scenario["check"] == "typos" or scenario["mode"] == "suggest"
            if (expect["response_language"] not in ("zh", "any", "not_applicable")
                    or (expect["response_language"] == "not_applicable") != nonprose):
                raise ValueError(f"invalid response language expectation: {sid}")
            question = "not_applicable" if nonprose else "require" if scenario["check"] == "clarification" else "forbid"
            if expect["final_question"] != question:
                raise ValueError(f"invalid final-question expectation: {sid}")
            if (expect["max_steps"] == 0) != (scenario["check"] == "typos"):
                raise ValueError(f"only local correction has a zero-step budget: {sid}")
    return suite


def required_tools(scenarios: list[dict]) -> set[str]:
    required = {"git", "bash", "python3", "tar", "ss"}
    if any(s["fixture"].startswith("rust") for s in scenarios):
        required |= {"cargo", "rustc", "cc"}
    if any(s["fixture"] == "node" for s in scenarios):
        required |= {"node", "npm"}
    if any(s["check"] == "versions" for s in scenarios):
        required |= {"cargo", "node"}
    return required


def discover_tools(scenarios: list[dict]) -> dict:
    tools = {}
    inherited_path = os.environ.get("PATH", "")
    env = dict(os.environ, LANG="C.UTF-8", LC_ALL="C.UTF-8", RUSTUP_AUTO_INSTALL="0")
    for name in sorted(required_tools(scenarios)):
        search = ("/usr/bin:/bin:" + inherited_path if name in {"git", "bash", "python3", "tar", "ss"}
                  else inherited_path + ":/usr/bin:/bin")
        executable = shutil.which(name, path=search)
        if not executable:
            raise ValueError(f"missing required executable: {name}; prepare the toolchain before evaluation")
        path = Path(executable).resolve()
        if name in ("cargo", "rustc") and path.name == "rustup":
            resolved = subprocess.check_output([str(path), "which", name], env=env, text=True, timeout=10).strip()
            path = Path(resolved).resolve(strict=True)
        proc = subprocess.run([str(path), "-V" if name == "ss" else "--version"],
                              env=env, check=True, capture_output=True, text=True, timeout=10)
        version = (proc.stdout or proc.stderr).strip()
        if not version:
            raise ValueError(f"{name} returned no version")
        if name == "node":
            major = re.match(r"v(\d+)\.", version)
            if not major or int(major[1]) < 22:
                raise ValueError("Node >= 22 is required for the dependency-free node:test fixture")
        tools[name] = {"path": str(path), "version": version.splitlines()[0], "sha256": fixtures.file_hash(path)}
    return tools


def environment(home: Path, threads: int, trace: Path | None, tools: dict | None = None) -> dict[str, str]:
    env = fixtures.project_environment(home, tools)
    env.update({
        "NOSH_HOME": str(home / "nosh"),
        "USER": "eval", "LOGNAME": "eval", "LANG": "C.UTF-8", "LC_ALL": "C.UTF-8", "TZ": "UTC",
        "TERM": "xterm-256color", "NO_COLOR": "1", "NOSH_STATS": "1",
        "NOSH_OFFLINE": "1", "HF_HUB_OFFLINE": "1", "PYTHONDONTWRITEBYTECODE": "1",
        "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": os.devnull,
        "CANDLE_NUM_THREADS": str(threads), "RAYON_NUM_THREADS": "1",
    })
    config = home / "nosh"
    config.mkdir(mode=0o700)
    (config / "config.toml").write_text(
        '[agent]\napproval = "confirm"\nmax_steps = 10\ncommand_timeout_sec = 60\nrestore_cwd = false\n'
        '[model]\ncontext_length = 8192\nthinking = "off"\n[download]\nauto = "never"\n',
        encoding="utf-8",
    )
    if trace:
        env["NOSH_EVAL_TRACE"] = str(trace)
    return env


def legacy_answer(text: str) -> str:
    lines = []
    for line in driver.plain(text).splitlines():
        if not line.startswith(("┃ ", "| ")):
            continue
        content = line[2:]
        if content.startswith(("⚙ ", "╭─", "+-")) or re.match(
            r"\* [a-z_]+  (?:SAFE|MUTATING|DANGEROUS|FORBIDDEN)(?: |$)", content
        ):
            lines = []
        elif not driver.SUMMARY.match(line) and not content.startswith((
            "  ", "stats:", "✔ ", "⚠ ", "✗ ", "cwd →", "cwd ->",
            "↳ ", "-> ", "…", "...", "│ ", "| ", "╰─", "reason (optional",
        )):
            lines.append(content)
    return "\n".join(lines).strip()


def execution_evidence(events: list[dict]) -> list[dict]:
    executions = []
    pending: dict[tuple, list[dict]] = {}
    for event in events:
        key = (event.get("engine"), event.get("sid"))
        if any(value is not None and type(value) is not int for value in key):
            raise ValueError("invalid engine/session identity in trace")
        if event["ev"] == "step_start":
            if not isinstance(event.get("messages"), list):
                raise ValueError("trace step is missing its messages")
            waiting = pending.get(key, [])
            for message in event["messages"]:
                if (not isinstance(message, dict) or not isinstance(message.get("role"), str)
                        or not isinstance(message.get("text"), str)):
                    raise ValueError("invalid observed engine message")
                if message["role"] != "tool" or not waiting:
                    continue
                execution = waiting.pop(0)
                text = message["text"]
                header = re.match(r"^\[exit_code=(-?\d+) duration=[\d.]+s truncated=(yes|no)([^\]]*)\]\n", text)
                execution.update(result=text, state="returned")
                if execution["call"]["name"] == "run_command":
                    execution["state"] = "executed" if header else "not_executed"
                    if header:
                        execution.update(exit_code=int(header[1]), truncated=header[2] == "yes",
                                         timed_out="timed_out=yes" in header[3],
                                         interrupted="interrupted=yes" in header[3])
        elif event["ev"] == "step_end":
            if not isinstance(event.get("tool_calls"), list):
                raise ValueError("trace step is missing its tool calls")
            waiting = []
            for call in event["tool_calls"]:
                if not isinstance(call, dict) or not isinstance(call.get("name"), str) or not isinstance(call.get("args"), dict):
                    raise ValueError("invalid observed tool call")
                execution = {"call": call, "result": None, "state": "unobserved", "exit_code": None}
                executions.append(execution)
                waiting.append(execution)
            pending[key] = waiting
    return executions


def observe(result: driver.Result, scenario: dict, trace: Path, legacy: bool, seed: int,
            inflight_timeout: bool = False) -> dict:
    if inflight_timeout and (legacy or result.timeout_phase not in ("agent", "cli")
                            or result.exit_code not in (-9, -15)):
        raise ValueError("task timeout recovery requires native in-flight agent observations")
    if re.search(r"(?m)^nosh: [^\n]*config\.toml:", driver.plain(result.stderr + result.transcript)):
        raise ValueError("nosh rejected part of the isolated configuration; see stderr/transcript")
    metrics = {
        "steps": None, "confirmations": len(result.approvals), "ttft_s": None,
        "total_s": result.total_s, "peak_rss_mib": result.peak_rss_mib,
        "task_s": None, "load_s": None, "task_status": None,
    }
    answer = result.stdout.strip() if scenario["mode"] == "suggest" else legacy_answer(result.transcript)
    notes = []
    text = driver.plain(result.transcript)
    summaries = list(driver.SUMMARY.finditer(text))
    if summaries:
        metrics.update(steps=sum(int(s[2]) for s in summaries), task_s=sum(float(s[3]) for s in summaries),
                       task_status="completed" if all(s[1] in ("✔", "+") for s in summaries) else "incomplete")
        ttft = re.search(r"(?m)^[┃|] stats: .*?ttft ([\d.]+)s", text)
        if ttft:
            metrics["ttft_s"] = float(ttft[1])
    if scenario["mode"] == "agent" and result.stdout:
        chunks = []
        done = None
        for line in result.stdout.splitlines():
            event = json.loads(line)
            if event.get("ev") == "tool_call":
                chunks = []
            elif event.get("ev") == "text":
                chunks.append(event["text"])
            elif event.get("ev") == "done":
                done = event
        answer = "".join(chunks).strip()
        if done:
            metrics.update(steps=done["steps"], ttft_s=done["usage"]["ttft_s"],
                           task_s=done["secs"], task_status=done["status"])
    if scenario["mode"] == "suggest":
        metrics.update(steps=1 if result.exit_code == 0 else None,
                       task_status="completed" if result.exit_code == 0 else "incomplete")
    if scenario["check"] == "typos":
        metrics.update(steps=0, task_status="local")
        notes.append("TTFT is not applicable: local spelling correction.")
    inputs = tools = sampling = None
    generated = []
    executions = None
    if not legacy and scenario["check"] != "typos":
        if not trace.is_file():
            raise ValueError("native engine trace is missing; use --legacy explicitly for an older binary")
        events = [json.loads(line) for line in trace.read_text(encoding="utf-8").splitlines()]
        if not events or any(not isinstance(e, dict) or type(e.get("schema_version")) is not int
                             or e["schema_version"] != 1 or not isinstance(e.get("ev"), str) for e in events):
            raise ValueError("invalid engine trace schema")
        if "evaluation trace:" in result.stderr or "evaluation trace:" in result.transcript:
            raise ValueError("nosh reported an evaluation trace failure")
        errors = [e.get("error") for e in events if e["ev"] == "step_error"]
        if any(not isinstance(error, str) for error in errors):
            raise ValueError("invalid engine error observation")
        if errors:
            raise RuntimeError("model engine failed: " + "; ".join(errors))
        starts = [e for e in events if e["ev"] == "step_start"]
        ends = [e for e in events if e["ev"] == "step_end"]
        opens = [e for e in events if e["ev"] == "open"]
        if not starts or len(starts) != len(ends) + int(inflight_timeout) or not opens:
            raise ValueError("incomplete engine observations; no successful fallback")
        if inflight_timeout:
            pending = set()
            for event in events:
                key = (event.get("engine"), event.get("sid"))
                if any(value is not None and type(value) is not int for value in key):
                    raise ValueError("invalid engine/session identity in trace")
                if event["ev"] == "step_start":
                    if key in pending:
                        raise ValueError("overlapping observed engine steps")
                    pending.add(key)
                elif event["ev"] == "step_end":
                    if key not in pending:
                        raise ValueError("engine result has no matching started step")
                    pending.remove(key)
                elif event["ev"] == "close" and key in pending:
                    raise ValueError("closed engine is missing its step result")
            completed_tasks = sum(turn.get("kind") == "agent" for turn in result.turns)
            if len(pending) != 1 or len(summaries) > completed_tasks:
                raise ValueError("timeout was not an unfinished model generation")
        if any(not isinstance(e.get("sampling"), dict) or type(e["sampling"].get("seed")) is not int
               or e["sampling"]["seed"] != seed for e in opens):
            raise ValueError("observed sampling seed differs from the requested seed")
        for event in ends:
            usage = event.get("usage")
            if (not isinstance(event.get("text"), str) or not isinstance(usage, dict)
                    or type(usage.get("ttft_s")) not in (int, float)
                    or not math.isfinite(usage["ttft_s"]) or usage["ttft_s"] < 0):
                raise ValueError("invalid generated text or step usage in trace")
        for event in events:
            if event["ev"] == "engine":
                info = event.get("info")
                if (not isinstance(info, dict) or type(info.get("load_s")) not in (int, float)
                        or not math.isfinite(info["load_s"]) or info["load_s"] < 0):
                    raise ValueError("invalid engine load observation")
        executions = execution_evidence(events)
        metrics["steps"] = len(starts)
        metrics["ttft_s"] = ends[0]["usage"]["ttft_s"] if ends else None
        metrics["load_s"] = sum(e["info"]["load_s"] for e in events if e["ev"] == "engine")
        inputs = [e for e in events if e["ev"] in ("open", "step_start", "rewind", "compact")]
        # Engine IDs are process-local bookkeeping, not model input.
        inputs = [{k: v for k, v in e.items() if k not in ("engine", "schema_version")} for e in inputs]
        tools = [call for e in ends for call in e["tool_calls"]]
        sampling = [e["sampling"] for e in opens]
        generated = [e["text"] for e in ends]
        if inflight_timeout:
            metrics["task_status"] = "timed_out"
            answer = ""
            notes.append("The declared trial deadline expired during model generation. "
                         "Steps include the observed in-flight call; there is no final answer. "
                         "TTFT is available only if the first step completed.")
        elif scenario["mode"] != "suggest":
            answer = ends[-1]["text"].strip()
    elif not legacy and trace.exists():
        raise ValueError("local correction unexpectedly loaded the inference engine")
    if legacy:
        notes.append("Legacy: engine inputs and exact tool traces are unavailable.")
        if scenario["mode"] == "suggest":
            notes.append("True -s TTFT is unobservable; a successful suggestion has one step by the CLI contract.")
        elif scenario["mode"] == "repl" and scenario["check"] != "typos":
            notes.append("Legacy terminal TTFT is rounded to 0.01 s; task time to 0.1 s.")
    if scenario["check"] != "typos" and metrics["steps"] is None and not result.error:
        raise ValueError("task completion/step measurement is missing")
    return {"metrics": metrics, "answer": answer, "inputs": inputs, "tool_calls": tools,
            "executions": executions, "sampling": sampling, "generated_answers": generated, "metric_notes": notes}


def model_files(path: Path) -> tuple[Path, Path]:
    path = path.expanduser().resolve(strict=True)
    if path.is_dir():
        weights = sorted(path.glob("*.gguf"))
        if len(weights) != 1:
            raise ValueError("model directory must contain exactly one GGUF; otherwise pass the desired file")
        path = weights[0]
    tokenizer = path.parent / "tokenizer.json"
    if path.suffix != ".gguf" or not path.is_file() or not tokenizer.is_file():
        raise ValueError("a GGUF and its adjacent tokenizer.json are required")
    return path, tokenizer


def machine_info() -> dict:
    model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                  if line.startswith("model name")), platform.processor())
    os_release = Path("/etc/os-release").read_text()
    return {"os_release": os_release, "kernel": platform.release(), "arch": platform.machine(),
            "cpu": model, "logical_cpus": os.cpu_count()}


def metadata(args, suite: dict, binary: Path, weights: Path, tokenizer: Path, toolchain: dict) -> dict:
    binary_hash = fixtures.file_hash(binary)
    build = {"source_revision": None, "source_clean": None, "binary_sha256": binary_hash,
             "provenance": "unverified external binary"}
    if args.build_info:
        build = json.loads(args.build_info.read_text(encoding="utf-8"))
        if (build.get("schema_version") != 1 or build.get("binary_sha256") != binary_hash
                or not re.fullmatch(r"[0-9a-f]{40}", build.get("source_revision", ""))):
            raise ValueError("build info does not identify this exact binary and source revision")
        build["provenance"] = "recorded build; supplied binary hash verified"
    tools = {"python": platform.python_version(), **{name: info["version"] for name, info in toolchain.items()}}
    return {
        "run_id": args.label or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ"),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "observation": "legacy" if args.legacy else "native-v1",
        "build": build,
        "model": {"weights": weights.name, "weights_sha256": fixtures.file_hash(weights),
                  "tokenizer_sha256": fixtures.file_hash(tokenizer)},
        "suite_sha256": fixtures.digest(suite),
        "suite_schema_version": suite["schema_version"],
        "harness_sha256": fixtures.digest({p.name: fixtures.file_hash(p) for p in sorted(HERE.glob("*.py"))}),
        "harness_content_sha256": fixtures.digest({p.name: fixtures.source_hash(p) for p in sorted(HERE.glob("*.py"))}),
        "grading_content_sha256": fixtures.source_hash(HERE / "checks.py"),
        "settings": {"threads": args.threads, "rayon_threads": 1, "context_length": 8192,
                     "max_steps": 10, "command_timeout_s": 60, "timeout_s": args.timeout or suite["timeout_s"],
                     "approval": "confirm", "locale": "C.UTF-8", "timezone": "UTC",
                     "path": "<trial-home>/bin:/usr/bin:/bin", "tty_size": [40, 160], "process_per_trial": True,
                     "process_niceness": os.getpriority(os.PRIO_PROCESS, 0),
                     "cargo_offline": True, "cargo_incremental": False, "cargo_jobs": 1, "npm_offline": True},
        "machine": machine_info(), "tools": tools, "toolchain": toolchain,
        "scenarios": suite["scenarios"], "seeds": args.seeds or suite["seeds"], "repeat": args.repeat,
    }


def run_trial(args, meta: dict, scenario: dict, seed: int, repeat: int,
              workspace: fixtures.Workspace, output: Path, binary: Path, weights: Path) -> dict:
    row = {
        "scenario_id": scenario["id"], "seed": seed, "repeat": repeat, "status": "error",
        "metrics": {key: None for key in report.METRICS},
        "answer": "", "reasons": [], "inputs": None, "tool_calls": None, "final_state": None,
        "grading": None, "executions": None,
    }
    result = None
    trace = None
    logs = output / "logs" / f"{scenario['id']}-{seed}-{repeat}"
    logs.mkdir(parents=True)
    try:
        root, home, facts = workspace.prepare(scenario)
        trace = home.parent / "engine.jsonl"
        env = environment(home, args.threads, None if args.legacy else trace, workspace.tools)
        if scenario["check"] in NATIVE_CHECKS:
            facts["tools"] = workspace.tools
        if scenario["check"] == "versions":
            facts["versions"] = {name: meta["tools"][name] for name in ("cargo", "node", "python3")}
        if scenario["check"] == "recent-history":
            facts["git_before"] = fixtures.git_state(root)
            facts["commit_ids"] = fixtures.git(root, "log", "--format=%H").splitlines()
        argv = [str(binary), "--offline", "--no-download", "--norc", "--seed", str(seed), "--model-path", str(weights)]
        timeout = meta["settings"]["timeout_s"]
        fixture = fixtures.listener(root) if scenario["fixture"] == "port" else contextlib.nullcontext(None)
        with fixture as port:
            if port:
                facts["listener"] = port
            if scenario["mode"] == "repl":
                result = driver.run_repl(
                    argv, root, env, timeout, scenario,
                    lambda command, card: checks.allow_approval(scenario["approval"], command, root, facts),
                )
            else:
                data = b""
                if scenario.get("stdin_command"):
                    data = subprocess.check_output(scenario["stdin_command"], cwd=root, env=env, timeout=5)
                flags = ["-a", "--json"] if scenario["mode"] == "agent" else ["-s"]
                result = driver.run_cli(argv + flags + [scenario["input"]], root, env, timeout, data)
        row.update(approvals=result.approvals, turns=result.turns, exit_code=result.exit_code, facts=facts)
        row["metrics"].update(total_s=result.total_s, peak_rss_mib=result.peak_rss_mib,
                              confirmations=len(result.approvals))
        after = fixtures.snapshot(root)
        row.update(fixture_sha256=fixtures.digest({k: v for k, v in facts.items() if k != "listener"}),
                   file_snapshot=after, final_state=checks.fixture_state(scenario, facts, root, after, result))
        if result.error:
            if not args.legacy and result.timeout_phase in ("agent", "cli") and trace.is_file():
                observed = observe(result, scenario, trace, False, seed, inflight_timeout=True)
                row.update(observed)
                reason = (f"agent exceeded the {timeout:g}s trial deadline during model generation "
                          f"({row['metrics']['steps']} started steps)")
                if scenario.get("expect") and row["metrics"]["steps"] > scenario["expect"]["max_steps"]:
                    reason += f"; step budget is {scenario['expect']['max_steps']}"
                row.update(status="fail", reasons=[reason], timeout_phase=result.timeout_phase,
                           grading={"facts": {"passed": False, "reasons": [reason]}, "experience": None})
                return row
            raise driver.DriverError(result.error)
        if result.failure:
            if not args.legacy and trace.is_file():
                observed = observe(result, scenario, trace, False, seed)
                row.update(observed)
                if observed["metrics"]["task_status"] is None:
                    raise driver.DriverError("an agent task ran but its completion marker was not observed")
            row.update(status="fail", reasons=[result.failure],
                       grading={"facts": {"passed": False, "reasons": [result.failure]}, "experience": None})
            return row
        observed = observe(result, scenario, trace, args.legacy, seed)
        row.update(observed)
        verdict = checks.judge(scenario, row["answer"], facts, root, after, result, row["metrics"], row)
        if not verdict.passed and any(not a["allowed"] for a in result.approvals):
            verdict.reasons.append("The declared approval policy denied at least one command; see approval evidence.")
        row.update(status="pass" if verdict.passed else "fail", reasons=verdict.reasons, grading=verdict.details)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        row["reasons"].append(f"{type(exc).__name__}: {exc}")
    finally:
        if result:
            for name in ("stdout", "stderr", "transcript"):
                (logs / f"{name}.txt").write_text(getattr(result, name), encoding="utf-8")
        if trace and trace.is_file():
            shutil.copyfile(trace, logs / "engine.jsonl")
        row["logs"] = str(logs.relative_to(output))
        workspace.clean(scenario["id"])
    return row


def arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "release" / "nosh")
    parser.add_argument("--model-path", type=Path, required=True)
    parser.add_argument("--suite", type=Path, default=HERE / "scenarios.json")
    parser.add_argument("--scenario", action="append", help="select a scenario (repeatable)")
    parser.add_argument("--seeds", nargs="+", type=int)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--timeout", type=float)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--build-info", type=Path)
    parser.add_argument("--label")
    parser.add_argument("--legacy", action="store_true", help="explicitly allow older binaries with incomplete observations")
    return parser.parse_args(argv)


def main(argv=None) -> int:
    args = arguments(argv)
    if sys.platform != "linux" or sys.version_info < (3, 11):
        print("eval: Linux/WSL and Python >= 3.11 are required", file=sys.stderr)
        return 2
    data = output = previous = None
    try:
        pidfd = os.pidfd_open(os.getpid())
        os.close(pidfd)
        suite = load_suite(args.suite)
        if args.seeds is not None:
            seeds(args.seeds)
        if args.repeat < 1 or args.threads < 1 or (args.timeout is not None and (not math.isfinite(args.timeout) or args.timeout <= 0)):
            raise ValueError("repeat, threads and timeout must be positive")
        if args.label is not None and not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.label):
            raise ValueError("label must be a plain name, not a path")
        if args.scenario:
            chosen = set(args.scenario)
            unknown = chosen - {s["id"] for s in suite["scenarios"]}
            if unknown:
                raise ValueError(f"unknown scenarios: {sorted(unknown)}")
            suite = dict(suite, scenarios=[s for s in suite["scenarios"] if s["id"] in chosen])
        if args.legacy and any(s["check"] in NATIVE_CHECKS for s in suite["scenarios"]):
            raise ValueError("selected scenarios require native execution evidence; --legacy is not supported")
        toolchain = discover_tools(suite["scenarios"])
        binary = args.binary.expanduser().resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("nosh binary is not executable; build it first with cargo build --release --locked")
        weights, tokenizer = model_files(args.model_path)
        previous = json.loads(args.compare.read_text(encoding="utf-8")) if args.compare else None
        if previous is not None:
            report.validate(previous)
        meta = metadata(args, suite, binary, weights, tokenizer, toolchain)
        output = (args.output or HERE / "results" / meta["run_id"]).absolute()
        work = (args.work_dir or Path(tempfile.gettempdir()) / f"nosh-eval-{os.getuid()}").absolute()
        for resource in (binary, weights, tokenizer, output, *(Path(t["path"]) for t in toolchain.values())):
            if resource.resolve().is_relative_to(work.resolve()):
                raise ValueError("binary, models and reports must be outside the disposable workspace")
        for parent in (work, *work.parents):
            if (parent / ".git").exists() or (parent / "NOSH.md").exists():
                raise ValueError("workspace must be outside existing repositories and NOSH.md ancestry")
        meta["settings"]["work_dir"] = str(work)
        output.mkdir(mode=0o700, parents=True, exist_ok=False)
        data = {"schema_version": report.SCHEMA_VERSION, "metadata": meta, "trials": []}
        with fixtures.Workspace(work, toolchain) as workspace:
            report.save(data, output, previous)
            for repeat in range(args.repeat):
                for scenario in suite["scenarios"]:
                    for seed in meta["seeds"]:
                        print(f"{scenario['id']} seed={seed} repeat={repeat}", flush=True)
                        row = run_trial(args, meta, scenario, seed, repeat, workspace, output, binary, weights)
                        data["trials"].append(row)
                        report.save(data, output, previous)
                        print(f"  {row['status']}: {'; '.join(row['reasons'])}", flush=True)
        print(f"Report: {output / 'report.md'}", flush=True)
        if any(t["status"] == "error" for t in data["trials"]):
            return 2
        if any(t["status"] == "fail" for t in data["trials"]) or any(
            p["consistent"] is not True for p in data["reproducibility"]
        ):
            return 1
        return 0
    except KeyboardInterrupt:
        if data is not None:
            data["error"] = "Interrupted; uncompleted trials remain in the planned denominator."
            report.save(data, output, previous)
        print("eval: interrupted", file=sys.stderr)
        return 130
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"eval: {exc}", file=sys.stderr)
        if data is not None:
            data["error"] = str(exc)
            try:
                report.save(data, output, previous)
            except OSError as write_error:
                print(f"eval: could not preserve partial report: {write_error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
