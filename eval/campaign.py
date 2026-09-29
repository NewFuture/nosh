"""Real-model evaluations: python3 -m eval --suite smoke --model-path MODEL_DIR."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

from . import fixtures, report, runtime
from .suite import BUILTIN_SUITES, load_suite, positive_seconds, seeds
from .trial import run_trial


def validate_budget(suite: dict, repeat: int, timeout_s: float, available_s: float | None = None) -> float:
    if type(repeat) is not int or repeat < 1:
        raise ValueError("repeat must be a positive integer")
    positive_seconds(timeout_s, "trial timeout")
    if available_s is not None:
        positive_seconds(available_s, "campaign budget")
    trials = len(suite["scenarios"]) * len(seeds(suite["seeds"])) * repeat
    if not trials:
        raise ValueError("campaign must contain trials")
    try:
        worst_case = trials * timeout_s
    except OverflowError as exc:
        raise ValueError("campaign trial deadlines exceed the finite range") from exc
    positive_seconds(worst_case, "campaign trial deadlines")
    if available_s is not None and worst_case > available_s:
        raise ValueError(
            f"{trials} trial deadlines require {worst_case:g}s, exceeding the {available_s:g}s "
            "campaign budget; use a shorter explicit timeout or run locally"
        )
    return worst_case


def is_complete(data: dict, declared: dict, repeat: int = 2) -> bool:
    report.validate(data)
    meta = data["metadata"]
    if type(repeat) is not int or repeat < 1:
        raise ValueError("repeat must be a positive integer")
    # Validation already proves that every recorded identity is unique and in-plan.
    expected = len(declared["scenarios"]) * len(declared["seeds"]) * repeat
    return (meta["scenarios"] == declared["scenarios"]
            and meta["seeds"] == declared["seeds"] and meta["repeat"] == repeat
            and meta["dataset_revision"] == declared["dataset_revision"]
            and len(data["trials"]) == expected
            and not any(trial["status"] == "error" for trial in data["trials"])
            and not data.get("error"))


def arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=runtime.ROOT / "target" / "release" / "nosh")
    parser.add_argument("--model-path", type=Path, help="GGUF or model directory; required unless --plan")
    parser.add_argument("--suite", default="regression",
                        help=f"built-in name ({', '.join(BUILTIN_SUITES)}) or explicit JSON path")
    parser.add_argument("--scenario", action="append", help="select a scenario (repeatable)")
    parser.add_argument("--seeds", nargs="+", type=int)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--device", type=runtime.inference_device, default="cpu",
                        help="cpu (default), cuda or cuda:N; written into every isolated trial config")
    parser.add_argument("--timeout", type=float)
    parser.add_argument("--plan", action="store_true", help="print the validated trial plan without running tools or models")
    parser.add_argument("--budget", type=float, help="maximum total trial-deadline budget, in seconds")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--build-info", type=Path)
    parser.add_argument("--label")
    return parser.parse_args(argv)


def preserve_partial_report(data: dict, output: Path, previous: dict | None) -> None:
    try:
        report.save(data, output, previous)
    except (OSError, ValueError) as write_error:
        print(f"eval: could not preserve partial report: {write_error}", file=sys.stderr)


def main(argv=None) -> int:
    args = arguments(argv)
    if sys.platform != "linux" or sys.version_info < (3, 11):
        print("eval: Linux/WSL and Python >= 3.11 are required", file=sys.stderr)
        return 2
    data = output = previous = None
    try:
        suite = load_suite(args.suite)
        if args.threads < 1:
            raise ValueError("threads must be positive")
        if args.label is not None and not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.label):
            raise ValueError("label must be a plain name, not a path")
        if args.scenario:
            chosen = set(args.scenario)
            unknown = chosen - {s["id"] for s in suite["scenarios"]}
            if unknown:
                raise ValueError(f"unknown scenarios: {sorted(unknown)}")
            suite = dict(suite, scenarios=[s for s in suite["scenarios"] if s["id"] in chosen])
        selected_seeds = args.seeds if args.seeds is not None else suite["seeds"]
        timeout = args.timeout if args.timeout is not None else suite["timeout_s"]
        maximum = validate_budget(dict(suite, seeds=selected_seeds), args.repeat, timeout, args.budget)
        planned = len(suite["scenarios"]) * len(selected_seeds) * args.repeat
        if args.plan:
            print(json.dumps({
                "dataset_revision": suite["dataset_revision"],
                "scenarios": [scenario["id"] for scenario in suite["scenarios"]],
                "seeds": selected_seeds, "repeat": args.repeat, "trials": planned,
                "timeout_s": timeout, "maximum_trial_seconds": maximum,
                "device": args.device,
            }, ensure_ascii=False, indent=2, allow_nan=False))
            return 0
        if args.model_path is None:
            raise ValueError("--model-path is required unless --plan is selected")
        pidfd = os.pidfd_open(os.getpid())
        os.close(pidfd)
        toolchain = runtime.discover_tools(suite["scenarios"])
        binary = args.binary.expanduser().resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("nosh binary is not executable; build it first with cargo build --release --locked")
        weights, tokenizer = runtime.model_files(args.model_path)
        previous = json.loads(args.compare.read_text(encoding="utf-8")) if args.compare else None
        if previous is not None:
            report.validate(previous)
        meta = runtime.metadata(args, suite, binary, weights, tokenizer, toolchain)
        output = (args.output or runtime.HERE / "results" / meta["run_id"]).absolute()
        work = (args.work_dir or Path(tempfile.gettempdir()) / f"nosh-eval-{os.getuid()}").absolute()
        for resource in (binary, weights, tokenizer, output, *(Path(t["path"]) for t in toolchain.values())):
            if resource.resolve().is_relative_to(work.resolve()):
                raise ValueError("binary, models and reports must be outside the disposable workspace")
        runtime.validate_workspace_ancestry(work)
        meta["settings"]["work_dir"] = str(work)
        output.mkdir(mode=0o700, parents=True, exist_ok=False)
        data = {"schema_version": report.SCHEMA_VERSION, "metadata": meta, "trials": []}
        with fixtures.Workspace(work, toolchain) as workspace:
            report.save(data, output, previous)
            for repeat in range(args.repeat):
                for scenario in suite["scenarios"]:
                    for seed in meta["seeds"]:
                        print(f"{scenario['id']} seed={seed} repeat={repeat}", flush=True)
                        row = run_trial(args, meta, scenario, seed, repeat, workspace, output, binary, weights)
                        data["trials"].append(row)
                        report.save(data, output, previous)
                        print(f"  {row['status']}: {'; '.join(row['reasons'])}", flush=True)
        print(f"Report: {output / 'report.md'}", flush=True)
        if any(t["status"] == "error" for t in data["trials"]):
            return 2
        if any(t["status"] == "fail" for t in data["trials"]) or any(
            p["consistent"] is not True for p in data["reproducibility"]
        ):
            return 1
        return 0
    except KeyboardInterrupt:
        if data is not None:
            data["error"] = "Interrupted; uncompleted trials remain in the planned denominator."
            preserve_partial_report(data, output, previous)
        print("eval: interrupted", file=sys.stderr)
        return 130
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"eval: {exc}", file=sys.stderr)
        if data is not None:
            data["error"] = str(exc)
            preserve_partial_report(data, output, previous)
        return 2
