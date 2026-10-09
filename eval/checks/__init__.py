"""Public scoring interface and shared verdict assembly."""

from __future__ import annotations
from pathlib import Path
from ..contracts import STATEFUL_CHECKS, check_spec
from . import agent, assist_context, capture, command_assist, experience, project
from .common import Verdict


def judge(scenario: dict, answer: str, facts: dict, root: Path, after: dict, result,
          metrics: dict, evidence: dict | None = None) -> Verdict:
    kind = scenario["check"]
    spec = check_spec(kind)
    family = spec.family
    reasons = []
    capture_verdicts = None
    expected_exit = 1 if scenario.get("assistance", {}).get("result") == "none" and scenario["mode"] == "suggest" else 0
    if result.exit_code != expected_exit:
        reasons.append(f"nosh exit code: {result.exit_code}")
    if metrics.get("task_status") not in ("completed", "local"):
        reasons.append(f"task did not complete: {metrics.get('task_status')}")
    if family == "assist":
        context_errors = assist_context.context_reasons(scenario, root, result, evidence)
        reasons.extend(context_errors)
        if not context_errors:
            reasons.extend(command_assist.assistance_judgment(scenario, answer, facts, root, after, evidence))
    elif family == "capture":
        capture_verdicts = capture.captured_components(scenario, answer, facts, root, after, result, evidence)
        reasons.extend(f"{name}: {reason}" for name, item in capture_verdicts.items()
                       for reason in item["reasons"])
    elif family == "project":
        if evidence is None:
            raise ValueError("project checks require native execution and final-state evidence")
        reasons.extend(project.project_judgment(scenario, answer, facts, root, after, result, evidence))
    elif family == "agent":
        reasons.extend(agent.agent_judgment(scenario, answer, facts, root, after, result, metrics, evidence))
    else:
        raise ValueError(f"unknown scoring family: {family}")
    if kind not in STATEFUL_CHECKS and not spec.custom_state and after != facts["before"]:
        reasons.append("unexpected fixture changes")
    if "expect" in scenario and scenario["fixture"] == "port" and not facts["listener"].get("alive_at_end"):
        reasons.append("the owned listener did not survive the task")
    fact_result = {"passed": not reasons, "reasons": list(reasons)}
    if capture_verdicts is not None:
        fact_result["components"] = capture_verdicts
    ux = experience.experience(scenario, answer, metrics, facts=facts)
    if ux:
        reasons.extend(f"experience {name}: {detail}" for name, detail in ux.items() if detail["passed"] is False)
    return Verdict(not reasons, reasons, {"facts": fact_result, "experience": ux})
