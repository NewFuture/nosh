"""Translate native traces and legacy output into measured trial evidence."""

from __future__ import annotations

import json
import math
from pathlib import Path
import re

from . import driver

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
