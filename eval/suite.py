"""Versioned scenario contracts and check/fixture compatibility."""

from __future__ import annotations

import json
import math
from pathlib import Path
import re

from . import fixtures
from .approval import APPROVAL_CHECKS

LEGACY_CHECKS = {"largest", "port", "lines", "rename", "python", "typos", "failure", "history", "archive", "cwd"}


CHECK_FIXTURES = {
    "largest": "big", "port": "port", "lines": "project", "rename": "rename",
    "python": "project", "typos": "typo", "failure": "failure", "history": "history",
    "archive": "logs", "cwd": "big",
    "rust-build": "rust", "rust-test": "rust", "rust-clean": "rust-built",
    "node-build": "node", "node-test": "node", "python-test": "python",
    "git-diff": "dirty-git", "git-commit": "staged-git", "recent-history": "history",
    "versions": "python", "clarification": "python",
    "build-failure": "rust-broken", "test-failure": "python-broken", "port-failure": "port",
}


CHECKS = set(CHECK_FIXTURES)


NATIVE_CHECKS = CHECKS - LEGACY_CHECKS


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
    if set(suite) - {"schema_version", "dataset_revision", "seeds", "timeout_s", "scenarios"}:
        raise ValueError("unknown suite fields")
    revision = suite.get("dataset_revision", 1)
    if type(revision) is not int or revision < 1:
        raise ValueError("dataset_revision must be a positive integer")
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
        legacy_commit_fixture = (
            revision == 1 and scenario["check"] == "git-commit" and scenario["fixture"] == "dirty-git"
        )
        if scenario["fixture"] != CHECK_FIXTURES[scenario["check"]] and not legacy_commit_fixture:
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
                if (scenario["check"] in ("build-failure", "test-failure", "port-failure")
                        and completions[0]["kind"] != "shell"):
                    raise ValueError(f"failure diagnosis requires an initial failed shell command: {sid}")
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
