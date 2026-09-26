"""Verify a downloaded evidence ZIP; no automatic download or model execution."""

from __future__ import annotations

import argparse
import hashlib
import json
from pathlib import Path, PurePosixPath
import stat
import subprocess
import sys
import tempfile
import zipfile

HERE = Path(__file__).resolve().parent


def digest(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def unpack(path: Path, index: dict, destination: Path) -> None:
    if path.stat().st_size != index["bytes"] or digest(path) != index["sha256"]:
        raise ValueError("archive size or SHA-256 does not match archive.json")
    with zipfile.ZipFile(path) as archive:
        members = archive.infolist()
        if (len(members) != index["file_count"]
                or sum(member.file_size for member in members) != index["uncompressed_bytes"]):
            raise ValueError("archive entry count or expanded size differs")
        manifest_bytes = archive.read("manifest.json")
        if hashlib.sha256(manifest_bytes).hexdigest() != index["manifest_sha256"]:
            raise ValueError("archive manifest SHA-256 differs")
        files = json.loads(manifest_bytes)["files"]
        if (len(files) + 1 != len(members)
                or {member.filename for member in members} != set(files) | {"manifest.json"}):
            raise ValueError("archive file set differs from the manifest")
        if any(destination.iterdir()):
            raise ValueError("verification directory must be empty")
        for member in members:
            name = member.filename
            relative = PurePosixPath(name)
            if (relative.is_absolute() or ".." in relative.parts or ":" in name or "\\" in name
                    or relative.as_posix() != name or member.is_dir()
                    or stat.S_ISLNK(member.external_attr >> 16)):
                raise ValueError(f"unsafe archived path: {name}")
            content = archive.read(member)
            if name != "manifest.json":
                expected = files[name]
                if len(content) != expected["bytes"] or hashlib.sha256(content).hexdigest() != expected["sha256"]:
                    raise ValueError(f"archived file changed: {name}")
            target = destination.joinpath(*relative.parts)
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_bytes(content)


def verify(path: Path, index: dict, summary: dict) -> None:
    with tempfile.TemporaryDirectory(prefix="nosh-evidence-") as temporary:
        root = Path(temporary)
        unpack(path, index, root)
        if json.loads((root / "summary.json").read_text(encoding="utf-8")) != summary:
            raise ValueError("archived summary does not match the committed baseline")
        subprocess.run([sys.executable, "-I", "-B", str(root / "verify.py")], cwd=root, check=True)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--archive", required=True, type=Path, help="ZIP downloaded from the URL in archive.json")
    parser.add_argument("--check", action="store_true", help="verification is always read-only; kept for compatibility")
    args = parser.parse_args(argv)
    try:
        index = json.loads((HERE / "archive.json").read_text(encoding="utf-8"))
        summary = json.loads((HERE / "summary.json").read_text(encoding="utf-8"))
        verify(args.archive, index, summary)
        return 0
    except (OSError, ValueError, KeyError, zipfile.BadZipFile, subprocess.SubprocessError) as exc:
        print(f"eval archive: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
