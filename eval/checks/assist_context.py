"""Bind model-visible Assist payloads to the declared task and terminal evidence."""

from __future__ import annotations

import json
from pathlib import Path
import re

from .. import driver


FENCE = re.compile(r"(?m)^(`{3,})(text|bash)\n([\s\S]*?)\n\1(?=\n|$)")


def packet(text: str) -> tuple[str, list[tuple[str, str, str]]]:
    matches = list(FENCE.finditer(text))
    outside = text[:matches[0].start()] if matches else text
    blocks = []
    for index, match in enumerate(matches):
        end = matches[index + 1].start() if index + 1 < len(matches) else len(text)
        tail = text[match.end():end]
        outside += tail
        blocks.append((match[2], match[3], tail))
    if re.search(r"(?m)^`{3,}", outside):
        raise ValueError("unclosed or unsupported input fence")
    return outside, blocks


def field(text: str, name: str, *, optional: bool = False):
    values = re.findall(r"(?m)^" + re.escape(name) + r": (.+)$", text)
    if not values and optional:
        return None
    if len(values) != 1:
        raise ValueError(f"missing or ambiguous {name}")
    return json.loads(values[0])


def bounded(text: str) -> tuple[str, bool]:
    encoded = text.encode("utf-8")
    # The source is valid UTF-8; only an incomplete final code point is omitted.
    return encoded[:1024].decode("utf-8", errors="ignore"), len(encoded) > 1024


def flag(text: str, name: str, expected: bool) -> None:
    value = field(text, name, optional=True)
    if expected and value is not True or not expected and value is not None and value is not False:
        raise ValueError(f"model {name} differs from the host record")


def execution_payload(block, execution: dict, cwd: str, turn: dict, *, historical: bool) -> None:
    language, command, tail = block
    expected, clipped = bounded(execution["command"]) if historical else (execution["command"], False)
    if language != "bash" or command != expected:
        raise ValueError("recorded command is missing or changed in model input")
    code = field(tail, "exit_code")
    if type(code) is not int or code != execution["exit"]:
        raise ValueError("model exit code differs from its execution")
    terminal_code = turn.get("exit_code")
    if terminal_code is not None and (type(terminal_code) is not int or terminal_code != code):
        raise ValueError("execution exit code differs from the terminal turn")
    flag(tail, "command_truncated", clipped)
    expected_cwd, cwd_clipped = (
        bounded(execution["execution_cwd"]) if historical else (execution["execution_cwd"], False)
    )
    visible_cwd = field(tail, "execution_cwd", optional=True)
    if (visible_cwd is None and execution["execution_cwd"] != cwd
            or visible_cwd is not None and visible_cwd != expected_cwd):
        raise ValueError("model execution cwd differs from its execution")
    flag(tail, "execution_cwd_truncated", cwd_clipped and visible_cwd is not None)


def context_reasons(scenario: dict, root: Path, result, evidence: dict | None) -> list[str]:
    try:
        validate_context(scenario, root, result, evidence)
    except (ValueError, KeyError, TypeError) as exc:
        return [f"invalid assistance context: {exc}"]
    return []


def validate_context(scenario: dict, root: Path, result, evidence: dict | None) -> None:
    if not isinstance(evidence, dict):
        raise ValueError("native evidence is missing")
    accepted = evidence.get("assistance")
    if not isinstance(accepted, list) or len(accepted) != 1 or not isinstance(accepted[0], dict):
        raise ValueError("exactly one host assistance result is required")
    actual = accepted[0]
    if actual.get("input_format") != "command_assist_v1":
        raise ValueError("current host task provenance is required")
    contract = scenario["assistance"]
    inputs = evidence.get("inputs")
    if not isinstance(inputs, list) or not all(isinstance(item, dict) for item in inputs):
        raise ValueError("model input observations are missing")
    opens = [item for item in inputs if item.get("ev") == "open"]
    starts = [item for item in inputs if item.get("ev") == "step_start"]
    if len(opens) != 1 or not starts:
        raise ValueError("exactly one Assist session with an initial request is required")
    spec = opens[0]
    suffix = "background" if contract["automatic"] else "foreground"
    if spec.get("label") != f"command_assist.{contract['intent']}.{suffix}":
        raise ValueError("session intent differs from the scenario")
    tools = spec.get("tools")
    if (not isinstance(tools, list) or not all(isinstance(tool, dict) for tool in tools)
            or [tool.get("name") for tool in tools] != ["command_help", "read_file", "grep"]):
        raise ValueError("unexpected Assist tool definitions")
    messages = starts[0].get("messages")
    if (not isinstance(messages, list) or len(messages) != 1 or not isinstance(messages[0], dict)
            or messages[0].get("role") != "user" or not isinstance(messages[0].get("text"), str)):
        raise ValueError("the initial request must be one nosh User packet")
    outside, blocks = packet(messages[0]["text"])
    cwd = str(root)
    if field(outside, "cwd") != cwd:
        raise ValueError("model cwd differs from the trial workspace")
    if contract["intent"] == "generate":
        if ([(language, text) for language, text, _ in blocks] != [("text", scenario["input"])]
                or actual.get("execution") is not None or actual.get("recent_executions")
                or re.search(r"(?m)^(?:exit_code|execution_cwd|command_id): ", outside)):
            raise ValueError("Generate must contain its verbatim request without execution history")
        return

    execution = actual.get("execution")
    if (not isinstance(execution, dict) or execution.get("command") != scenario["inputs"][-1]
            or execution.get("execution_cwd") != cwd
            or type(execution.get("exit")) is not int
            or (execution["exit"] == 0) != (contract["intent"] == "next")
            or type(actual.get("command_id")) is not int or actual["command_id"] <= 0
            or execution.get("command_id") != actual.get("command_id")):
        raise ValueError("host execution differs from the declared user command")
    turns = result.turns
    if (not isinstance(turns, list) or not all(isinstance(turn, dict) for turn in turns)
            or [turn.get("input") for turn in turns] != scenario["inputs"]):
        raise ValueError("terminal input sequence differs from the scenario")
    commands = [block for block in blocks if block[0] == "bash"]
    if contract["intent"] == "next":
        history = actual.get("recent_executions", [])
        history_turns = [
            turn for turn in turns[:-1] if turn["input"] not in ("#auto off", "#auto on")
        ][-3:]
        expected_history = [turn["input"] for turn in history_turns]
        if (not isinstance(history, list) or not all(isinstance(item, dict) for item in history)
                or [item.get("command") for item in history] != expected_history
                or len(commands) != len(blocks) or len(commands) != len(history) + 1):
            raise ValueError("Next history is missing, changed or not model-visible")
        for block, item, turn in zip(commands[:-1], history, history_turns):
            execution_payload(block, item, cwd, turn, historical=True)
        execution_payload(commands[-1], execution, cwd, turns[-1], historical=False)
    else:
        captures = [block for block in blocks if block[0] == "text"]
        if len(commands) != 1 or len(captures) != 1 or len(blocks) != 2 or actual.get("recent_executions"):
            raise ValueError("Fix needs its command and captured output, not additional history")
        execution_payload(commands[0], execution, cwd, turns[-1], historical=False)
        capture = actual.get("captured_output")
        if not isinstance(capture, dict):
            raise ValueError("Fix capture provenance is missing")
        command, command_clipped = bounded(execution["command"])
        execution_cwd, cwd_clipped = bounded(cwd)
        if (capture.get("command_id") != execution["command_id"] or capture.get("command") != command
                or capture.get("execution_cwd") != execution_cwd or capture.get("exit") != execution["exit"]
                or capture.get("command_truncated") is not command_clipped
                or capture.get("cwd_truncated") is not cwd_clipped
                or capture.get("state") != "captured" or capture.get("source") != "terminal"
                or capture.get("mixed") is not False
                or any(type(capture.get(key)) is not bool for key in ("truncated", "incomplete"))):
            raise ValueError("Fix capture is unavailable, mixed or bound to another execution")
        for key in ("truncated", "incomplete"):
            flag(outside, key, capture[key])
        flag(outside, "concurrent_output", False)
        if field(outside, "state", optional=True) not in (None, "captured"):
            raise ValueError("model capture state differs from the host record")
        body = captures[0][1]
        retained = capture.get("retained_bytes")
        if type(retained) is not int or not 0 < retained <= 4096 or len(body.encode("utf-8")) != retained:
            raise ValueError("model failure output differs from the captured byte count")
        matching = [turn for turn in turns if turn.get("input") == execution["command"]]
        if len(matching) != 1 or body not in driver.plain(matching[0].get("output", "")):
            raise ValueError("model failure output is absent from the original terminal turn")
    if len(re.findall(r"(?m)^exit_code: ", outside)) != len(commands):
        raise ValueError("unexpected execution metadata outside recorded commands")
