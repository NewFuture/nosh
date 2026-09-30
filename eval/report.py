"""Versioned results, paired comparisons, and a self-contained human report."""

from __future__ import annotations

from collections import defaultdict
import json
import os
from pathlib import Path
import re
import statistics
import tempfile

from .contracts import CAPTURE_CHECKS, CAPTURE_PARTS, finite_number

SCHEMA_VERSION = 2
METRICS = ("steps", "confirmations", "ttft_s", "total_s", "peak_rss_mib")
TOKEN_METRICS = ("prompt_tokens", "cached_tokens", "completion_tokens")


def validate(report: dict) -> None:
    if (not isinstance(report, dict) or type(report.get("schema_version")) is not int
            or report["schema_version"] != SCHEMA_VERSION):
        raise ValueError("unsupported report schema")
    if not isinstance(report.get("metadata"), dict) or not isinstance(report.get("trials"), list):
        raise ValueError("report must contain metadata and trials")
    revision = report["metadata"].get("dataset_revision")
    if type(revision) is not int or revision < 1:
        raise ValueError("dataset_revision must be a positive integer")
    seen = set()
    meta = report["metadata"]
    mode = meta.get("settings", {}).get("execution_mode", "cold")
    if mode not in ("cold", "resident"):
        raise ValueError("invalid model execution mode")
    if meta.get("observation") != "native-v1":
        raise ValueError("report requires native-v1 observations")
    if (not isinstance(meta.get("scenarios"), list) or not meta["scenarios"]
            or not isinstance(meta.get("seeds"), list) or not meta["seeds"]
            or type(meta.get("repeat")) is not int or meta["repeat"] < 1):
        raise ValueError("v2 report is missing the trial plan")
    if any(not isinstance(s, dict) or not isinstance(s.get("id"), str)
           or not re.fullmatch(r"[a-z][a-z0-9-]*", s["id"]) for s in meta["scenarios"]):
        raise ValueError("invalid scenario in trial plan")
    if (any(type(seed) is not int or not 0 <= seed < 2**64 for seed in meta["seeds"])
            or len(set(meta["seeds"])) != len(meta["seeds"])
            or len({s["id"] for s in meta["scenarios"]}) != len(meta["scenarios"])):
        raise ValueError("duplicate/invalid planned scenario or seed")
    scenarios = {s["id"]: s for s in meta["scenarios"]}
    planned_seeds = set(meta["seeds"])
    for trial in report["trials"]:
        key = trial_key(trial)
        if key in seen:
            raise ValueError(f"duplicate trial: {key}")
        seen.add(key)
        if key[0] not in scenarios or key[1] not in planned_seeds or key[2] >= meta["repeat"]:
            raise ValueError(f"trial was not in the declared plan: {key}")
        if trial.get("status") not in ("pass", "fail", "error"):
            raise ValueError(f"invalid trial status: {key}")
        engines = trial.get("engines")
        if engines is not None and (not isinstance(engines, list) or any(
                not isinstance(engine, dict)
                or ("device" in engine and (not isinstance(engine["device"], str)
                    or not re.fullmatch(r"cpu|cuda:(?:0|[1-9][0-9]*)", engine["device"])))
                for engine in engines)):
            raise ValueError(f"invalid observed devices: {key}")
        metrics = trial.get("metrics")
        if not isinstance(metrics, dict):
            raise ValueError(f"missing metrics: {key}")
        if any(name not in metrics for name in METRICS):
            raise ValueError(f"missing metric fields: {key}")
        if not isinstance(trial.get("answer"), str) or "final_state" not in trial:
            raise ValueError(f"missing answer or final-state evidence: {key}")
        state = trial["final_state"]
        if (state is not None and not isinstance(state, dict)) or (trial["status"] == "pass" and state is None):
            raise ValueError(f"invalid or missing final-state evidence: {key}")
        for name in METRICS:
            value = metrics.get(name)
            if value is not None and (not finite_number(value) or value < 0):
                raise ValueError(f"invalid {name} for {key}")
        if any(metrics[k] is not None and type(metrics[k]) is not int for k in ("steps", "confirmations")):
            raise ValueError(f"step/confirmation counts must be integers: {key}")
        for name in ("load_s", "prefill_s", "decode_s", "case_other_s"):
            if metrics.get(name) is not None and (not finite_number(metrics[name]) or metrics[name] < 0):
                raise ValueError(f"invalid {name} for {key}")
        if engines:
            for engine in engines:
                if engine.get("execution_mode", "cold") != mode:
                    raise ValueError(f"mixed cold/resident engine observations: {key}")
                if mode == "resident" and (engine.get("load_s") != 0
                        or engine.get("worker_pid") != meta.get("worker", {}).get("pid")):
                    raise ValueError(f"invalid resident worker identity/cost: {key}")
        grading = trial.get("grading")
        if grading is not None:
            if (not isinstance(grading, dict) or set(grading) != {"facts", "experience"}
                    or not isinstance(grading["facts"], dict)
                    or type(grading["facts"].get("passed")) is not bool
                    or not isinstance(grading["facts"].get("reasons"), list)
                    or any(not isinstance(reason, str) for reason in grading["facts"]["reasons"])
                    or (grading["facts"]["passed"] and grading["facts"]["reasons"])):
                raise ValueError(f"invalid fact grading: {key}")
            check = scenarios[key[0]].get("check")
            components = grading["facts"].get("components")
            if components is not None:
                if check not in CAPTURE_CHECKS or not isinstance(components, dict) or set(components) != set(CAPTURE_PARTS):
                    raise ValueError(f"invalid capture components: {key}")
                for name, item in components.items():
                    required = name != "citation" or CAPTURE_CHECKS[check]
                    if (not isinstance(item, dict) or item.get("required") is not required
                            or not isinstance(item.get("reasons"), list)
                            or any(not isinstance(reason, str) for reason in item["reasons"])
                            or (required and type(item.get("passed")) is not bool)
                            or (not required and item.get("passed") is not None)
                            or (item.get("passed") is True and item["reasons"])):
                        raise ValueError(f"invalid {name} component: {key}")
                if grading["facts"]["passed"] and any(item["passed"] is False for item in components.values()):
                    raise ValueError(f"passing facts have failed capture components: {key}")
            elif check in CAPTURE_CHECKS and (trial["status"] == "pass" or grading["facts"]["passed"]):
                raise ValueError(f"passing capture facts have no component evidence: {key}")
            ux = grading["experience"]
            if ux is not None and (not isinstance(ux, dict) or set(ux) != {
                "steps", "confirmations", "final_question", "response_language",
            } or any(not isinstance(item, dict) or "passed" not in item or (item["passed"] is not None
                     and type(item["passed"]) is not bool) for item in ux.values())):
                raise ValueError(f"invalid experience grading: {key}")
            if ux is not None:
                limits = scenarios[key[0]].get("expect")
                if not isinstance(limits, dict):
                    raise ValueError(f"experience results have no declared expectations: {key}")
                for name in ("steps", "confirmations"):
                    item = ux[name]
                    if (type(item["passed"]) is not bool or item.get("actual") != metrics[name]
                            or type(item.get("maximum")) is not int or item["maximum"] < 0
                            or type(item.get("actual")) is not int
                            or item["maximum"] != limits.get("max_" + name)
                            or item["passed"] != (item["actual"] <= item["maximum"])):
                        raise ValueError(f"invalid {name} budget evidence: {key}")
                for name in ("response_language", "final_question"):
                    item = ux[name]
                    expected = limits.get(name)
                    applicable = expected == "zh" if name == "response_language" else expected in ("require", "forbid")
                    if (item.get("expected") != expected or (applicable and type(item["passed"]) is not bool)
                            or (not applicable and item["passed"] is not None)):
                        raise ValueError(f"missing or invalid {name} evidence: {key}")
        if trial["status"] == "pass" and (grading is None or not grading["facts"]["passed"]
                or metrics["steps"] is None or metrics["confirmations"] is None
                or ("expect" in scenarios[key[0]] and grading["experience"] is None)
                or any(item["passed"] is False for item in (grading["experience"] or {}).values())):
            raise ValueError(f"passing trial has missing or failed grading: {key}")


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
    if a["dataset_revision"] != b["dataset_revision"]:
        warnings.append("dataset_revision differs; paired deltas are descriptive, not a controlled regression")
    if a.get("settings", {}).get("execution_mode", "cold") != b.get("settings", {}).get("execution_mode", "cold"):
        warnings.append("cold/resident model lifecycles differ; per-trial latency and RSS are not the same measurement scope")
    for key in ("suite_sha256", "suite_schema_version", "grading_content_sha256", "model",
                "settings", "machine", "tools", "toolchain", "observation"):
        if a.get(key) != b.get(key):
            warnings.append(f"{key} differs; paired deltas are descriptive, not a controlled regression")
    if a.get("harness_content_sha256") != b.get("harness_content_sha256"):
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
    def observed_devices(trial, metadata):
        engines = trial.get("engines")
        if engines is not None:
            return sorted({engine.get("device", "cpu") for engine in engines})
        if trial["metrics"].get("task_status") == "local":
            return []
        # Native-v1 reports without device settings/observations were CPU-only.
        if metadata.get("settings", {}).get("device", "cpu") == "cpu":
            return ["cpu"]
        return None
    if any(observed_devices(trial, a) != observed_devices(old[trial_key(trial)], b)
           for trial in current["trials"] if trial_key(trial) in old):
        warnings.append("observed devices differ; auto-selected backends may not be a controlled regression")
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
            usable = (first["status"] != "error" and other["status"] != "error"
                      and isinstance(first.get("final_state"), dict) and isinstance(other.get("final_state"), dict))
            inputs_observed = first.get("inputs") is not None and other.get("inputs") is not None
            tools_observed = first.get("tool_calls") is not None and other.get("tool_calls") is not None
            results.append({
                "scenario_id": scenario, "seed": seed, "repeat": other["repeat"],
                "consistent": (first["status"] == other["status"]
                               and first["final_state"] == other["final_state"]) if usable else None,
                "answer_changed": first.get("answer") != other.get("answer"),
                "tools_changed": (first["tool_calls"] != other["tool_calls"]) if tools_observed else None,
                "inputs_changed": (first["inputs"] != other["inputs"]) if inputs_observed else None,
                "note": "Interface inputs include time, command durations, and live PIDs; no inference-level cause is assumed."
                        if inputs_observed and tools_observed else "Model input/tool evidence was not recorded for this pair.",
            })
    return results


def summarize(trials: list[dict], planned: int) -> dict:
    row = {
        "planned": planned, "pass": sum(t["status"] == "pass" for t in trials),
        "fail": sum(t["status"] == "fail" for t in trials),
        "error": sum(t["status"] == "error" for t in trials), "missing": planned - len(trials),
    }
    for metric in METRICS:
        values = [r["metrics"][metric] for r in trials if r["metrics"].get(metric) is not None]
        row[metric] = ((statistics.mean(values) if metric in ("steps", "confirmations")
                       else max(values) if metric == "peak_rss_mib"
                       else statistics.median(values)) if values else None)
        row[metric + "_samples"] = len(values)
    for metric in TOKEN_METRICS:
        values = [r["metrics"][metric] for r in trials if r["metrics"].get(metric) is not None]
        row[metric] = statistics.mean(values) if values else None
        row[metric + "_samples"] = len(values)
    return row


def grading_counts(trials: list[dict]) -> dict:
    graded = [t["grading"] for t in trials if t.get("grading") is not None]
    ux = [g["experience"] for g in graded if g["experience"] is not None]
    return {
        "facts_pass": sum(g["facts"]["passed"] for g in graded), "facts_samples": len(graded),
        "experience_pass": sum(all(d["passed"] is not False for d in item.values()) for item in ux),
        "experience_samples": len(ux),
    }


def capture_components(report: dict) -> list[dict]:
    rows = []
    for scenario in report["metadata"]["scenarios"]:
        if scenario.get("check") not in CAPTURE_CHECKS:
            continue
        planned = len(report["metadata"]["seeds"]) * report["metadata"]["repeat"]
        trials = [trial for trial in report["trials"] if trial["scenario_id"] == scenario["id"]]
        for name in CAPTURE_PARTS:
            row = {"scenario_id": scenario["id"], "component": name,
                   "required": name != "citation" or CAPTURE_CHECKS[scenario["check"]],
                   "planned": planned, "pass": 0, "fail": 0, "not_applicable": 0, "unobserved": planned}
            for trial in trials:
                item = (((trial.get("grading") or {}).get("facts") or {}).get("components") or {}).get(name)
                if item is None:
                    continue
                row["unobserved"] -= 1
                row["not_applicable" if item["passed"] is None else "pass" if item["passed"] else "fail"] += 1
            row["measured"] = row["pass"] + row["fail"]
            rows.append(row)
    return rows


def aggregate(report: dict) -> list[dict]:
    rows = []
    for scenario in report["metadata"]["scenarios"]:
        trials = [r for r in report["trials"] if r["scenario_id"] == scenario["id"]]
        expected = len(report["metadata"]["seeds"]) * report["metadata"]["repeat"]
        row = {"scenario_id": scenario["id"], **summarize(trials, expected)}
        row.update(grading_counts(trials))
        rows.append(row)
    return rows


def groups(report: dict) -> list[dict]:
    meta = report["metadata"]
    rows = []
    for name in ("all", "mvp", "expanded", "command-assist", "generate", "fix", "next", "model", "local"):
        scenarios = [
            s for s in meta["scenarios"]
            if name == "all" or name == s.get("group")
            or name == s.get("assistance", {}).get("intent")
            or (name == "model" and s.get("check") != "typos")
            or (name == "local" and s.get("check") == "typos")
        ]
        if not scenarios:
            continue
        ids = {s["id"] for s in scenarios}
        trials = [t for t in report["trials"] if t["scenario_id"] in ids]
        expected = len(ids) * len(meta["seeds"]) * meta["repeat"]
        rows.append({"group": name, **summarize(trials, expected), **grading_counts(trials)})
    return rows


def fmt(value, digits=2):
    return "N/A" if value is None else f"{value:.{digits}f}"


def summary_tables(report: dict) -> dict:
    tables = {"summary": aggregate(report)}
    tables["groups"] = groups(report)
    if components := capture_components(report):
        tables["capture_components"] = components
    return tables


def markdown(report: dict) -> str:
    validate(report)
    return _markdown(report, summary_tables(report))


def _markdown(report: dict, tables: dict) -> str:
    meta = report["metadata"]
    build = meta["build"]
    mode = meta.get("settings", {}).get("execution_mode", "cold")
    rows = [
        "# nosh real-model evaluation",
        "",
        f"Run: `{meta['run_id']}`. Observation: `{meta['observation']}`. "
        f"Source: `{build.get('source_revision') or 'unverified'}`. "
        f"Binary SHA-256: `{build['binary_sha256']}`.",
        "",
        f"Harness source: `{meta.get('harness_revision') or meta.get('harness_content_sha256') or 'unverified'}`.",
        "",
        f"Inference device: `{meta.get('settings', {}).get('device', 'cpu')}`. "
        "Peak RSS measures host memory only, not GPU memory.",
        "",
        f"Seeds: `{meta['seeds']}`; repeats: {meta['repeat']}. "
        + f"Dataset revision: {meta['dataset_revision']}. "
        + f"Execution mode: **{mode}**. Each trial starts an isolated CLI/shell. "
        + ("One resident model/prefix cache is shared; conversations are not. " if mode == "resident"
           else "Each model trial loads its own engine with an empty prefix cache. ")
        + "The typo scenario does not use a model.",
        "",
        "| Scenario | Passed/planned | Failed / error / missing | Steps (mean) | Confirmations (mean) | TTFT (median s) | Process time (median s) | Peak RSS (max MiB) |",
        "|---|---:|---:|---:|---:|---:|---:|---:|",
    ]
    overview = ["## Overall and groups", "",
                "| Group | Passed/planned | Pass rate | Failed / error / missing | Steps mean (samples) | Confirmations mean (samples) |",
                "|---|---:|---:|---:|---:|---:|"]
    for row in tables["groups"]:
        overview.append(
            f"| {row['group']} | {row['pass']}/{row['planned']} | {100 * row['pass'] / row['planned']:.1f}% | "
            f"{row['fail']} / {row['error']} / {row['missing']} | "
            f"{fmt(row['steps'])} ({row['steps_samples']}) | {fmt(row['confirmations'])} ({row['confirmations_samples']}) |"
        )
    rows = rows[:-2] + overview + ["", "## Scenarios", "", *rows[-2:]]
    for row in tables["summary"]:
        pct = 100 * row["pass"] / row["planned"]
        rows.append(
            f"| {row['scenario_id']} | {row['pass']}/{row['planned']} ({pct:.0f}%) | "
            f"{row['fail']} / {row['error']} / {row['missing']} | "
            + " | ".join(fmt(row[name]) for name in METRICS) + " |"
        )
    if any(t.get("metrics", {}).get("prompt_tokens") is not None for t in report["trials"]):
        rows.extend(["", "## Model token cost", "",
                     "| Scenario | New prompt tokens | Reused tokens | Generated tokens |",
                     "|---|---:|---:|---:|"])
        for row in tables["summary"]:
            rows.append(f"| {row['scenario_id']} | " + " | ".join(fmt(row[name]) for name in TOKEN_METRICS) + " |")
        rows.append("\nMeans include failed trials with measured usage. New and reused tokens are separate; tool schemas and message templates are included.")
    worker = meta.get("worker")
    if worker:
        rows.extend(["", "## Resident worker lifecycle", "",
                     f"Status: `{worker['status']}`; startup to readiness: {fmt(worker.get('startup_s'))} s "
                     f"(includes model load {fmt(worker.get('info', {}).get('load_s'))} s); "
                     f"shutdown: {fmt(worker.get('shutdown_s'))} s. "
                     "These one-time costs are NOT included in individual CLI process times. "
                     "No warmup generation is discarded. The first real case warms the process.",
                     f"Execution wall time, including fixtures, grading and report writes: {fmt(meta.get('execution_wall_s'))} s. "
                     "Worker RSS snapshots are recorded separately in worker_after; CLI wait4 RSS excludes the worker. "
                     "Neither metric measures VRAM."])
    if any(t["metrics"].get("prefill_s") is not None for t in report["trials"]):
        rows.extend(["", "## Per-trial engine timing", "",
                     "| Trial / seed / repeat | Model load s | Prefill s | Decode s | Other case wall s | First-step new / cached tokens |",
                     "|---|---:|---:|---:|---:|---:|"])
        for trial in report["trials"]:
            m = trial["metrics"]
            rows.append(f"| {trial['scenario_id']} / {trial['seed']} / {trial['repeat']} | "
                        + " | ".join(fmt(m.get(key)) for key in ("load_s", "prefill_s", "decode_s", "case_other_s"))
                        + f" | {fmt(m.get('first_step_prompt_tokens'))} / {fmt(m.get('first_step_cached_tokens'))} |")
        rows.append("\nPrefill/decode are native wall-clock regions, not GPU busy time. Other case wall time is "
                    "CLI total minus native load/prefill/decode: tokenization, IPC, tools, approval/input waits and "
                    "shell lifecycle remain included. For an unfinished step the residual is unknown, not zero; "
                    "prefill/decode totals then cover completed steps only. Exact-token prefix hits are measured, not assumed.")
    rows.extend(["", "## Declared experience budgets", "",
                 "| Scenario | Max steps | Max confirmations | Language | Final question | Facts passed / measured | Experience passed / measured |",
                 "|---|---:|---:|---|---|---:|---:|"])
    summaries = {r["scenario_id"]: r for r in tables["summary"]}
    for scenario in meta["scenarios"]:
        limits = scenario.get("expect", {})
        row = summaries[scenario["id"]]
        rows.append(
            f"| {scenario['id']} | {limits.get('max_steps', 'N/A')} | {limits.get('max_confirmations', 'N/A')} | "
            f"{limits.get('response_language', 'N/A')} | {limits.get('final_question', 'N/A')} | "
            f"{row['facts_pass']}/{row['facts_samples']} | {row['experience_pass']}/{row['experience_samples']} |"
        )
    rows.extend(["", "A trial passes only when its facts/state and every applicable experience check pass. "
                 "Missing observations are not zero or a pass. Group means use individual measured trials, including failures; "
                 "the local spelling-correction cases are separate from model tasks. "
                 "Language and closing-question checks are deterministic heuristics, not a model judge."])
    components = tables.get("capture_components", [])
    if components:
        rows.extend(["", "## Captured output: separate verdicts", "",
                     "| Scenario | Component | Required | Passed / measured | Failed | N/A | Unobserved / planned |",
                     "|---|---|---|---:|---:|---:|---:|"])
        for item in components:
            rows.append(
                f"| {item['scenario_id']} | {item['component']} | {'yes' if item['required'] else 'no'} | "
                f"{item['pass']}/{item['measured']} | {item['fail']} | {item['not_applicable']} | "
                f"{item['unobserved']}/{item['planned']} |"
            )
        rows.extend(["", "Capture measures original evidence, attribution and single execution. Diagnosis checks the "
                     "missing setting, remedy and explicitly covered unsupported assertions; it is not a general semantic truth proof. "
                     "Citation is required only in the dedicated diagnostic_id task. Missing evidence is unobserved, not a pass; "
                     "a citation failure must not be reported as a capture failure."])
    rows.extend([
        "",
        "TTFT is the engine's first-step time to its first sampled token, excluding model loading. "
        "CLI process time includes terminal interactions and shutdown, but excludes fixture setup. "
        "Cold mode includes per-trial model loading; resident mode reports worker startup/loading separately. "
        "A cold model process has an empty KV cache, not necessarily a cold OS page cache. "
        "RSS is Linux wait4 ru_maxrss for each nosh process (including the kernel's accounting of waited-for descendants, "
        "not a sum of a process tree); the listener/verifier are separate. "
        "Timing aggregates include failed executions with available measurements. JSON contains sample counts and all raw values.",
        "",
        ("The expanded suite preserves the ten MVP tasks and adds real-project tasks and experience gates. "
         "Changed datasets and grading rules are not controlled before/after comparisons with historical baselines."),
    ])
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
        grade = trial.get("grading")
        rows.append("Grading: " + (
            "not measured" if grade is None else
            f"facts={'pass' if grade['facts']['passed'] else 'fail'}; "
            + ("experience=not measured" if grade["experience"] is None else ", ".join(
                f"{name}={'N/A' if item['passed'] is None else 'pass' if item['passed'] else 'fail'}"
                for name, item in grade["experience"].items()
            ))
        ))
        components = ((grade or {}).get("facts") or {}).get("components")
        if components:
            rows.append("Components: " + "; ".join(
                f"{name}={'N/A' if item['passed'] is None else 'pass' if item['passed'] else 'fail'}"
                for name, item in components.items()
            ))
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
    validate(report)
    tables = summary_tables(report)
    report.pop("capture_components", None)
    report.update(tables)
    report["reproducibility"] = repetitions(report["trials"])
    if previous is not None:
        report["comparison"] = compare(report, previous)
    for name, text in (
        ("report.json", json.dumps(report, ensure_ascii=False, indent=2, allow_nan=False) + "\n"),
        ("report.md", _markdown(report, tables)),
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
