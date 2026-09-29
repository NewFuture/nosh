"""Current evaluation relationships and shared primitive validation."""

from __future__ import annotations

from dataclasses import dataclass
import math
from typing import Literal


@dataclass(frozen=True)
class CheckSpec:
    fixture: str
    family: Literal["agent", "project", "capture", "assist"] = "agent"
    approval: str = "deny"
    citation: bool = False
    custom_state: bool = False
    assist_result: Literal["command", "clarify", "none"] | None = None
    assist_intent: Literal["generate", "fix", "next"] | None = None


CHECK_SPECS = {
    "largest": CheckSpec("big"),
    "port": CheckSpec("port"),
    "lines": CheckSpec("project"),
    "rename": CheckSpec("rename", approval="rename", custom_state=True),
    "denied-rename": CheckSpec("rename"),
    "python": CheckSpec("project"),
    "typos": CheckSpec("typo"),
    "failure": CheckSpec("failure"),
    "history": CheckSpec("history"),
    "archive": CheckSpec("logs", custom_state=True),
    "cwd": CheckSpec("big", approval="cwd"),
    "cwd-follow-up": CheckSpec("big", approval="cwd"),
    "rust-build": CheckSpec("rust", "project", "rust-build"),
    "rust-test": CheckSpec("rust", "project", "rust-test"),
    "rust-clean": CheckSpec("rust-built", "project", "rust-clean"),
    "node-build": CheckSpec("node", "project", "node-build"),
    "node-test": CheckSpec("node", "project", "node-test"),
    "python-test": CheckSpec("python", "project", "python-test"),
    "git-diff": CheckSpec("dirty-git", "project"),
    "git-commit": CheckSpec("staged-git", "project", "git-commit"),
    "recent-history": CheckSpec("history", "project"),
    "versions": CheckSpec("python", "project"),
    "clarification": CheckSpec("python", "project"),
    "build-failure": CheckSpec("rust-broken", "project", "rust-build"),
    "test-failure": CheckSpec("python-broken", "project", "python-test"),
    "port-failure": CheckSpec("port", "project", "port-failure"),
    "captured-diagnosis": CheckSpec("diagnostic-failure", "capture"),
    "captured-citation": CheckSpec("diagnostic-failure", "capture", citation=True),
    "assist-archive": CheckSpec("logs", "assist", assist_result="command"),
    "assist-none": CheckSpec("logs", "assist", assist_result="none"),
    "assist-clarify": CheckSpec("logs", "assist", assist_result="clarify"),
    "assist-next-review": CheckSpec("review-workflow", "assist", assist_result="command", assist_intent="next"),
    "assist-resume-archive": CheckSpec("partial-archive", "assist", custom_state=True,
                                     assist_result="command", assist_intent="fix"),
    "config-lookup": CheckSpec("config-lookup"),
    "config-missing": CheckSpec("config-missing"),
}

CHECK_FIXTURES = {name: spec.fixture for name, spec in CHECK_SPECS.items()}
SCENARIO_FIXTURES = set(CHECK_FIXTURES.values())
CHECKS = set(CHECK_SPECS)
ASSIST_CHECKS = {name for name, spec in CHECK_SPECS.items() if spec.family == "assist"}
STATEFUL_CHECKS = {name for name, spec in CHECK_SPECS.items() if spec.family in ("project", "capture")}
CAPTURE_CHECKS = {name: spec.citation for name, spec in CHECK_SPECS.items() if spec.family == "capture"}
CAPTURE_PARTS = ("capture", "diagnosis", "citation")
APPROVAL_CHECKS = {
    policy: {name for name, spec in CHECK_SPECS.items() if spec.approval == policy}
    for policy in sorted({spec.approval for spec in CHECK_SPECS.values()} - {"deny"})
}
POLICY_FIXTURES = {
    policy: {CHECK_FIXTURES[name] for name in names}
    for policy, names in APPROVAL_CHECKS.items()
}


def check_spec(name: str) -> CheckSpec:
    if not isinstance(name, str) or name not in CHECK_SPECS:
        raise ValueError(f"unknown check: {name}")
    return CHECK_SPECS[name]


def finite_number(value) -> bool:
    if type(value) not in (int, float):
        return False
    try:
        return math.isfinite(value)
    except OverflowError:
        return False
