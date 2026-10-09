"""Shared evaluation scenarios and execution evidence fixtures."""

import copy
import json
import re

from eval import driver, runtime, suite as suite_api

SUITE = suite_api.load_suite(runtime.HERE / "suites" / "regression.json")


SCENARIOS = {s["id"]: s for s in SUITE["scenarios"]}


def execution(command, code=0, stdout="", stderr=""):
    return {
        "call": {"name": "exec", "args": {"command": command}},
        "state": "executed", "exit_code": code, "timed_out": False, "interrupted": False,
        "result": f"[exit_code={code} duration=0.00s truncated=no]\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    }


def bind_assist_test_context(evidence, scenario, root, result):
    """Supply a current packet for scorer-only fixtures, not for native CLI tests."""
    evidence = copy.deepcopy(evidence)
    host = evidence["assistance"][0]
    intent = scenario["assistance"]["intent"]
    host["input_format"] = "command_assist_v1"

    def block(text, language):
        width = max(3, max((len(match[0]) for match in re.finditer(r"`+", text)), default=0) + 1)
        marker = "`" * width
        return f"{marker}{language}\n{text}\n{marker}"

    packet = "Unit task packet:\n\nEnvironment:\ncwd: " + json.dumps(str(root)) + "\n\n"
    if intent == "generate":
        host["command_id"] = None
        host["execution"] = None
        packet += block(scenario["input"], "text")
    else:
        record = host.get("execution") or {}
        host["command_id"] = record.get("command_id", 1)
        record.setdefault("command_id", host["command_id"])
        record.setdefault("command", scenario["inputs"][-1])
        record.setdefault("execution_cwd", str(root))
        record.setdefault("exit", 1 if intent == "fix" else 0)
        record.setdefault("command_truncated", False)
        record.setdefault("status", "failed" if record["exit"] else "succeeded")
        host["execution"] = record
        for item in [*host.get("recent_executions", []), record]:
            packet += block(item["command"], "bash") + f"\nexit_code: {item['exit']}\n\n"
        if intent == "fix":
            matching = [turn for turn in result.turns if turn.get("input") == record["command"]]
            body = driver.plain(matching[0]["output"]) if matching else "fixture failure\n"
            capture = {
                "command_id": record["command_id"], "command": record["command"],
                "execution_cwd": record["execution_cwd"], "exit": record["exit"],
                "command_truncated": False, "cwd_truncated": False,
                "state": "captured", "source": "terminal", "retained_bytes": len(body.encode()),
                "truncated": False, "incomplete": False, "mixed": False,
            }
            host.setdefault("captured_output", capture)
            packet += "Terminal output (stdout/stderr not separated):\n" + block(body, "text")
        else:
            body = ""
        if not result.turns:
            result.turns = [{"input": line, "output": body} for line in scenario["inputs"]]
    evidence["inputs"] = [
        {"ev": "open", "sid": 1, "label": f"command_assist.{intent}."
         + ("background" if scenario["assistance"]["automatic"] else "foreground"),
         "tools": [{"name": name} for name in ("command_help", "read_file", "grep")]},
        {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": packet}]},
    ]
    return evidence
