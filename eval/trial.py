"""One isolated trial: prepare, execute, observe, grade and preserve evidence."""

from __future__ import annotations

import contextlib
from pathlib import Path
import shutil
import subprocess

from . import approval, checks, driver, fixtures, report
from .observations import observe
from .runtime import environment


def attachment_bytes(root: Path, name: str, facts: dict) -> bytes:
    path = root / name
    if (name not in facts["before"] or path.is_symlink() or not path.is_file()
            or not path.resolve().is_relative_to(root.resolve())):
        raise ValueError("stdin attachment must be a recorded regular fixture file")
    with path.open("rb") as stream:
        data = stream.read(64 * 1024 + 1)
    if not data or len(data) > 64 * 1024 or b"\0" in data:
        raise ValueError("stdin attachment must be nonempty text within 64 KiB")
    data.decode("utf-8", errors="strict")
    return data


def run_trial(args, meta: dict, scenario: dict, seed: int, repeat: int,
              workspace: fixtures.Workspace, output: Path, binary: Path, weights: Path, *, worker=None) -> dict:
    row = {
        "scenario_id": scenario["id"], "seed": seed, "repeat": repeat, "status": "error",
        "metrics": {key: None for key in report.METRICS},
        "answer": "", "reasons": [], "inputs": None, "tool_calls": None, "final_state": None,
        "grading": None, "executions": None,
    }
    result = None
    trace = None
    expected_worker = worker.record if worker is not None else None
    logs = output / "logs" / f"{scenario['id']}-{seed}-{repeat}"
    logs.mkdir(parents=True)
    try:
        root, home, facts = workspace.prepare(scenario, seed, repeat)
        trace = home.parent / "engine.jsonl"
        env = environment(home, args.threads, trace, workspace.tools,
                          capture_output=scenario.get("capture_output"),
                          command_assist=scenario.get("assistance", {}).get("automatic", False),
                          device=getattr(args, "device", "cpu"))
        if worker is not None and scenario["check"] != "typos":
            env["NOSH_EVAL_WORKER"] = str(worker.path)
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
                if scenario.get("stdin_file"):
                    data = attachment_bytes(root, scenario["stdin_file"], facts)
                elif scenario.get("stdin_command"):
                    data = subprocess.check_output(scenario["stdin_command"], cwd=root, env=env, timeout=5)
                flags = ["-a", "--json"] if scenario["mode"] == "agent" else ["-s"]
                result = driver.run_cli(argv + flags + [scenario["input"]], root, env, timeout, data)
        row.update(approvals=result.approvals, questions=result.questions,
                   turns=result.turns, exit_code=result.exit_code, facts=facts)
        row["metrics"].update(total_s=result.total_s, peak_rss_mib=result.peak_rss_mib,
                              confirmations=len(result.approvals))
        after = fixtures.snapshot(root)
        row.update(fixture_sha256=fixtures.digest({k: v for k, v in facts.items() if k != "listener"}),
                   file_snapshot=after, final_state=fixtures.fixture_state(scenario, facts, root, after, result))
        if result.error:
            if result.timeout_phase in ("agent", "cli", "assist") and trace.is_file():
                observed = observe(result, scenario, trace, seed=seed, deadline_timeout=True,
                                   expected_worker=expected_worker,
                                   expected_device=getattr(args, "device", "cpu"))
                row.update(observed)
                stage = ("after final generation completed but before the completion marker"
                         if observed["deadline_state"] == "after_generation"
                         else "during model generation")
                reason = (f"agent exceeded the {timeout:g}s trial deadline {stage} "
                          f"({row['metrics']['steps']} started steps)")
                if scenario.get("expect") and row["metrics"]["steps"] > scenario["expect"]["max_steps"]:
                    reason += f"; step budget is {scenario['expect']['max_steps']}"
                row.update(status="fail", reasons=[reason], timeout_phase=result.timeout_phase,
                           grading={"facts": {"passed": False, "reasons": [reason]}, "experience": None})
                return row
            raise driver.DriverError(result.error)
        if result.failure:
            if trace.is_file():
                observed = observe(result, scenario, trace, seed=seed,
                                   expected_worker=expected_worker,
                                   expected_device=getattr(args, "device", "cpu"))
                row.update(observed)
                if observed["metrics"]["task_status"] is None:
                    raise driver.DriverError("an agent task ran but its completion marker was not observed")
            row.update(status="fail", reasons=[result.failure],
                       grading={"facts": {"passed": False, "reasons": [result.failure]}, "experience": None})
            return row
        observed = observe(result, scenario, trace, seed=seed,
                           expected_worker=expected_worker,
                           expected_device=getattr(args, "device", "cpu"))
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
