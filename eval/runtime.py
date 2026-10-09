"""Tool discovery, isolated runtime configuration and reproducible provenance."""

from __future__ import annotations

from datetime import datetime, timezone
import json
import os
from pathlib import Path
import platform
import re
import shutil
import subprocess
import tarfile
import tomllib

from . import fixtures

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent


def inference_device(value: str) -> str:
    if value in ("cpu", "auto"):
        return value
    if value == "cuda":
        return "cuda:0"
    if isinstance(value, str) and re.fullmatch(r"cuda:[0-9]+", value):
        index = int(value[5:])
        if index < 2**31:
            return f"cuda:{index}"
    raise ValueError("device must be cpu, auto, cuda or cuda:N")


def resolve_source_revision(source_ref: str = "main", revision: str = "", cwd: Path | None = None) -> str:
    """Pin a commit reachable from one explicit origin branch, never a shell expression."""
    if not source_ref or source_ref.startswith("-"):
        raise ValueError("source_ref must be a branch name")
    if revision and not re.fullmatch(r"[0-9a-f]{40}", revision):
        raise ValueError("source_revision must be a full lowercase commit SHA")
    valid = subprocess.run(["git", "check-ref-format", "--branch", source_ref], cwd=cwd,
                           stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, check=False)
    if valid.returncode:
        raise ValueError("source_ref must be a valid branch name")
    subprocess.run(["git", "fetch", "--no-tags", "origin", f"refs/heads/{source_ref}"],
                   cwd=cwd, stdout=subprocess.DEVNULL, check=True)
    tip = subprocess.check_output(["git", "rev-parse", "--verify", "FETCH_HEAD^{commit}"],
                                  cwd=cwd, text=True).strip()
    source = revision or tip
    reachable = subprocess.run(["git", "merge-base", "--is-ancestor", source, tip],
                               cwd=cwd, check=False)
    if reachable.returncode == 1:
        raise ValueError("source_revision must be reachable from source_ref")
    reachable.check_returncode()
    return source


def source_tool_command(source: Path) -> list[str]:
    """Use the selected archive's tool and pin, not the harness checkout's submodule."""
    manifest = tomllib.loads(source.joinpath("Cargo.toml").read_text(encoding="utf-8"))
    expected_paths = {
        ("workspace", "dependencies", "reedline"): ".nosh/reedline",
        ("patch", "crates-io", "brush-core"): ".nosh/brush/brush-core",
        ("patch", "crates-io", "brush-parser"): ".nosh/brush/brush-parser",
    }
    for keys, path in expected_paths.items():
        dependency = manifest
        for key in keys:
            dependency = dependency.get(key) if isinstance(dependency, dict) else None
        if not isinstance(dependency, dict) or dependency.get("path") != path:
            raise ValueError("selected source has an incomplete managed dependency layout")
    tool = source / "tools" / "source" / "Cargo.toml"
    if not tool.is_file() or any(not source.joinpath("patches", name, "source.toml").is_file()
                                 for name in ("reedline", "brush-core")):
        raise ValueError("selected source has an incomplete managed dependency layout")
    return ["cargo", "run", "--manifest-path", str(tool), "--locked", "--"]


def source_dependency_provenance(source: Path) -> dict:
    command = source_tool_command(source)
    data = json.loads(subprocess.check_output(
        [*command, "provenance", "--root", str(source)], cwd=source, text=True))
    if not isinstance(data, dict):
        raise ValueError("selected source tool returned invalid dependency provenance")
    expected = ("reedline", "brush-core")
    if set(data) != set(expected):
        raise ValueError("selected source tool returned incomplete dependency provenance")
    for dependency in expected:
        state = data[dependency]
        pin = tomllib.loads(source.joinpath("patches", dependency, "source.toml").read_text(encoding="utf-8"))
        if (not isinstance(state, dict) or type(state.get("schema_version")) is not int
                or state["schema_version"] != 1
                or state.get("upstream_revision") != pin["revision"]
                or state.get("upstream_repository") != pin["repository"]
                or any(not isinstance(state.get(field), str)
                       or not re.fullmatch(r"[0-9a-f]{64}", state[field])
                       for field in ("patch_sha256", "source_pin_sha256", "prepared_archive_sha256"))
                or any(not isinstance(state.get(field), str)
                       or not re.fullmatch(r"[0-9a-f]{40}", state[field])
                       for field in ("upstream_tree", "prepared_tree"))):
            raise ValueError("selected source tool returned invalid dependency provenance")
    return {"schema_version": 1, "layout": "managed", "managed_sources": data}


def prepare_source_dependencies(source: Path) -> dict:
    command = source_tool_command(source)
    subprocess.run([*command, "prepare", "--root", str(source)], cwd=source, check=True)
    return source_dependency_provenance(source)


def verify_source_archive(source: Path, archive: Path) -> None:
    """Check tracked inputs after preparation/cache/build; generated outputs are separate."""
    with tarfile.open(archive) as contents:
        for member in contents:
            relative = Path(member.name)
            if relative.is_absolute() or ".." in relative.parts:
                raise ValueError("unsafe source archive member")
            path = source / relative
            if member.isfile():
                stream = contents.extractfile(member)
                if (stream is None or path.is_symlink() or not path.is_file()
                        or path.read_bytes() != stream.read()
                        or (os.name != "nt" and path.stat().st_mode & 0o111 != member.mode & 0o111)):
                    raise ValueError(f"archived source changed during preparation/build: {member.name}")
            elif member.issym():
                if not path.is_symlink() or os.readlink(path) != member.linkname:
                    raise ValueError(f"archived source link changed: {member.name}")
            elif not member.isdir():
                raise ValueError(f"unsupported source archive member: {member.name}")


def required_tools(scenarios: list[dict]) -> set[str]:
    required = {"git", "bash", "python3", "tar", "ss"}
    if any(s["fixture"].startswith("rust") for s in scenarios):
        required |= {"cargo", "rustc", "cc"}
    if any(s["fixture"] == "node" for s in scenarios):
        required |= {"node", "npm"}
    if any(s["check"] == "versions" for s in scenarios):
        required |= {"cargo", "node"}
    return required


def validate_workspace_ancestry(work: Path) -> None:
    for parent in (work, *work.parents):
        for name in (
            ".git", "AGENTS.md", "README.md", "Readme.md", "readme.md", "README.rst", "README.txt", "README",
        ):
            path = parent / name
            try:
                path.lstat()
            except FileNotFoundError:
                continue
            raise ValueError(
                f"workspace must be outside existing repositories and AGENTS.md/README ancestry: {path}"
            )


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


def environment(home: Path, threads: int, trace: Path | None, tools: dict | None = None,
                capture_output: str | None = None, command_assist: bool = False,
                device: str = "cpu") -> dict[str, str]:
    device = inference_device(device)
    if capture_output not in (None, "off", "last"):
        raise ValueError("capture_output must be off or last")
    if type(command_assist) is not bool:
        raise ValueError("command_assist must be a boolean")
    env = fixtures.project_environment(home, tools)
    env.update({
        "NOSH_HOME": str(home / "nosh"),
        "USER": "eval", "LOGNAME": "eval",
        "TERM": "xterm-256color", "NO_COLOR": "1", "NOSH_STATS": "1",
        "NOSH_OFFLINE": "1", "HF_HUB_OFFLINE": "1",
        "CANDLE_NUM_THREADS": str(threads), "RAYON_NUM_THREADS": "1",
    })
    # A CUDA-linked executable needs its libraries before it can select CPU.
    if "LD_LIBRARY_PATH" in os.environ:
        env["LD_LIBRARY_PATH"] = os.environ["LD_LIBRARY_PATH"]
    if device == "auto" or device.startswith("cuda:"):
        for key in ("CUDA_VISIBLE_DEVICES", "CUDA_DEVICE_ORDER"):
            if key in os.environ:
                env[key] = os.environ[key]
    config = home / "nosh"
    config.mkdir(mode=0o700)
    shell_config = f"[shell]\ncommand_assist = {str(command_assist).lower()}\n"
    if capture_output is not None:
        shell_config += f'capture_output = "{capture_output}"\n'
    (config / "config.toml").write_text(
        shell_config + '[agent]\napproval = "confirm"\nmax_steps = 10\ncommand_timeout_sec = 60\nrestore_cwd = false\n'
        f'[model]\ndevice = "{device}"\ncontext_length = 8192\nthinking = "off"\n[download]\nauto = "never"\n',
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


def harness_sources() -> list[Path]:
    """Runtime sources only, including scorer modules but not tests or archived judges."""
    return sorted([*HERE.glob("*.py"), *(HERE / "checks").rglob("*.py")])


def metadata(args, suite: dict, binary: Path, weights: Path, tokenizer: Path, toolchain: dict) -> dict:
    device = inference_device(getattr(args, "device", "cpu"))
    binary_hash = fixtures.file_hash(binary)
    build = {"source_revision": None, "source_clean": None, "binary_sha256": binary_hash,
             "provenance": "unverified external binary"}
    if args.build_info:
        build = json.loads(args.build_info.read_text(encoding="utf-8"))
        if (not isinstance(build, dict) or type(build.get("schema_version")) is not int
                or build["schema_version"] != 1 or build.get("binary_sha256") != binary_hash
                or not isinstance(build.get("source_revision"), str)
                or not re.fullmatch(r"[0-9a-f]{40}", build["source_revision"])):
            raise ValueError("build info does not identify this exact binary and source revision")
        build["provenance"] = "recorded build; supplied binary hash verified"
    tools = {"python": platform.python_version(), **{name: info["version"] for name, info in toolchain.items()}}
    sources = {p.relative_to(HERE).as_posix(): p for p in harness_sources()}
    content_hashes = {name: fixtures.source_hash(path) for name, path in sources.items()}
    return {
        "run_id": args.label or datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ"),
        "started_at": datetime.now(timezone.utc).isoformat(),
        "observation": "native-v1",
        "build": build,
        "model": {"weights": weights.name, "weights_sha256": fixtures.file_hash(weights),
                  "tokenizer_sha256": fixtures.file_hash(tokenizer)},
        "suite_sha256": fixtures.digest(suite),
        "suite_schema_version": suite["schema_version"],
        "dataset_revision": suite["dataset_revision"],
        "harness_sha256": fixtures.digest({name: fixtures.file_hash(path) for name, path in sources.items()}),
        "harness_content_sha256": fixtures.digest(content_hashes),
        "grading_content_sha256": fixtures.digest({name: value for name, value in content_hashes.items() if name.startswith("checks/")}),
        "settings": {"device": device, "ld_library_path": os.environ.get("LD_LIBRARY_PATH"),
                     "execution_mode": getattr(args, "execution_mode", "cold"),
                     "warmup_generations": 0,
                     "worker_start_timeout_s": getattr(args, "worker_start_timeout", 120)
                                                if getattr(args, "execution_mode", "cold") == "resident" else None,
                     "cuda_environment": {key: os.environ.get(key) for key in
                                          ("CUDA_VISIBLE_DEVICES", "CUDA_DEVICE_ORDER")}
                                         if device == "auto" or device.startswith("cuda:") else None,
                     "threads": args.threads, "rayon_threads": 1, "context_length": 8192,
                     "capture_output": "binary_default",
                     "max_steps": 10, "command_timeout_s": 60, "timeout_s": args.timeout or suite["timeout_s"],
                     "approval": "confirm", "locale": "C.UTF-8", "timezone": "UTC",
                     "path": "<trial-home>/bin:/usr/bin:/bin", "tty_size": [40, 160], "process_per_trial": True,
                     "process_niceness": os.getpriority(os.PRIO_PROCESS, 0),
                     "cargo_offline": True, "cargo_incremental": False, "cargo_jobs": 1, "npm_offline": True},
        "machine": machine_info(), "tools": tools, "toolchain": toolchain,
        "scenarios": suite["scenarios"], "seeds": args.seeds or suite["seeds"], "repeat": args.repeat,
    }
