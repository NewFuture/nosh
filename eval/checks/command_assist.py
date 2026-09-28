"""Deterministic command assist scoring; no model judge."""

from __future__ import annotations
from pathlib import Path
import re
import subprocess
import tarfile
from ..approval import shell_parts



def archive_command(command: str, root: Path) -> tuple[list[str], Path]:
    parts = shell_parts(command.strip())
    cwd = root
    if parts[:1] == ["cd"]:
        try:
            split = next(i for i, word in enumerate(parts) if word in ("&&", ";"))
        except StopIteration as exc:
            raise ValueError("cd must be followed by tar") from exc
        directory = parts[1:split]
        if directory[:1] == ["--"]:
            directory = directory[1:]
        if len(directory) != 1:
            raise ValueError("unsupported cd arguments")
        cwd = (root / directory[0]).resolve()
        if cwd not in (root, root / "logs"):
            raise ValueError("cd escapes the logs fixture")
        parts = parts[split + 1:]
    if not parts or parts[0] not in ("tar", "/usr/bin/tar", "/bin/tar"):
        raise ValueError("only tar with an optional cd is supported")
    if any(p in (";", "&&", "&", "|", "||") for p in parts):
        raise ValueError("additional shell commands are not allowed")
    args = parts[1:]
    created = zipped = False
    archive = None
    operands = []
    input_cwd = cwd
    i = 0
    while i < len(args):
        word = args[i]
        if word in ("--create", "--gzip", "--verbose"):
            created |= word == "--create"
            zipped |= word == "--gzip"
        elif word in ("--file", "--directory", "-C") or word.startswith(("--file=", "--directory=")):
            flag, separator, value = word.partition("=")
            if not separator:
                i += 1
                if i == len(args):
                    raise ValueError("missing tar option argument")
                value = args[i]
            if flag == "--file":
                if archive is not None:
                    raise ValueError("multiple archive paths")
                archive = (cwd / value).resolve()
            else:
                input_cwd = (input_cwd / value).resolve()
                if input_cwd != root / "logs":
                    raise ValueError("tar directory escapes logs")
        elif (word.startswith("-") and word != "--") or (i == 0 and re.fullmatch(r"[czvf]+", word)):
            traditional = not word.startswith("-")
            flags = word if traditional else word[1:]
            if (not flags or any(c not in "czvf" for c in flags) or flags.count("f") > 1
                    or (not traditional and "f" in flags and not flags.endswith("f"))):
                raise ValueError(f"unsupported tar flags: {word}")
            created |= "c" in flags
            zipped |= "z" in flags
            if "f" in flags:
                i += 1
                if i == len(args) or archive is not None:
                    raise ValueError("invalid archive path")
                archive = (cwd / args[i]).resolve()
        elif word == "--":
            operands.extend((input_cwd / name).resolve() for name in args[i + 1:])
            break
        else:
            operands.append((input_cwd / word).resolve())
        i += 1
    if not created or not zipped or archive != root / "logs.tar.gz":
        raise ValueError("expected gzip creation at logs.tar.gz")
    if not operands or any(path != root / "logs" for path in operands):
        raise ValueError("only the fixture logs directory can be archived")
    return ["tar", *args], cwd


def check_archive(answer: str, root: Path, before: dict, after: dict) -> list[str]:
    if after != before:
        return ["nosh -s modified the fixture instead of only suggesting"]
    syntax = subprocess.run(["bash", "--noprofile", "--norc", "-n", "-c", answer],
                            env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=5)
    if syntax.returncode:
        return ["invalid shell syntax: " + syntax.stderr.strip()]
    try:
        args, cwd = archive_command(answer, root)
    except ValueError as exc:
        return [str(exc)]
    archive = root / "logs.tar.gz"
    try:
        proc = subprocess.run(args, cwd=cwd, env={"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8"},
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
        if proc.returncode:
            return ["tar failed: " + proc.stderr.strip()]
        expected = {name.removeprefix("logs/"): item["sha256"] for name, item in before.items()}
        actual = {}
        with tarfile.open(archive, "r:gz") as stream:
            for member in stream:
                if member.isdir():
                    continue
                if not member.isfile():
                    return ["archive contains non-regular entries"]
                name = member.name.removeprefix("./").removeprefix("logs/")
                if name in actual:
                    return ["archive contains duplicate files"]
                import hashlib

                with stream.extractfile(member) as content:
                    actual[name] = hashlib.file_digest(content, "sha256").hexdigest()
        return [] if actual == expected else ["archive members or contents do not match logs"]
    except (tarfile.TarError, OSError, subprocess.TimeoutExpired) as exc:
        return [f"archive validation failed: {exc}"]
    finally:
        archive.unlink(missing_ok=True)


def assistance_judgment(scenario, answer, facts, root, after, evidence):
    kind = scenario["check"]
    reasons = []
    expected = scenario["assistance"]
    accepted = (evidence or {}).get("assistance") or []
    if len(accepted) != 1:
        reasons.append("expected exactly one observed assistance result")
    else:
        actual = accepted[0]
        if (actual.get("status") != "completed" or actual.get("intent") != expected["intent"]
                or actual.get("kind") != expected["result"] or actual.get("background") != expected["automatic"]):
            reasons.append("assistance result does not match the declared intent and result")
    if any(call["name"] not in ("command_info", "read_file", "grep", "finish")
           for call in (evidence or {}).get("tool_calls") or []):
        reasons.append("assistance attempted an unavailable task-execution tool")
    if expected.get("require_query") and not any(
            item["call"]["name"] == "command_info"
            and item["call"]["args"].get("query") == "help"
            and item.get("state") == "returned"
            and isinstance(item.get("result"), str)
            and re.match(r"^\[query program=.+ exit=0 truncated=(?:true|false)(?: topic=[^\n]*)?\]\n", item["result"])
            for item in (evidence or {}).get("executions") or []):
        reasons.append("installed command help was not successfully queried")
    if kind == "assist-archive":
        reasons.extend(check_archive(answer, root, facts["before"], after))
    elif kind == "assist-none" and answer:
        reasons.append("no-suggestion result must not contain a command or prose")
    elif kind == "assist-clarify" and not (
            re.search(r"directory|folder|source|目录|源", answer, re.I)
            and re.search(r"destination|filename|file name|output|目标|文件名", answer, re.I)):
        reasons.append("clarification does not request the source and destination")

    return reasons
