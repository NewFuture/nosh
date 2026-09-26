"""Usage (Linux/WSL, Python >= 3.11): python3 -m eval.run --model-path MODEL_DIR."""

from __future__ import annotations

import argparse
import contextlib
from datetime import datetime, timezone
import json
import math
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import sys
import tempfile

from . import approval, checks, driver, fixtures, report
from .observations import observe
from .suite import NATIVE_CHECKS, load_suite, seeds

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def required_tools(scenarios: list[dict]) -> set[str]:
    required = {"git", "bash", "python3", "tar", "ss"}
    if any(s["fixture"].startswith("rust") for s in scenarios):
        required |= {"cargo", "rustc", "cc"}
    if any(s["fixture"] == "node" for s in scenarios):
        required |= {"node", "npm"}
    if any(s["check"] == "versions" for s in scenarios):
        required |= {"cargo", "node"}
    return required


def discover_tools(scenarios: list[dict]) -> dict:
    tools = {}
    inherited_path = os.environ.get("PATH", "")
    env = dict(os.environ, LANG="C.UTF-8", LC_ALL="C.UTF-8", RUSTUP_AUTO_INSTALL="0")
    for name in sorted(required_tools(scenarios)):
        search = ("/usr/bin:/bin:" + inherited_path if name in {"git", "bash", "python3", "tar", "ss"}
                  else inherited_path + ":/usr/bin:/bin")
        executable = shutil.which(name, path=search)
        if not executable:
            raise ValueError(f"missing required executable: {name}; prepare the toolchain before evaluation")
        path = Path(executable).resolve()
        if name in ("cargo", "rustc") and path.name == "rustup":
            resolved = subprocess.check_output([str(path), "which", name], env=env, text=True, timeout=10).strip()
            path = Path(resolved).resolve(strict=True)
        proc = subprocess.run([str(path), "-V" if name == "ss" else "--version"],
                              env=env, check=True, capture_output=True, text=True, timeout=10)
        version = (proc.stdout or proc.stderr).strip()
        if not version:
            raise ValueError(f"{name} returned no version")
        if name == "node":
            major = re.match(r"v(\d+)\.", version)
            if not major or int(major[1]) < 22:
                raise ValueError("Node >= 22 is required for the dependency-free node:test fixture")
        tools[name] = {"path": str(path), "version": version.splitlines()[0], "sha256": fixtures.file_hash(path)}
    return tools


def environment(home: Path, threads: int, trace: Path | None, tools: dict | None = None) -> dict[str, str]:
    env = fixtures.project_environment(home, tools)
    env.update({
        "NOSH_HOME": str(home / "nosh"),
        "USER": "eval", "LOGNAME": "eval",
        "TERM": "xterm-256color", "NO_COLOR": "1", "NOSH_STATS": "1",
        "NOSH_OFFLINE": "1", "HF_HUB_OFFLINE": "1",
        "CANDLE_NUM_THREADS": str(threads), "RAYON_NUM_THREADS": "1",
    })
    config = home / "nosh"
    config.mkdir(mode=0o700)
    (config / "config.toml").write_text(
        '[agent]\napproval = "confirm"\nmax_steps = 10\ncommand_timeout_sec = 60\nrestore_cwd = false\n'
        '[model]\ncontext_length = 8192\nthinking = "off"\n[download]\nauto = "never"\n',
        encoding="utf-8",
    )
    if trace:
        env["NOSH_EVAL_TRACE"] = str(trace)
    return env


def model_files(path: Path) -> tuple[Path, Path]:
    path = path.expanduser().resolve(strict=True)
    if path.is_dir():
        weights = sorted(path.glob("*.gguf"))
        if len(weights) != 1:
            raise ValueError("model directory must contain exactly one GGUF; otherwise pass the desired file")
        path = weights[0]
    tokenizer = path.parent / "tokenizer.json"
    if path.suffix != ".gguf" or not path.is_file() or not tokenizer.is_file():
        raise ValueError("a GGUF and its adjacent tokenizer.json are required")
    return path, tokenizer


def machine_info() -> dict:
    model = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                  if line.startswith("model name")), platform.processor())
    os_release = Path("/etc/os-release").read_text()
    return {"os_release": os_release, "kernel": platform.release(), "arch": platform.machine(),
            "cpu": model, "logical_cpus": os.cpu_count()}


def metadata(args, suite: dict, binary: Path, weights: Path, tokenizer: Path, toolchain: dict) -> dict:
    binary_hash = fixtures.file_hash(binary)
    build = {"source_revision": None, "source_clean": None, "binary_sha256": binary_hash,
             "provenance": "unverified external binary"}
    if args.build_info:
        build = json.loads(args.build_info.read_text(encoding="utf-8"))
        if (build.get("schema_version") != 1 or build.get("binary_sha256") != binary_hash
                or not re.fullmatch(r"[0-9a-f]{40}", build.get("source_revision", ""))):
            raise ValueError("build info does not identify this exact binary and source revision")
        build["provenance"] = "recorded build; supplied binary hash verified"
    tools = {"python": platform.python_version(), **{name: info["version"] for name, info in toolchain.items()}}
    return {
        "run_id": args.label or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ"),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "observation": "legacy" if args.legacy else "native-v1",
        "build": build,
        "model": {"weights": weights.name, "weights_sha256": fixtures.file_hash(weights),
                  "tokenizer_sha256": fixtures.file_hash(tokenizer)},
        "suite_sha256": fixtures.digest(suite),
        "suite_schema_version": suite["schema_version"],
        "dataset_revision": suite.get("dataset_revision", 1),
        "harness_sha256": fixtures.digest({p.name: fixtures.file_hash(p) for p in sorted(HERE.glob("*.py"))}),
        "harness_content_sha256": fixtures.digest({p.name: fixtures.source_hash(p) for p in sorted(HERE.glob("*.py"))}),
        "grading_content_sha256": fixtures.source_hash(HERE / "checks.py"),
        "settings": {"threads": args.threads, "rayon_threads": 1, "context_length": 8192,
                     "max_steps": 10, "command_timeout_s": 60, "timeout_s": args.timeout or suite["timeout_s"],
                     "approval": "confirm", "locale": "C.UTF-8", "timezone": "UTC",
                     "path": "<trial-home>/bin:/usr/bin:/bin", "tty_size": [40, 160], "process_per_trial": True,
                     "process_niceness": os.getpriority(os.PRIO_PROCESS, 0),
                     "cargo_offline": True, "cargo_incremental": False, "cargo_jobs": 1, "npm_offline": True},
        "machine": machine_info(), "tools": tools, "toolchain": toolchain,
        "scenarios": suite["scenarios"], "seeds": args.seeds or suite["seeds"], "repeat": args.repeat,
    }


def run_trial(args, meta: dict, scenario: dict, seed: int, repeat: int,
              workspace: fixtures.Workspace, output: Path, binary: Path, weights: Path) -> dict:
    row = {
        "scenario_id": scenario["id"], "seed": seed, "repeat": repeat, "status": "error",
        "metrics": {key: None for key in report.METRICS},
        "answer": "", "reasons": [], "inputs": None, "tool_calls": None, "final_state": None,
        "grading": None, "executions": None,
    }
    result = None
    trace = None
    logs = output / "logs" / f"{scenario['id']}-{seed}-{repeat}"
    logs.mkdir(parents=True)
    try:
        root, home, facts = workspace.prepare(scenario)
        trace = home.parent / "engine.jsonl"
        env = environment(home, args.threads, None if args.legacy else trace, workspace.tools)
        if scenario["check"] in NATIVE_CHECKS:
            facts["tools"] = workspace.tools
        if scenario["check"] == "versions":
            facts["versions"] = {name: meta["tools"][name] for name in ("cargo", "node", "python3")}
        if scenario["check"] == "recent-history":
            facts["git_before"] = fixtures.git_state(root)
            facts["commit_ids"] = fixtures.git(root, "log", "--format=%H").splitlines()
        argv = [str(binary), "--offline", "--no-download", "--norc", "--seed", str(seed), "--model-path", str(weights)]
        timeout = meta["settings"]["timeout_s"]
        fixture = fixtures.listener(root) if scenario["fixture"] == "port" else contextlib.nullcontext(None)
        with fixture as port:
            if port:
                facts["listener"] = port
            if scenario["mode"] == "repl":
                result = driver.run_repl(
                    argv, root, env, timeout, scenario,
                    lambda command, card: approval.allow_approval(scenario["approval"], command, root, facts),
                )
            else:
                data = b""
                if scenario.get("stdin_command"):
                    data = subprocess.check_output(scenario["stdin_command"], cwd=root, env=env, timeout=5)
                flags = ["-a", "--json"] if scenario["mode"] == "agent" else ["-s"]
                result = driver.run_cli(argv + flags + [scenario["input"]], root, env, timeout, data)
        row.update(approvals=result.approvals, turns=result.turns, exit_code=result.exit_code, facts=facts)
        row["metrics"].update(total_s=result.total_s, peak_rss_mib=result.peak_rss_mib,
                              confirmations=len(result.approvals))
        after = fixtures.snapshot(root)
        row.update(fixture_sha256=fixtures.digest({k: v for k, v in facts.items() if k != "listener"}),
                   file_snapshot=after, final_state=checks.fixture_state(scenario, facts, root, after, result))
        if result.error:
            if not args.legacy and result.timeout_phase in ("agent", "cli") and trace.is_file():
                observed = observe(result, scenario, trace, False, seed, inflight_timeout=True)
                row.update(observed)
                reason = (f"agent exceeded the {timeout:g}s trial deadline during model generation "
                          f"({row['metrics']['steps']} started steps)")
                if scenario.get("expect") and row["metrics"]["steps"] > scenario["expect"]["max_steps"]:
                    reason += f"; step budget is {scenario['expect']['max_steps']}"
                row.update(status="fail", reasons=[reason], timeout_phase=result.timeout_phase,
                           grading={"facts": {"passed": False, "reasons": [reason]}, "experience": None})
                return row
            raise driver.DriverError(result.error)
        if result.failure:
            if not args.legacy and trace.is_file():
                observed = observe(result, scenario, trace, False, seed)
                row.update(observed)
                if observed["metrics"]["task_status"] is None:
                    raise driver.DriverError("an agent task ran but its completion marker was not observed")
            row.update(status="fail", reasons=[result.failure],
                       grading={"facts": {"passed": False, "reasons": [result.failure]}, "experience": None})
            return row
        observed = observe(result, scenario, trace, args.legacy, seed)
        row.update(observed)
        verdict = checks.judge(scenario, row["answer"], facts, root, after, result, row["metrics"], row)
        if not verdict.passed and any(not a["allowed"] for a in result.approvals):
            verdict.reasons.append("The declared approval policy denied at least one command; see approval evidence.")
        row.update(status="pass" if verdict.passed else "fail", reasons=verdict.reasons, grading=verdict.details)
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        row["reasons"].append(f"{type(exc).__name__}: {exc}")
    finally:
        if result:
            for name in ("stdout", "stderr", "transcript"):
                (logs / f"{name}.txt").write_text(getattr(result, name), encoding="utf-8")
        if trace and trace.is_file():
            shutil.copyfile(trace, logs / "engine.jsonl")
        row["logs"] = str(logs.relative_to(output))
        workspace.clean(scenario["id"])
    return row


def arguments(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target" / "release" / "nosh")
    parser.add_argument("--model-path", type=Path, required=True)
    parser.add_argument("--suite", type=Path, default=HERE / "scenarios.json")
    parser.add_argument("--scenario", action="append", help="select a scenario (repeatable)")
    parser.add_argument("--seeds", nargs="+", type=int)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("--timeout", type=float)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--compare", type=Path)
    parser.add_argument("--build-info", type=Path)
    parser.add_argument("--label")
    parser.add_argument("--legacy", action="store_true", help="explicitly allow older binaries with incomplete observations")
    return parser.parse_args(argv)


def main(argv=None) -> int:
    args = arguments(argv)
    if sys.platform != "linux" or sys.version_info < (3, 11):
        print("eval: Linux/WSL and Python >= 3.11 are required", file=sys.stderr)
        return 2
    data = output = previous = None
    try:
        pidfd = os.pidfd_open(os.getpid())
        os.close(pidfd)
        suite = load_suite(args.suite)
        if args.seeds is not None:
            seeds(args.seeds)
        if args.repeat < 1 or args.threads < 1 or (args.timeout is not None and (not math.isfinite(args.timeout) or args.timeout <= 0)):
            raise ValueError("repeat, threads and timeout must be positive")
        if args.label is not None and not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._-]*", args.label):
            raise ValueError("label must be a plain name, not a path")
        if args.scenario:
            chosen = set(args.scenario)
            unknown = chosen - {s["id"] for s in suite["scenarios"]}
            if unknown:
                raise ValueError(f"unknown scenarios: {sorted(unknown)}")
            suite = dict(suite, scenarios=[s for s in suite["scenarios"] if s["id"] in chosen])
        if args.legacy and any(s["check"] in NATIVE_CHECKS for s in suite["scenarios"]):
            raise ValueError("selected scenarios require native execution evidence; --legacy is not supported")
        toolchain = discover_tools(suite["scenarios"])
        binary = args.binary.expanduser().resolve(strict=True)
        if not binary.is_file() or not os.access(binary, os.X_OK):
            raise ValueError("nosh binary is not executable; build it first with cargo build --release --locked")
        weights, tokenizer = model_files(args.model_path)
        previous = json.loads(args.compare.read_text(encoding="utf-8")) if args.compare else None
        if previous is not None:
            report.validate(previous)
        meta = metadata(args, suite, binary, weights, tokenizer, toolchain)
        output = (args.output or HERE / "results" / meta["run_id"]).absolute()
        work = (args.work_dir or Path(tempfile.gettempdir()) / f"nosh-eval-{os.getuid()}").absolute()
        for resource in (binary, weights, tokenizer, output, *(Path(t["path"]) for t in toolchain.values())):
            if resource.resolve().is_relative_to(work.resolve()):
                raise ValueError("binary, models and reports must be outside the disposable workspace")
        for parent in (work, *work.parents):
            if (parent / ".git").exists() or (parent / "NOSH.md").exists():
                raise ValueError("workspace must be outside existing repositories and NOSH.md ancestry")
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
            report.save(data, output, previous)
        print("eval: interrupted", file=sys.stderr)
        return 130
    except (OSError, ValueError, RuntimeError, subprocess.SubprocessError) as exc:
        print(f"eval: {exc}", file=sys.stderr)
        if data is not None:
            data["error"] = str(exc)
            try:
                report.save(data, output, previous)
            except OSError as write_error:
                print(f"eval: could not preserve partial report: {write_error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
