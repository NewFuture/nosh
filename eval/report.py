"""Versioned results, paired comparisons, and a self-contained human report."""

from __future__ import annotations

from collections import defaultdict
import json
import math
import os
from pathlib import Path
import re
import statistics
import tempfile

SCHEMA_VERSION = 1
METRICS = ("steps", "confirmations", "ttft_s", "total_s", "peak_rss_mib")


def validate(report: dict) -> None:
    if not isinstance(report, dict) or report.get("schema_version") != SCHEMA_VERSION:
        raise ValueError("unsupported report schema")
    if not isinstance(report.get("metadata"), dict) or not isinstance(report.get("trials"), list):
        raise ValueError("report must contain metadata and trials")
    seen = set()
    for trial in report["trials"]:
        key = trial_key(trial)
        if key in seen:
            raise ValueError(f"duplicate trial: {key}")
        seen.add(key)
        if trial.get("status") not in ("pass", "fail", "error"):
            raise ValueError(f"invalid trial status: {key}")
        metrics = trial.get("metrics")
        if not isinstance(metrics, dict):
            raise ValueError(f"missing metrics: {key}")
        if any(name not in metrics for name in METRICS):
            raise ValueError(f"missing metric fields: {key}")
        if not isinstance(trial.get("answer"), str) or "final_state" not in trial:
            raise ValueError(f"missing answer or final-state evidence: {key}")
        for name in METRICS:
            value = metrics.get(name)
            if value is not None and (type(value) not in (int, float) or not math.isfinite(value) or value < 0):
                raise ValueError(f"invalid {name} for {key}")


def trial_key(trial: dict) -> tuple:
    try:
        key = (trial["scenario_id"], trial["seed"], trial["repeat"])
    except (KeyError, TypeError) as exc:
        raise ValueError("trial identity is missing") from exc
    if not isinstance(key[0], str) or not re.fullmatch(r"[a-z][a-z0-9-]*", key[0]):
        raise ValueError("invalid scenario identity")
    if type(key[1]) is not int or not 0 <= key[1] < 2**64 or type(key[2]) is not int or key[2] < 0:
        raise ValueError("invalid seed or repetition")
    return key


def compare(current: dict, previous: dict) -> dict:
    validate(current)
    validate(previous)
    a, b = current["metadata"], previous["metadata"]
    warnings = []
    for key in ("suite_sha256", "grading_content_sha256", "model", "settings", "machine", "tools", "observation"):
        if a.get(key) != b.get(key):
            warnings.append(f"{key} differs; paired deltas are descriptive, not a controlled regression")
    if a.get("harness_content_sha256", a.get("harness_sha256")) != b.get("harness_content_sha256", b.get("harness_sha256")):
        warnings.append("harness sources differ; paired deltas are descriptive, not a controlled regression")
    old = {trial_key(t): t for t in previous["trials"]}
    changes = []
    for trial in current["trials"]:
        prior = old.get(trial_key(trial))
        if prior is None:
            continue
        deltas = {}
        for name in METRICS:
            now, before = trial["metrics"].get(name), prior["metrics"].get(name)
            deltas[name] = now - before if now is not None and before is not None else None
        changes.append({
            "scenario_id": trial["scenario_id"], "seed": trial["seed"], "repeat": trial["repeat"],
            "before": prior["status"], "after": trial["status"], "metric_delta": deltas,
            "sampling_changed": trial.get("sampling") != prior.get("sampling"),
        })
    if any(change["sampling_changed"] for change in changes):
        warnings.append("observed sampling parameters differ")
    return {
        "previous_run": previous["metadata"].get("run_id"),
        "warnings": warnings,
        "paired_trials": len(changes),
        "unpaired_current": len(current["trials"]) - len(changes),
        "unpaired_previous": len(previous["trials"]) - len(changes),
        "regressions": [c for c in changes if c["before"] == "pass" and c["after"] != "pass"],
        "improvements": [c for c in changes if c["before"] != "pass" and c["after"] == "pass"],
        "pairs": changes,
    }


def repetitions(trials: list[dict]) -> list[dict]:
    groups = defaultdict(list)
    for trial in trials:
        groups[(trial["scenario_id"], trial["seed"])].append(trial)
    results = []
    for (scenario, seed), rows in groups.items():
        rows.sort(key=lambda row: row["repeat"])
        first = rows[0]
        for other in rows[1:]:
            usable = first["status"] != "error" and other["status"] != "error"
            observed = first.get("inputs") is not None and other.get("inputs") is not None
            results.append({
                "scenario_id": scenario, "seed": seed, "repeat": other["repeat"],
                "consistent": (first["status"] == other["status"]
                               and first["final_state"] == other["final_state"]) if usable else None,
                "answer_changed": first.get("answer") != other.get("answer"),
                "tools_changed": (first.get("tool_calls") != other.get("tool_calls")) if observed else None,
                "inputs_changed": (first["inputs"] != other["inputs"]) if observed else None,
                "note": "Interface inputs include time, command durations, and live PIDs; no inference-level cause is assumed."
                        if observed else "Legacy mode cannot observe engine inputs/tool calls completely.",
            })
    return results


def aggregate(report: dict) -> list[dict]:
    rows = []
    for scenario in report["metadata"]["scenarios"]:
        trials = [r for r in report["trials"] if r["scenario_id"] == scenario["id"]]
        expected = len(report["metadata"]["seeds"]) * report["metadata"]["repeat"]
        row = {
            "scenario_id": scenario["id"], "planned": expected,
            "pass": sum(t["status"] == "pass" for t in trials),
            "fail": sum(t["status"] == "fail" for t in trials),
            "error": sum(t["status"] == "error" for t in trials),
            "missing": expected - len(trials),
        }
        for metric in METRICS:
            values = [r["metrics"][metric] for r in trials if r["metrics"].get(metric) is not None]
            row[metric] = ((statistics.mean(values) if metric in ("steps", "confirmations")
                           else max(values) if metric == "peak_rss_mib"
                           else statistics.median(values)) if values else None)
            row[metric + "_samples"] = len(values)
        rows.append(row)
    return rows


def fmt(value, digits=2):
    return "N/A" if value is None else f"{value:.{digits}f}"


def markdown(report: dict) -> str:
    meta = report["metadata"]
    build = meta["build"]
    rows = [
        "# nosh real-model evaluation",
        "",
        f"Run: `{meta['run_id']}`. Observation: `{meta['observation']}`. "
        f"Source: `{build.get('source_revision') or 'unverified'}`. "
        f"Binary SHA-256: `{build['binary_sha256']}`.",
        "",
        f"Harness source: `{meta.get('harness_revision') or meta.get('harness_content_sha256') or meta.get('harness_sha256', 'unverified')}`.",
        "",
        f"Seeds: `{meta['seeds']}`; repeats: {meta['repeat']}. "
        "Each scenario/seed starts a new process. The typo scenario does not load a model.",
        "",
        "| Scenario | Passed/planned | Failed / error / missing | Steps (mean) | Confirmations (mean) | TTFT (median s) | Process time (median s) | Peak RSS (max MiB) |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    for row in aggregate(report):
        pct = 100 * row["pass"] / row["planned"]
        rows.append(
            f"| {row['scenario_id']} | {row['pass']}/{row['planned']} ({pct:.0f}%) | "
            f"{row['fail']} / {row['error']} / {row['missing']} | "
            + " | ".join(fmt(row[name]) for name in METRICS) + " |"
        )
    rows.extend([
        "",
        "TTFT is the engine's first-step time to its first sampled token, excluding model loading. "
        "Process time includes loading, terminal interactions, and shutdown, but excludes fixture setup. "
        "A fresh process has a cold conversation/KV cache, not necessarily a cold OS page cache. "
        "RSS is Linux wait4 ru_maxrss for each nosh process (including the kernel's accounting of waited-for descendants, "
        "not a sum of a process tree); the listener/verifier are separate. "
        "Timing aggregates include failed executions with available measurements. JSON contains sample counts and all raw values.",
        "",
        "The suite rebuilds the MVP task intents, not the unavailable original fixtures. "
        "These objective pass rates are not directly comparable to the old manual correctness grades or warm-session timings.",
    ])
    if meta["observation"] == "legacy":
        rows.extend(["", "**Provisional original-main measurement, not the complete post-merge baseline.** "
                     "Legacy -s has no observable true TTFT. Legacy engine inputs and exact tool traces are unavailable; "
                     "N/A is not zero. A successful -s has one step by its CLI contract."])
    if meta.get("regrade"):
        rows.extend(["", "## Grading provenance",
                     meta["regrade"]["note"],
                     f"Grader: `{meta['regrade']['revision']}`. "
                     f"Changed verdicts: {meta['regrade']['changed_verdicts']}. "
                     "Original verdicts/reasons remain in each JSON trial; model outputs, seeds, timings and captured states were not replaced."])
    comparison = report.get("comparison")
    if comparison:
        rows.extend(["", "## Previous run",
                     f"Compared with `{comparison['previous_run']}`: {comparison['paired_trials']} paired trials, "
                     f"{len(comparison['regressions'])} pass-to-nonpass changes, "
                     f"{len(comparison['improvements'])} nonpass-to-pass changes. "
                     f"Unpaired current/previous: {comparison['unpaired_current']}/{comparison['unpaired_previous']}."])
        rows.extend(f"- {warning}" for warning in comparison["warnings"])
        rows.extend(["", "| Scenario / seed / repeat | Before → after | Δ steps | Δ confirmations | Δ TTFT s | Δ time s | Δ RSS MiB |",
                     "|---|---|---:|---:|---:|---:|---:|"])
        for pair in comparison["pairs"]:
            rows.append(f"| {pair['scenario_id']} / {pair['seed']} / {pair['repeat']} | "
                        f"{pair['before']} → {pair['after']} | "
                        + " | ".join(fmt(pair["metric_delta"][m]) for m in METRICS) + " |")
    replay = report.get("reproducibility", [])
    rows.extend(["", "## Repeatability"])
    if replay:
        rows.append(f"{sum(r['consistent'] is True for r in replay)}/{len(replay)} pairs have the same verdict and final state; "
                    f"{sum(r['consistent'] is None for r in replay)} pairs lack usable execution evidence.")
        rows.extend(["", "| Scenario / seed / repeat | Verdict + state consistent | Answer changed | Tools changed | Inputs changed |",
                     "|---|---|---|---|---|"])
        for pair in replay:
            values = [pair.get(k) for k in ("consistent", "answer_changed", "tools_changed", "inputs_changed")]
            rows.append(f"| {pair['scenario_id']} / {pair['seed']} / {pair['repeat']} | "
                        + " | ".join("N/A" if v is None else str(v).lower() for v in values) + " |")
    else:
        rows.append("Not measured: this run has no repeated scenario/seed pairs. Use --repeat 2; do not infer repeatability from fixed seeds alone.")
    rows.extend(["", "Dynamic prompt time, tool/recent-command durations, and live PIDs can change model inputs. "
                 "Answer/trace differences are not automatically inference nondeterminism. Interface inputs do not include "
                 "all internally retained assistant tokens: unchanged interface records are not proof of identical token prompts. "
                 "See per-trial interface inputs where available.",
                 "", "## Per-trial answers and evidence",
                 "", "Display copies below omit line-end whitespace; JSON retains the recorded answer text."])
    for trial in report["trials"]:
        rows.extend(["", f"### {trial['scenario_id']} / seed {trial['seed']} / repeat {trial['repeat']}: {trial['status']}", ""])
        rows.extend(f"- {reason}" for reason in trial.get("reasons", []))
        answer = trial.get("answer", "")
        displayed = "\n".join(line.rstrip() for line in answer.splitlines())
        fence = "`" * max(3, 1 + max((len(s) for s in re.findall(r"`+", answer)), default=0))
        rows.extend([f"{fence}text", displayed or "(no answer)", fence])
        if trial.get("metric_notes"):
            rows.append("Measurement notes: " + "; ".join(trial["metric_notes"]))
    if report.get("error"):
        rows.extend(["", "## Incomplete run", str(report["error"])])
    return "\n".join(rows) + "\n"


def save(report: dict, directory: Path, previous: dict | None = None) -> None:
    report["summary"] = aggregate(report)
    report["reproducibility"] = repetitions(report["trials"])
    if previous is not None:
        report["comparison"] = compare(report, previous)
    validate(report)
    for name, text in (
        ("report.json", json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n"),
        ("report.md", markdown(report)),
    ):
        path = directory / name
        stream = tempfile.NamedTemporaryFile(mode="w", encoding="utf-8", dir=directory,
                                              prefix=f".{name}.", suffix=".tmp", delete=False)
        temporary = Path(stream.name)
        try:
            with stream:
                stream.write(text)
                stream.flush()
                os.fsync(stream.fileno())
            temporary.replace(path)
        finally:
            temporary.unlink(missing_ok=True)
