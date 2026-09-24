"""Rebuilt, versioned fixtures and an exclusively owned working directory."""

from __future__ import annotations

import contextlib
import hashlib
import json
import os
from pathlib import Path
import random
import re
import select
import shutil
import subprocess
import sys
import time

EPOCH = 1_700_000_000
OWNER = "nosh-eval-workspace-v1\n"
FIXTURES = {"big", "port", "project", "rename", "typo", "failure", "history", "logs"}
PROJECT = {
    "main.py": "from lib.maths import add\nvalues = [1, 2, 3]\nresult = add(values[0], values[1])\nprint(result)\nprint(len(values))\n",
    "lib/maths.py": "def add(a, b):\n    return a + b\ndef square(value):\n    return value * value\nprint(square(3))\n",
    "tools/report.py": "names = ['alpha', 'beta']\nfor name in names:\n    print(name)\ncount = len(names)\nprint(count)\n",
    "web/app.js": "const count = 3;\nconst double = n => n * 2;\nconsole.log(count);\nconsole.log(double(count));\n",
    "src/main.rs": "fn double(n: i32) -> i32 {\n    n * 2\n}\nfn main() {\n    println!(\"{}\", double(3));\n}\n",
    "scripts/check.sh": "name=fixture\ncount=3\nprintf '%s\\n' \"$name\"\nprintf '%s\\n' \"$count\"\n",
}
HISTORY = [
    ("shell", "pipeline", "support shell pipelines"),
    ("permissions", "approval", "require approval for file writes"),
    ("llm", "seed", "add seeded model sampling"),
    ("hub", "checksum", "verify model checksums"),
    ("core", "truncate", "truncate long tool output"),
    ("cli", "suggest", "add command suggestion mode"),
    ("tests", "timeout", "cover command timeouts"),
    ("docs", "offline", "document offline usage"),
]


def file_hash(path: Path) -> str:
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def digest(value: object) -> str:
    data = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return hashlib.sha256(data.encode()).hexdigest()


def snapshot(root: Path) -> dict:
    result = {}
    for base, dirs, files in os.walk(root, followlinks=False):
        dirs[:] = sorted(d for d in dirs if d != ".git")
        for name in sorted(files + [d for d in dirs if (Path(base) / d).is_symlink()]):
            path = Path(base) / name
            key = path.relative_to(root).as_posix()
            if path.is_symlink():
                result[key] = {"symlink": os.readlink(path)}
            elif path.is_file():
                result[key] = {"bytes": path.stat().st_size, "sha256": file_hash(path)}
    return result


def write(root: Path, name: str, text: str) -> None:
    path = root / name
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(text, encoding="utf-8")


def git(root: Path, *args: str) -> str:
    env = {
        "PATH": "/usr/bin:/bin",
        "HOME": str(root),
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        "TZ": "UTC",
        "GIT_CONFIG_NOSYSTEM": "1",
        "GIT_CONFIG_GLOBAL": os.devnull,
        "GIT_AUTHOR_NAME": "Eval Fixture",
        "GIT_AUTHOR_EMAIL": "fixture@example.invalid",
        "GIT_COMMITTER_NAME": "Eval Fixture",
        "GIT_COMMITTER_EMAIL": "fixture@example.invalid",
        "GIT_AUTHOR_DATE": f"{EPOCH} +0000",
        "GIT_COMMITTER_DATE": f"{EPOCH} +0000",
    }
    return subprocess.check_output(
        ["git", "-c", "core.autocrlf=false", "-c", "commit.gpgsign=false", *args],
        cwd=root, env=env, stderr=subprocess.PIPE, text=True,
    )


def create(root: Path, kind: str) -> dict:
    if kind not in FIXTURES:
        raise ValueError(f"unknown fixture: {kind}")
    root.mkdir(mode=0o700)
    facts: dict = {}
    if kind == "big":
        sizes = {
            "data/dump.bin": 21_000_000,
            "video.bin": 12_000_000,
            "cache/archive.bin": 5_000_000,
            "data/small.bin": 800_000,
            "notes.txt": 100,
        }
        block = random.Random(0).randbytes(1024 * 1024)
        for name, size in sizes.items():
            path = root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            with path.open("wb") as stream:
                # Materialize bytes: sparse truncate() would make ordinary du
                # report zero for every file, changing the intended task.
                data = b"nosh evaluation fixture\n" if name.endswith(".txt") else block
                remaining = size
                while remaining:
                    written = stream.write(data[:remaining])
                    remaining -= written
        facts["largest"] = sorted(sizes, key=sizes.get, reverse=True)[:3]
        facts["data_files"] = ["dump.bin", "small.bin"]
    elif kind == "project":
        for name, text in PROJECT.items():
            write(root, name, text)
        languages = {".py": "python", ".js": "javascript", ".rs": "rust", ".sh": "shell"}
        counts: dict[str, int] = {}
        for name, text in PROJECT.items():
            language = languages[Path(name).suffix]
            counts[language] = counts.get(language, 0) + len(text.splitlines())
        facts.update(
            languages=counts, total=sum(counts.values()), file_count=len(PROJECT),
            language_files={lang: sum(Path(name).suffix == ext for name in PROJECT)
                            for ext, lang in languages.items()},
            python=sorted(name for name in PROJECT if name.endswith(".py")),
        )
    elif kind == "rename":
        for name in ["alpha.txt", "beta.txt", "two words.txt", "notes.txt", "readme.md"]:
            write(root, name, f"Keep these bytes: {name}\n")
        facts["renames"] = {n: n[:-4] + ".md" for n in snapshot(root) if n.endswith(".txt")}
    elif kind == "failure":
        write(root, "broken.py", "import json\nwith open('config.json') as stream:\n    settings = json.load(stream)\nprint(settings)\n")
    elif kind == "logs":
        write(root, "logs/app.log", "INFO service started\nERROR fixture error\n")
        write(root, "logs/old/access.log", "GET /health 200\n")
    elif kind in {"history", "typo"}:
        git(root, "init", "--quiet", "--initial-branch=fixture", "--template=")
        if kind == "history":
            for component, fact, subject in HISTORY:
                write(root, f"{component}/{fact}.txt", f"{component}: {subject}\n")
                git(root, "add", ".")
                git(root, "commit", "--quiet", "-m", f"{component}: {subject}")
            facts["history"] = [list(entry[:2]) for entry in HISTORY]
            facts["git_log"] = git(root, "log", "--stat", "-8")
    for path in sorted(root.rglob("*"), reverse=True):
        if not path.is_symlink():
            path.chmod(0o755 if path.is_dir() else 0o644)
            os.utime(path, (EPOCH, EPOCH))
    os.utime(root, (EPOCH, EPOCH))
    facts["before"] = snapshot(root)
    return facts


class Workspace:
    def __init__(self, root: Path):
        self.root = root.absolute()
        self.lock = None

    def __enter__(self):
        import fcntl

        if self.root.is_symlink() or self.root == Path(self.root.anchor):
            raise ValueError(f"unsafe workspace: {self.root}")
        if self.root.resolve() != self.root:
            raise ValueError(f"workspace must not traverse symlinks: {self.root}")
        self.root.mkdir(mode=0o700, parents=True, exist_ok=True)
        marker = self.root / ".owner"
        if not marker.exists():
            if any(self.root.iterdir()):
                raise ValueError(f"refusing non-eval workspace: {self.root}")
            with marker.open("x", encoding="ascii") as stream:
                stream.write(OWNER)
        if marker.is_symlink() or marker.read_text(encoding="ascii") != OWNER:
            raise ValueError(f"invalid workspace ownership: {self.root}")
        fd = os.open(self.root / ".lock", os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
        self.lock = os.fdopen(fd, "w")
        try:
            fcntl.flock(self.lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError as exc:
            self.lock.close()
            self.lock = None
            raise RuntimeError(f"another evaluation owns {self.root}") from exc
        return self

    def clean(self, scenario_id: str) -> None:
        if self.lock is None or self.lock.closed:
            raise ValueError("workspace cleanup requires its exclusive lock")
        if not re.fullmatch(r"[a-z][a-z0-9-]*", scenario_id):
            raise ValueError(f"invalid scenario id: {scenario_id}")
        if self.root.is_symlink() or self.root.resolve() != self.root:
            raise ValueError("workspace moved or replaced")
        marker = self.root / ".owner"
        if marker.is_symlink() or marker.read_text(encoding="ascii") != OWNER:
            raise ValueError("workspace ownership marker changed")
        path = self.root / scenario_id
        if path.is_symlink():
            path.unlink()
        elif path.exists():
            shutil.rmtree(path)

    def prepare(self, scenario: dict) -> tuple[Path, Path, dict]:
        self.clean(scenario["id"])
        base = self.root / scenario["id"]
        base.mkdir(mode=0o700)
        home = base / "home"
        home.mkdir(mode=0o700)
        root = base / "files"
        return root, home, create(root, scenario["fixture"])

    def __exit__(self, *_):
        if self.lock:
            self.lock.close()


@contextlib.contextmanager
def listener(root: Path, port: int = 8080):
    proc = subprocess.Popen(
        [sys.executable, "-u", "-m", "http.server", str(port), "--bind", "127.0.0.1",
         "--directory", str(root)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True,
    )
    try:
        deadline = time.monotonic() + 5
        ready = b""
        while b"Serving HTTP" not in ready:
            if proc.poll() is not None:
                raise RuntimeError(f"port fixture could not bind {port}: {proc.stderr.read().decode()}")
            if time.monotonic() >= deadline:
                raise TimeoutError("port fixture did not become ready")
            if select.select([proc.stdout], [], [], 0.05)[0]:
                ready += os.read(proc.stdout.fileno(), 4096)
        yield {"pid": proc.pid, "port": port, "process": "python3"}
    finally:
        if proc.poll() is None:
            proc.terminate()
            try:
                proc.wait(timeout=2)
            except subprocess.TimeoutExpired:
                proc.kill()
                proc.wait()
        proc.stdout.close()
        proc.stderr.close()
