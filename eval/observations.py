"""Decode native engine evidence and current CLI completion records."""

from __future__ import annotations

import json
from collections import defaultdict
from pathlib import Path
import re

from . import driver
from .contracts import finite_number

TOKEN_METRICS = ("prompt_tokens", "cached_tokens", "completion_tokens")


def session_key(event: dict) -> tuple[int, int]:
    key = (event.get("engine"), event.get("sid"))
    if any(type(value) is not int or not 0 <= value < 2**64 for value in key):
        raise ValueError("invalid engine/session identity in trace")
    return key


def index_trace(events: list[dict]) -> tuple[dict, set]:
    records = defaultdict(list)
    engines, opened, active, pending = set(), set(), set(), set()
    session_events = {"open", "step_start", "step_end", "close", "rewind", "compact", "tool_choice", "observation"}
    for event in events:
        kind = event["ev"]
        if kind == "engine":
            engine, info = event.get("engine"), event.get("info")
            if (type(engine) is not int or not 0 <= engine < 2**64 or engine in engines
                    or not isinstance(info, dict) or not finite_number(info.get("load_s"))
                    or info["load_s"] < 0):
                raise ValueError("invalid or duplicate engine load observation")
            engines.add(engine)
        else:
            if kind not in session_events:
                raise ValueError(f"unknown engine trace event: {kind}")
            key = session_key(event)
            if key[0] not in engines:
                raise ValueError("session has no engine load observation")
            if kind == "open":
                if key in opened:
                    raise ValueError("duplicate engine session")
                opened.add(key)
                active.add(key)
            elif key not in active:
                raise ValueError("engine event has no open session")
            elif kind == "step_start":
                if key in pending:
                    raise ValueError("overlapping observed engine steps")
                pending.add(key)
            elif kind == "step_end":
                if key not in pending:
                    raise ValueError("engine result has no matching started step")
                pending.remove(key)
            elif kind == "close":
                if key in pending:
                    raise ValueError("closed engine is missing its step result")
                active.remove(key)
        records[kind].append(event)
    return records, pending


def assistance_observations(events: list[dict]) -> list[dict]:
    labels, executions, responses = {}, {}, {}
    completed = set()
    results = []
    for event in events:
        if event["ev"] not in ("open", "step_start", "step_end", "observation"):
            continue
        key = session_key(event)
        if event.get("ev") == "open":
            labels[key] = event.get("label", "")
        elif event.get("ev") == "step_start":
            for message in event.get("messages", []):
                if message.get("role") == "system":
                    records = re.findall(r"(?m)^\[execution\]\n([^\n]+)", message.get("text", ""))
                    if records:
                        executions[key] = json.loads(records[-1])
        elif event.get("ev") == "step_end":
            responses[key] = event
        if event.get("ev") != "observation":
            continue
        value = event.get("value")
        if not isinstance(value, dict) or value.get("workflow") != "command_assist":
            raise ValueError("unknown host observation")
        expected = f"command_assist.{value.get('intent')}.{'background' if value.get('background') else 'foreground'}"
        if (labels.get(key) != expected or key in completed
                or value.get("intent") not in ("generate", "fix", "next")
                or type(value.get("background")) is not bool
                or value.get("status") not in ("completed", "cancelled", "failed")):
            raise ValueError("invalid assistance provenance")
        command_id = value.get("command_id")
        execution = None
        if value["intent"] == "generate":
            if command_id is not None:
                raise ValueError("generate result cannot refer to an execution")
        else:
            execution = executions.get(key, {})
            if (not isinstance(execution, dict)
                    or type(command_id) is not int or command_id <= 0
                    or execution.get("command_id") != command_id
                    or type(execution.get("exit")) is not int
                    or (execution["exit"] == 0) != (value["intent"] == "next")):
                raise ValueError("assistance result does not match its execution")
        if value["status"] == "completed":
            kind, text = value.get("kind"), value.get("text")
            if (kind not in ("command", "clarify", "none")
                    or (kind == "none" and text is not None)
                    or (kind != "none" and (not isinstance(text, str) or not text.strip()))):
                raise ValueError("invalid accepted assistance result")
            response = responses.get(key, {})
            calls = response.get("tool_calls", [])
            if (response.get("stop") != "end_of_turn" or response.get("errors") != []
                    or response.get("text", "").strip() or len(calls) != 1
                    or calls[0].get("name") != "finish"):
                raise ValueError("accepted assistance has no matching finish")
            args = calls[0].get("args", {})
            if (args.get("kind") != kind or set(args) != ({"kind"} if kind == "none" else {"kind", "text"})
                    or (kind != "none" and (not isinstance(args.get("text"), str) or args["text"].strip() != text))):
                raise ValueError("accepted assistance differs from the generated finish")
        completed.add(key)
        results.append(dict(value, execution=execution))
    return results


def execution_evidence(events: list[dict]) -> list[dict]:
    executions = []
    pending: dict[tuple, list[dict]] = {}
    for event in events:
        if event["ev"] not in ("step_start", "step_end"):
            continue
        key = session_key(event)
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
                if call["name"] == "finish":
                    continue
                execution = {"call": call, "result": None, "state": "unobserved", "exit_code": None}
                executions.append(execution)
                waiting.append(execution)
            pending[key] = waiting
    return executions


def observe(result: driver.Result, scenario: dict, trace: Path, *, seed: int,
            deadline_timeout: bool = False) -> dict:
    if type(seed) is not int or not 0 <= seed < 2**64:
        raise ValueError("observation requires a u64 seed")
    if deadline_timeout and (result.timeout_phase not in ("agent", "cli", "assist")
                            or result.exit_code not in (-9, -15)):
        raise ValueError("task timeout recovery requires native in-flight agent observations")
    if re.search(r"(?m)^nosh: [^\n]*config\.toml:", driver.plain(result.stderr + result.transcript)):
        raise ValueError("nosh rejected part of the isolated configuration; see stderr/transcript")
    metrics = {
        "steps": None, "confirmations": len(result.approvals), "ttft_s": None,
        "total_s": result.total_s, "peak_rss_mib": result.peak_rss_mib,
        "task_s": None, "load_s": None, "task_status": None,
    }
    answer = ""
    cli_text = []
    done = None
    notes = []
    text = driver.plain(result.transcript)
    summaries = list(driver.SUMMARY.finditer(text))
    if summaries:
        metrics.update(task_s=sum(float(s[3]) for s in summaries),
                       task_status="completed" if all(s[1] in ("✔", "+") for s in summaries) else "incomplete")
    if scenario["mode"] == "agent" and result.stdout:
        for line in result.stdout.splitlines():
            event = json.loads(line)
            if not isinstance(event, dict) or not isinstance(event.get("ev"), str):
                raise ValueError("invalid CLI event")
            if event["ev"] == "text":
                if not isinstance(event.get("text"), str):
                    raise ValueError("invalid CLI text")
                cli_text.append(event["text"])
            elif event["ev"] in ("tool_call", "error"):
                cli_text.clear()
            elif event["ev"] == "done":
                if (done is not None or event.get("status") not in ("completed", "incomplete", "cancelled", "failed")
                        or not finite_number(event.get("secs")) or event["secs"] < 0):
                    raise ValueError("invalid or duplicate CLI completion")
                done = event
        if done:
            metrics.update(task_s=done["secs"], task_status=done["status"])
    if scenario["check"] == "typos":
        metrics.update(steps=0, task_status="local")
        notes.append("TTFT is not applicable: local spelling correction.")
    inputs = tools = sampling = None
    generated = []
    executions = None
    deadline_state = None
    assistance = []
    if scenario["check"] != "typos":
        if not trace.is_file():
            raise ValueError("native engine trace is missing")
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
        records, pending = index_trace(events)
        starts, ends, opens = records["step_start"], records["step_end"], records["open"]
        if not starts or not opens or (not deadline_timeout and pending):
            raise ValueError("incomplete engine observations; no successful fallback")
        if deadline_timeout:
            completed_tasks = sum(turn.get("kind") == "agent" for turn in result.turns)
            if len(starts) == len(ends) + 1 and len(pending) == 1:
                deadline_state = "during_generation"
            elif len(starts) == len(ends) and not pending:
                last = ends[-1]
                accepted_finish = any(
                    e.get("ev") == "observation" and e.get("sid") == last.get("sid")
                    and e.get("engine") == last.get("engine")
                    and e.get("value", {}).get("status") == "completed" for e in events
                )
                if (last.get("stop") != "end_of_turn" or (last.get("tool_calls") != [] and not accepted_finish)
                        or last.get("errors") != []):
                    raise ValueError("timeout was not after a completed final generation")
                deadline_state = "after_generation"
            else:
                raise ValueError("timeout was not an unfinished model generation")
            if len(summaries) != completed_tasks:
                raise ValueError("timeout completion markers do not match completed tasks")
        if any(not isinstance(e.get("sampling"), dict) or type(e["sampling"].get("seed")) is not int
               or e["sampling"]["seed"] != seed for e in opens):
            raise ValueError("observed sampling seed differs from the requested seed")
        for event in ends:
            usage = event.get("usage")
            if (not isinstance(event.get("text"), str) or not isinstance(usage, dict)
                    or not finite_number(usage.get("ttft_s")) or usage["ttft_s"] < 0):
                raise ValueError("invalid generated text or step usage in trace")
        executions = execution_evidence(events)
        assistance = assistance_observations(events)
        expected_assistance = scenario.get("assistance")
        assist_session = any(isinstance(e.get("label"), str) and e["label"].startswith("command_assist.")
                             for e in opens)
        if (scenario["mode"] == "suggest" or expected_assistance or assist_session) and not assistance and not deadline_timeout:
            raise ValueError("command assistance has no host result observation")
        if assistance and not expected_assistance and scenario["mode"] != "suggest":
            raise ValueError("command assistance ran during isolated Agent evaluation")
        metrics["steps"] = len(starts)
        first_end = next((event for event in ends if session_key(event) == session_key(starts[0])), None)
        metrics["ttft_s"] = first_end["usage"]["ttft_s"] if first_end is not None else None
        metrics["load_s"] = sum(e["info"]["load_s"] for e in records["engine"])
        inputs = [e for e in events if e["ev"] in ("open", "step_start", "rewind", "compact", "tool_choice")]
        # Engine IDs are process-local bookkeeping, not model input.
        inputs = [{k: v for k, v in e.items() if k not in ("engine", "schema_version")} for e in inputs]
        tools = [call for e in ends for call in e["tool_calls"]]
        sampling = [e["sampling"] for e in opens]
        generated = [e["text"] for e in ends]
        for metric in TOKEN_METRICS:
            values = [e["usage"].get(metric) for e in ends]
            if any(value is not None and (type(value) is not int or value < 0) for value in values):
                raise ValueError("invalid token usage")
            if values and all(type(value) is int and value >= 0 for value in values):
                metrics[metric] = sum(values)
            else:
                metrics[metric] = None
        if deadline_state == "during_generation":
            metrics["task_status"] = "timed_out"
            answer = ""
            notes.append("The declared trial deadline expired during model generation. "
                         "Steps include the observed in-flight call; there is no final answer. "
                         "TTFT is available only if the first step completed.")
        elif deadline_state == "after_generation":
            metrics["task_status"] = "timed_out"
            answer = (assistance[-1].get("text") or "") if assistance else ends[-1]["text"].strip()
            notes.append("The declared trial deadline expired after the final generation completed "
                         "but before the REPL completion marker. The observed answer is preserved as "
                         "evidence, but the trial remains a timeout failure.")
        elif scenario["mode"] != "suggest":
            answer = ends[-1]["text"].strip()
        if assistance and not deadline_timeout:
            last = assistance[-1]
            metrics["task_status"] = last["status"]
            answer = last.get("text") or ""
            if scenario["mode"] == "suggest" and last["status"] == "completed":
                expected_stdout = answer if last["kind"] == "command" else ""
                if result.stdout.strip() != expected_stdout:
                    raise ValueError("CLI stdout differs from the accepted assistance result")
    elif trace.exists():
        raise ValueError("local correction unexpectedly loaded the inference engine")
    if scenario["mode"] == "agent" and not deadline_timeout:
        if done is None:
            raise ValueError("CLI agent completion is missing")
        if metrics["task_status"] == "completed" and "".join(cli_text).strip() != answer:
            raise ValueError("CLI agent answer differs from the native final response")
    return {"metrics": metrics, "answer": answer, "inputs": inputs, "tool_calls": tools,
            "executions": executions, "sampling": sampling, "generated_answers": generated,
            "assistance": assistance, "deadline_state": deadline_state, "metric_notes": notes}
