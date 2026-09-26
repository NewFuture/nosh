"""Reproduce this baseline's deterministic corrections; never runs a model."""

from __future__ import annotations

import argparse
import copy
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[3]
HERE = Path(__file__).resolve().parent
REVISION = "a26d1b2ddf1d06dfcb1bdda2bdae8ddb6922cf12"
TIMEOUT = ("zh-clean-build", 1, 1)
DENIAL = "The declared approval policy denied at least one command; see approval evidence."
SOURCE_FILES = ("__init__.py", "checks.py", "driver.py", "fixtures.py", "report.py", "run.py")


def sha(text: str) -> str:
    return hashlib.sha256(text.encode("utf-8")).hexdigest()


def frozen_sources() -> dict[str, bytes]:
    return {
        name: subprocess.check_output([os.environ.get("GIT", "git"), "show", f"{REVISION}:eval/{name}"], cwd=ROOT)
        for name in SOURCE_FILES
    }


def normalized(sources: dict) -> tuple[dict, dict]:
    workflow = json.loads((HERE / "workflow-provenance.json").read_text(encoding="utf-8"))
    raw = json.loads((HERE / "raw-report.json").read_text(encoding="utf-8"))
    report.validate(raw)
    if fixtures.file_hash(HERE / "raw-report.json") != workflow["attributed_report_hashes"]["report.json"]:
        raise ValueError("raw report hash mismatch")
    if sha(report.markdown(raw)) != workflow["attributed_report_hashes"]["report.md"]:
        raise ValueError("raw Markdown hash mismatch")
    initial = copy.deepcopy(raw)
    initial["metadata"].pop("harness_revision")
    if sha(json.dumps(initial, ensure_ascii=False, indent=2, allow_nan=False) + "\n") != workflow["original_report_hashes"]["report.json"]:
        raise ValueError("original report cannot be reconstructed")
    if sha(report.markdown(initial)) != workflow["original_report_hashes"]["report.md"]:
        raise ValueError("original Markdown cannot be reconstructed")
    scenarios = {s["id"]: s for s in raw["metadata"]["scenarios"]}
    expected = {(sid, seed, repeat) for sid in scenarios for seed in range(5) for repeat in range(2)}
    if len(scenarios) != 25 or {report.trial_key(t) for t in raw["trials"]} != expected:
        raise ValueError("the campaign does not contain every planned identity")
    if [report.trial_key(t) for t in raw["trials"] if t["status"] == "error"] != [TIMEOUT]:
        raise ValueError("unexpected original error set")
    data = copy.deepcopy(raw)
    changes = []
    for row in data["trials"]:
        key = report.trial_key(row)
        scenario = scenarios[key[0]]
        before = copy.deepcopy(row)
        if key == TIMEOUT:
            result = driver.Result(
                exit_code=row["exit_code"], total_s=row["metrics"]["total_s"],
                peak_rss_mib=row["metrics"]["peak_rss_mib"], approvals=row["approvals"],
                turns=row["turns"], error=row["reasons"][0], timeout_phase="agent",
                transcript=(HERE / "timeout-transcript.txt").read_text(encoding="utf-8"),
            )
            observed = run.observe(result, scenario, HERE / "timeout-engine.jsonl",
                                   False, row["seed"], inflight_timeout=True)
            if observed["metrics"]["steps"] != 7 or observed["answer"]:
                raise ValueError("timeout evidence differs from the recorded correction")
            row.update(observed)
            reason = (f"agent exceeded the {data['metadata']['settings']['timeout_s']:g}s trial deadline "
                      f"during model generation ({row['metrics']['steps']} started steps); "
                      f"step budget is {scenario['expect']['max_steps']}")
            row.update(status="fail", reasons=[reason], timeout_phase="agent",
                       grading={"facts": {"passed": False, "reasons": [reason]}, "experience": None},
                       original_trial=before)
        elif row["grading"]["experience"] is not None:
            ux = checks.experience(scenario, row["answer"], row["metrics"])
            if ux == row["grading"]["experience"]:
                continue
            row["original_judgment"] = {name: before[name] for name in ("status", "reasons", "grading")}
            row["grading"]["experience"] = ux
            reasons = list(row["grading"]["facts"]["reasons"])
            reasons.extend(f"experience {name}: {item}" for name, item in ux.items() if item["passed"] is False)
            if reasons and any(not approval["allowed"] for approval in row["approvals"]):
                reasons.append(DENIAL)
            row.update(status="fail" if reasons else "pass", reasons=reasons)
        else:
            continue
        changes.append({
            "scenario_id": key[0], "seed": key[1], "repeat": key[2],
            "before": before["status"], "after": row["status"],
            "before_reasons": before["reasons"], "after_reasons": row["reasons"],
            "metrics_recovered": key == TIMEOUT,
        })
    restored = []
    for row in data["trials"]:
        if "original_trial" in row:
            restored.append(row["original_trial"])
        else:
            item = copy.deepcopy(row)
            if "original_judgment" in item:
                item.update(item.pop("original_judgment"))
            restored.append(item)
    if restored != raw["trials"]:
        raise ValueError("normalization did not preserve the original trial payload")
    changed_verdicts = sum(item["before"] != item["after"] for item in changes)
    data["metadata"]["grading_content_sha256"] = sources["checks.py"]
    data["metadata"]["regrade"] = {
        "revision": REVISION, "changed_verdicts": changed_verdicts,
        "original_grading_content_sha256": raw["metadata"]["grading_content_sha256"],
        "note": "Deterministic corrections only: distinguish declarative closing advice from questions, "
                "and classify one proven in-flight generation deadline as a failed task. "
                "Its seven started steps and first-step TTFT are recovered from the preserved native trace; "
                "there is no final answer. Raw reports, judgments and the original timeout trial remain available. "
                "No sampling, approvals, budgets or factual judgments for the other 249 trials changed.",
    }
    evidence = {
        "schema_version": 1, "complete": True, "normalization_revision": REVISION,
        "normalization_sources": sources,
        "runtime_harness_revision": raw["metadata"]["harness_revision"],
        "source_revision": raw["metadata"]["build"]["source_revision"],
        "workflow_url": workflow["workflow_url"], "workflow_conclusion": "failure before normalization",
        "raw_workflow_provenance": workflow,
        "raw_trial_payload_sha256": fixtures.digest(raw["trials"]),
        "restored_trial_payload_sha256": fixtures.digest(restored),
        "timeout_evidence_sha256": {name: fixtures.file_hash(HERE / name) for name in (
            "timeout-engine.jsonl", "timeout-transcript.txt")},
        "changes": changes,
        "prior_attempts": [
            {"run": 36167381077, "outcome": "hosted runner lost communication; no report/artifact available"},
            {"run": 36196161448, "outcome": "complete diagnostic campaign, excluded from the accepted baseline because "
             "the approval whitelist rejected safe stderr merging; all 250 original records retained",
             "report_sha256": fixtures.file_hash(HERE / "diagnostic-report.json")},
        ],
    }
    return data, evidence


def main() -> int:
    global checks, driver, fixtures, report, run

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="verify committed outputs without overwriting them")
    parser.add_argument("--source-root", type=Path, help=argparse.SUPPRESS)
    args = parser.parse_args()
    sources = frozen_sources()
    if args.source_root is None:
        with tempfile.TemporaryDirectory(prefix="nosh-baseline-source-") as temporary:
            package = Path(temporary) / "eval"
            package.mkdir()
            for name, content in sources.items():
                (package / name).write_bytes(content)
            return subprocess.run(
                [sys.executable, str(Path(__file__).resolve()), *sys.argv[1:], "--source-root", temporary],
                env=dict(os.environ, PYTHONDONTWRITEBYTECODE="1"),
            ).returncode
    for name, content in sources.items():
        if (args.source_root / "eval" / name).read_bytes() != content:
            raise ValueError(f"frozen normalization source changed: {name}")
    sys.path.insert(0, str(args.source_root))
    from eval import checks, driver, fixtures, report, run

    hashes = {name: sha(content.decode("utf-8").replace("\r\n", "\n").replace("\r", "\n"))
              for name, content in sources.items() if name != "__init__.py"}
    data, evidence = normalized(hashes)
    with tempfile.TemporaryDirectory() as temporary:
        destination = Path(temporary) if args.check else HERE
        report.save(data, destination)
        evidence["derived_report_hashes"] = {name: fixtures.file_hash(destination / name)
                                            for name in ("report.json", "report.md")}
        (destination / "provenance.json").write_text(json.dumps(evidence, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
        if args.check:
            for name in ("report.json", "report.md", "provenance.json"):
                if (destination / name).read_bytes() != (HERE / name).read_bytes():
                    raise ValueError(f"derived baseline differs: {name}")
    print(json.dumps({"groups": data["groups"], "changed_records": len(evidence["changes"]),
                      "changed_verdicts": data["metadata"]["regrade"]["changed_verdicts"]}, ensure_ascii=False, indent=2))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
