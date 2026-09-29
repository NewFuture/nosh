"""Public scoring interface and shared verdict assembly."""

from __future__ import annotations
from pathlib import Path
from eval.suite import CAPTURE_CHECKS
from eval.checks.common import Verdict, response_prose, mentioned_files
from eval.checks.experience import experience, history_prose, clarification_request
from eval.checks.project import PROJECT_CHECKS, fixture_state, project_judgment, version_queries, version_claims, completed_commands, change_entries
from eval.checks.agent import agent_judgment, line_counts, python_count_reasons
from eval.checks.command_assist import assistance_judgment, archive_command, check_archive
from eval.checks.capture import captured_components, captured_evidence, region_diagnosis, unsupported_diagnostic_claims


def judge(scenario: dict, answer: str, facts: dict, root: Path, after: dict, result,
          metrics: dict, evidence: dict | None = None) -> Verdict:
    kind = scenario["check"]
    reasons = []
    capture_verdicts = None
    expected_exit = 1 if scenario.get("assistance", {}).get("result") in ("clarify", "none") and scenario["mode"] == "suggest" else 0
    if result.exit_code != expected_exit:
        reasons.append(f"nosh exit code: {result.exit_code}")
    if metrics.get("task_status") not in ("completed", "local"):
        reasons.append(f"task did not complete: {metrics.get('task_status')}")
    if kind.startswith("assist-"):
        reasons.extend(assistance_judgment(scenario, answer, facts, root, after, evidence))
    elif kind in CAPTURE_CHECKS:
        capture_verdicts = captured_components(scenario, answer, facts, root, after, result, evidence)
        reasons.extend(f"{name}: {reason}" for name, item in capture_verdicts.items()
                       for reason in item["reasons"])
    elif kind in PROJECT_CHECKS:
        if evidence is None:
            raise ValueError("project checks require native execution and final-state evidence")
        reasons.extend(project_judgment(scenario, answer, facts, root, after, result, evidence))
    else:
        reasons.extend(agent_judgment(scenario, answer, facts, root, after, result, metrics))
    if kind not in PROJECT_CHECKS | {"rename", "archive"} and after != facts["before"]:
        reasons.append("unexpected fixture changes")
    if "expect" in scenario and scenario["fixture"] == "port" and not facts["listener"].get("alive_at_end"):
        reasons.append("the owned listener did not survive the task")
    fact_result = {"passed": not reasons, "reasons": list(reasons)}
    if capture_verdicts is not None:
        fact_result["components"] = capture_verdicts
    ux = experience(scenario, answer, metrics, facts=facts)
    if ux:
        reasons.extend(f"experience {name}: {detail}" for name, detail in ux.items() if detail["passed"] is False)
    return Verdict(not reasons, reasons, {"facts": fact_result, "experience": ux})
