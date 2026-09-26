"""Copy an atomic report and its completed-trial logs without altering a campaign."""

from __future__ import annotations

import argparse
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
    report.validate(data)
    destination.mkdir(mode=0o700, parents=True, exist_ok=False)
    saved = destination / "campaign"
    saved.mkdir(mode=0o700)
    (saved / "report.json").write_bytes(raw)
    (saved / "report.md").write_text(report.markdown(data), encoding="utf-8")
    for name in ("build-info.json", "environment.txt"):
        path = source / name
        if path.is_symlink() or not path.is_file():
            raise ValueError(f"missing or unsafe provenance file: {name}")
        shutil.copyfile(path, destination / name)
    for trial in data["trials"]:
        relative = Path(trial["logs"])
        if relative.is_absolute() or ".." in relative.parts or len(relative.parts) != 2 or relative.parts[0] != "logs":
            raise ValueError("invalid trial log location")
        directory = campaign / relative
        if directory.is_symlink() or not directory.is_dir() or not directory.resolve().is_relative_to(campaign.resolve()):
            raise ValueError("trial logs escape the campaign")
        copied = saved / relative
        copied.mkdir(mode=0o700, parents=True)
        for name in ("stdout.txt", "stderr.txt", "transcript.txt", "engine.jsonl"):
            path = directory / name
            if path.is_symlink():
                raise ValueError("trial log is a symlink")
            if path.is_file():
                shutil.copyfile(path, copied / name)
    info = {
        "schema_version": 1, "checkpoint_only": True,
        "captured_at": datetime.now(timezone.utc).isoformat(),
        "trials": len(data["trials"]),
        "counts": {status: sum(t["status"] == status for t in data["trials"]) for status in ("pass", "fail", "error")},
        "report_sha256": fixtures.file_hash(saved / "report.json"),
        "note": "Closed trials only. No regrading, resampling, or mutation of the live report.",
    }
    (destination / "checkpoint.json").write_text(json.dumps(info, indent=2) + "\n", encoding="utf-8")
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
