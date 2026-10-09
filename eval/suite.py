"""Current suite schema and explicit scenario contracts."""

from __future__ import annotations

import json
from pathlib import Path, PurePosixPath
import re

from .contracts import (
    APPROVAL_CHECKS, ASSIST_CHECKS, CAPTURE_CHECKS, CHECK_FIXTURES, CHECKS,
    SCENARIO_FIXTURES, STATEFUL_CHECKS, check_spec, finite_number,
)

BUILTIN_SUITES = ("regression", "command-assist", "smoke", "workflows")


def positive_seconds(value, name: str) -> int | float:
    if not finite_number(value) or value <= 0:
        raise ValueError(f"{name} must be finite and positive")
    return value


def seeds(value: list) -> list[int]:
    if not isinstance(value, list) or not value or any(type(n) is not int or not 0 <= n < 2**64 for n in value):
        raise ValueError("seeds must be a nonempty list of u64 integers")
    if len(value) != len(set(value)):
        raise ValueError("duplicate seeds")
    return value


def suite_path(path: Path | str) -> Path:
    if isinstance(path, str) and path in BUILTIN_SUITES:
        return Path(__file__).resolve().parent / "suites" / f"{path}.json"
    return Path(path)


def load_suite(path: Path | str) -> dict:
    path = suite_path(path)
    suite = json.loads(path.read_text(encoding="utf-8"))
    return validate_suite(suite, catalog_root=path.parent)


def expand_catalogs(suite: dict, root: Path) -> dict:
    catalogs, selected = suite["catalogs"], suite.get("scenarios")
    if (suite["schema_version"] != 2 or not isinstance(catalogs, list) or not catalogs
            or not all(isinstance(name, str) and name for name in catalogs)):
        raise ValueError("catalogs must be explicit paths in a schema v2 suite")
    if (not isinstance(selected, list) or not selected
            or not all(isinstance(sid, str) and sid for sid in selected)):
        raise ValueError("catalog suites must select a nonempty ordered list of scenario IDs")
    definitions = {}
    for name in catalogs:
        catalog = root / name
        entries = json.loads(catalog.read_text(encoding="utf-8"))
        if not isinstance(entries, list) or not entries:
            raise ValueError(f"scenario catalog must be a nonempty array: {catalog}")
        for entry in entries:
            sid = entry.get("id") if isinstance(entry, dict) else None
            if not isinstance(sid, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", sid):
                raise ValueError(f"invalid scenario ID in catalog: {catalog}")
            if sid in definitions:
                raise ValueError(f"duplicate catalog scenario: {sid}")
            definitions[sid] = entry
    missing = set(selected) - definitions.keys()
    if missing:
        raise ValueError(f"unknown scenario IDs: {', '.join(sorted(missing))}")
    expanded = {key: value for key, value in suite.items() if key != "catalogs"}
    expanded["scenarios"] = [definitions[sid] for sid in selected]
    return expanded


def validate_suite(suite: dict, *, catalog_root: Path | None = None) -> dict:
    if not isinstance(suite, dict) or type(suite.get("schema_version")) is not int or suite["schema_version"] != 2:
        raise ValueError("unsupported scenario schema")
    if set(suite) - {"schema_version", "dataset_revision", "seeds", "timeout_s", "scenarios", "catalogs"}:
        raise ValueError("unknown suite fields")
    if "catalogs" in suite:
        if catalog_root is None:
            raise ValueError("catalog validation requires an explicit catalog_root or load_suite")
        suite = expand_catalogs(suite, catalog_root)
    revision = suite.get("dataset_revision")
    if type(revision) is not int or revision < 1:
        raise ValueError("dataset_revision must be a positive integer")
    seeds(suite.get("seeds"))
    positive_seconds(suite.get("timeout_s"), "timeout_s")
    scenarios = suite.get("scenarios")
    if not isinstance(scenarios, list) or not scenarios:
        raise ValueError("scenarios must be a nonempty list")
    ids = set()
    for scenario in scenarios:
        if not isinstance(scenario, dict):
            raise ValueError("scenario must be an object")
        fields = {"id", "title", "mode", "fixture", "inputs", "input",
                  "corrections", "stdin_command", "stdin_file", "approval", "check",
                  "group", "expect", "completions", "capture_output", "assistance"}
        if set(scenario) - fields:
            raise ValueError("unknown scenario fields")
        kind = scenario.get("check")
        if not isinstance(kind, str) or kind not in CHECKS:
            raise ValueError(f"unknown fixture or check: {scenario.get('id')}")
        spec = check_spec(kind)
        assistance = scenario.get("assistance")
        if assistance is not None:
            if (not isinstance(assistance, dict)
                    or set(assistance) - {"intent", "result", "automatic"}
                    or not {"intent", "result", "automatic"} <= set(assistance)
                    or assistance["intent"] not in ("generate", "fix", "next")
                    or assistance["result"] not in ("command", "none")
                    or type(assistance["automatic"]) is not bool):
                raise ValueError("invalid command assistance contract")
            if assistance["automatic"] != (scenario.get("mode") == "repl"):
                raise ValueError("automatic assistance requires a REPL completion")
            if (assistance["intent"] == "generate") != (scenario.get("mode") == "suggest"):
                raise ValueError("generate requires suggest mode; fix/next require REPL mode")
            if spec.assist_result is not None and assistance["result"] != spec.assist_result:
                raise ValueError("assistance result does not match the check")
            if spec.assist_intent is not None and assistance["intent"] != spec.assist_intent:
                raise ValueError("assistance intent does not match the check")
        if kind in ASSIST_CHECKS and assistance is None:
            raise ValueError("assistance checks require an assistance contract")
        capture = scenario.get("capture_output")
        if "capture_output" in scenario and capture not in ("off", "last"):
            raise ValueError("capture_output must be off or last")
        if capture == "last" and scenario.get("mode") != "repl":
            raise ValueError("user output capture requires REPL mode")
        if scenario.get("check") in CAPTURE_CHECKS and capture != "last":
            raise ValueError("captured failure diagnosis requires capture_output=last")
        sid = scenario.get("id")
        if not isinstance(sid, str) or not re.fullmatch(r"[a-z][a-z0-9-]*", sid) or sid in ids:
            raise ValueError(f"invalid/duplicate scenario id: {sid}")
        ids.add(sid)
        if (not isinstance(scenario.get("fixture"), str) or not isinstance(scenario.get("check"), str)
                or scenario["fixture"] not in SCENARIO_FIXTURES or scenario["check"] not in CHECKS):
            raise ValueError(f"unknown fixture or check: {sid}")
        if scenario["fixture"] != CHECK_FIXTURES[scenario["check"]]:
            raise ValueError(f"fixture does not supply the check's required facts: {sid}")
        if not isinstance(scenario.get("title"), str) or not scenario["title"].strip():
            raise ValueError(f"missing scenario title: {sid}")
        policy = scenario.get("approval")
        if not isinstance(policy, str) or (policy != "deny" and scenario["check"] not in APPROVAL_CHECKS.get(policy, set())):
            raise ValueError(f"unknown approval policy: {sid}")
        mode = scenario.get("mode")
        if "stdin_command" in scenario:
            if mode != "agent":
                raise ValueError(f"stdin attachments require agent mode: {sid}")
            if scenario["stdin_command"] != ["git", "log", "--stat", "-8"]:
                raise ValueError(f"unsupported stdin producer: {sid}")
        attachment = scenario.get("stdin_file")
        if "stdin_file" in scenario:
            if (mode != "agent" or "stdin_command" in scenario or not isinstance(attachment, str)
                    or not attachment or any(ord(c) < 32 or ord(c) == 127 for c in attachment)
                    or "\\" in attachment or ":" in attachment):
                raise ValueError(f"invalid stdin_file attachment: {sid}")
            relative = PurePosixPath(attachment)
            if relative.is_absolute() or ".." in relative.parts or not relative.parts or relative.as_posix() != attachment:
                raise ValueError(f"stdin_file must be a canonical fixture-relative path: {sid}")
        if scenario["check"] in ("config-lookup", "config-missing") and attachment != "incident.txt":
            raise ValueError(f"configuration lookup requires the incident.txt attachment: {sid}")
        if mode == "repl":
            inputs = scenario.get("inputs")
        elif mode in ("agent", "suggest"):
            inputs = [scenario.get("input")]
        else:
            raise ValueError(f"unknown mode: {sid}")
        if (not isinstance(inputs, list) or not inputs
                or not all(isinstance(s, str) and s and "\0" not in s and "\r" not in s and "\n" not in s for s in inputs)):
            raise ValueError(f"inputs must be nonempty single lines: {sid}")
        if mode == "repl":
            corrections = scenario.get("corrections")
            if corrections is not None and (
                not isinstance(corrections, list) or len(corrections) != len(inputs)
                or not all(isinstance(c, str) and c for c in corrections)
            ):
                raise ValueError(f"invalid corrections: {sid}")
            if (scenario["check"] == "typos") != (corrections is not None):
                raise ValueError(f"only local correction cases must specify corrections: {sid}")
            completions = scenario.get("completions")
            if not isinstance(completions, list) or len(completions) != len(inputs):
                raise ValueError(f"each REPL input requires a completion contract: {sid}")
            for completion in completions:
                if not isinstance(completion, dict):
                    raise ValueError(f"invalid completion contract: {sid}")
                kind = completion.get("kind")
                if kind not in ("agent", "shell", "correction", "assist", "observe"):
                    raise ValueError(f"unknown input completion: {sid}")
                if kind == "shell":
                    code = completion.get("exit_code")
                    contains = completion.get("contains")
                    if (set(completion) != {"kind", "exit_code", "contains"}
                            or type(code) is not int or not 1 <= code <= 255
                            or not isinstance(contains, list) or not contains
                            or not all(isinstance(s, str) and s and "\n" not in s for s in contains)):
                        raise ValueError(f"invalid failed-command completion: {sid}")
                elif kind == "agent":
                    if set(completion) - {"kind", "answers"}:
                        raise ValueError(f"unknown completion fields: {sid}")
                    if "answers" in completion:
                        answers = completion["answers"]
                        if (not isinstance(answers, list) or not answers
                                or not all(isinstance(answer, str) and answer.strip()
                                           and len(answer.encode("utf-8")) <= 4096
                                           and all(c.isprintable() for c in answer)
                                           for answer in answers)):
                            raise ValueError(f"answers must be nonempty single-line user replies: {sid}")
                elif set(completion) != {"kind"}:
                    raise ValueError(f"unknown completion fields: {sid}")
                if kind == "assist" and (assistance is None or not assistance["automatic"]):
                    raise ValueError("assist completion requires automatic assistance")
                if (kind == "correction") != (corrections is not None):
                    raise ValueError(f"correction completion does not match inputs: {sid}")
            if corrections is None and completions[-1]["kind"] not in ("agent", "assist"):
                raise ValueError(f"the final REPL input must ask the agent: {sid}")
            if (scenario["check"] in {"build-failure", "test-failure", "port-failure", *CAPTURE_CHECKS}
                    and completions[0]["kind"] != "shell"):
                raise ValueError(f"failure diagnosis requires an initial failed shell command: {sid}")
            if (scenario["check"] in CAPTURE_CHECKS
                    and not re.fullmatch(r"ai fix(?: .+)?", inputs[-1])):
                raise ValueError(f"captured diagnosis requires an explicit ai fix input: {sid}")
        else:
            if any(key in scenario for key in ("inputs", "corrections", "completions")):
                raise ValueError(f"REPL fields in a CLI scenario: {sid}")
        if scenario["check"] in STATEFUL_CHECKS | {"typos", "failure", "cwd", "cwd-follow-up", "denied-rename"} and scenario["mode"] != "repl":
            raise ValueError(f"this check requires a shared interactive session: {sid}")
        if scenario.get("group") not in ("mvp", "expanded", "command-assist"):
            raise ValueError(f"invalid scenario group: {sid}")
        expect = scenario.get("expect")
        if not isinstance(expect, dict) or set(expect) != {
            "max_steps", "max_confirmations", "response_language", "final_question",
        }:
            raise ValueError(f"all experience expectations must be declared: {sid}")
        if any(type(expect[k]) is not int or expect[k] < 0 for k in ("max_steps", "max_confirmations")):
            raise ValueError(f"experience limits must be nonnegative integers: {sid}")
        nonprose = scenario["check"] == "typos" or scenario["mode"] == "suggest" or assistance is not None
        if (expect["response_language"] not in ("zh", "any", "not_applicable")
                or (expect["response_language"] == "not_applicable") != nonprose):
            raise ValueError(f"invalid response language expectation: {sid}")
        has_answers = any(completion.get("answers") for completion in scenario.get("completions", []))
        question = "not_applicable" if nonprose else "require" if scenario["check"] == "clarification" and not has_answers else "forbid"
        if expect["final_question"] != question:
            raise ValueError(f"invalid final-question expectation: {sid}")
        if (expect["max_steps"] == 0) != (scenario["check"] == "typos"):
            raise ValueError(f"only local correction has a zero-step budget: {sid}")
    return suite
