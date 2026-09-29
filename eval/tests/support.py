"""Shared evaluation scenarios and execution evidence fixtures."""

from eval import runtime, suite as suite_api

SUITE = suite_api.load_suite(runtime.HERE / "suites" / "regression.json")


SCENARIOS = {s["id"]: s for s in SUITE["scenarios"]}


def execution(command, code=0, stdout="", stderr=""):
    return {
        "call": {"name": "run_command", "args": {"command": command}},
        "state": "executed", "exit_code": code, "timed_out": False, "interrupted": False,
        "result": f"[exit_code={code} duration=0.00s truncated=no]\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    }
