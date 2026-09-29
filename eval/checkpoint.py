"""Copy an atomic report and its completed-trial logs without altering a campaign."""

from __future__ import annotations

import argparse
from contextlib import ExitStack
from datetime import datetime, timezone
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

from . import fixtures, report


def snapshot(source: Path, destination: Path) -> dict:
    source = source.resolve(strict=True)
    destination = destination.absolute()
    if destination.resolve().is_relative_to(source) or source.is_relative_to(destination.resolve()):
        raise ValueError("checkpoint and live campaign must not overlap")
    campaign = source / "campaign"
    raw = (campaign / "report.json").read_bytes()
    data = json.loads(raw)
    rendered = report.markdown(data)
    copies, directories = [], []
    for name in ("build-info.json", "environment.txt"):
        path = source / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"missing or unsafe provenance file: {name}")
        copies.append((path, Path(name)))
    for trial in data["trials"]:
        location = trial.get("logs")
        if not isinstance(location, str):
            raise ValueError("invalid trial log location")
        relative = Path(location)
        expected = Path("logs") / f"{trial['scenario_id']}-{trial['seed']}-{trial['repeat']}"
        if relative != expected:
            raise ValueError("invalid trial log location")
        directory = campaign / relative
        if directory.is_symlink() or not directory.is_dir() or not directory.resolve().is_relative_to(campaign.resolve()):
            raise ValueError("trial logs escape the campaign")
        directories.append(relative)
        for name in ("stdout.txt", "stderr.txt", "transcript.txt", "engine.jsonl"):
            path = directory / name
            if path.is_symlink():
                raise ValueError("trial log is a symlink")
            if path.is_file():
                copies.append((path, Path("campaign") / relative / name))
    with ExitStack() as cleanup:
        destination.mkdir(mode=0o700, parents=True, exist_ok=False)
        cleanup.callback(shutil.rmtree, destination)
        saved = destination / "campaign"
        saved.mkdir(mode=0o700)
        for relative in directories:
            (saved / relative).mkdir(mode=0o700, parents=True)
        (saved / "report.json").write_bytes(raw)
        (saved / "report.md").write_text(rendered, encoding="utf-8")
        for source_file, relative in copies:
            target = destination / relative
            target.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
            shutil.copyfile(source_file, target)
        info = {
            "schema_version": 1, "checkpoint_only": True,
            "captured_at": datetime.now(timezone.utc).isoformat(),
            "trials": len(data["trials"]),
            "counts": {status: sum(t["status"] == status for t in data["trials"]) for status in ("pass", "fail", "error")},
            "report_sha256": fixtures.file_hash(saved / "report.json"),
            "note": "Closed trials only. No regrading, resampling, or mutation of the live report.",
        }
        (destination / "checkpoint.json").write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
        cleanup.pop_all()
    return info


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("source", type=Path)
    parser.add_argument("destination", type=Path)
    args = parser.parse_args(argv)
    try:
        info = snapshot(args.source, args.destination)
        if sys.platform == "linux":
            resources = {
                "load_average": os.getloadavg(),
                "memory": Path("/proc/meminfo").read_text(),
                "free_disk_bytes": shutil.disk_usage(args.source).free,
                "processes": subprocess.check_output(
                    ["ps", "-u", str(os.getuid()), "-o", "pid,ppid,comm,pcpu,rss"],
                    env={"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8"}, text=True, timeout=5,
                ),
            }
            (args.destination / "resources.json").write_text(json.dumps(resources, indent=2) + "\n", encoding="utf-8")
        print(json.dumps(info))
        return 0
    except (OSError, ValueError, subprocess.SubprocessError) as exc:
        print(f"eval checkpoint: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
