"""Deterministic command assist scoring; no model judge."""

from __future__ import annotations
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tarfile
import tempfile
from ..approval import shell_parts
from .common import response_prose
from .experience import clarification_request
from .workflows import next_review_judgment



def archive_command(command: str, root: Path, *, source: str = "logs",
                    destination: str = "logs.tar.gz") -> tuple[list[str], Path]:
    source_root = (root / source).resolve()
    target = (root / destination).resolve()
    if not source_root.is_relative_to(root) or not target.is_relative_to(root):
        raise ValueError("archive operands escape the fixture")
    parts = shell_parts(command.strip())
    if parts[-1:] == [";"]:
        parts.pop()
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
        if cwd not in (root, source_root):
            raise ValueError("cd escapes the archive fixture")
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
                if input_cwd != source_root:
                    raise ValueError("tar directory escapes the declared source")
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
    if not created or not zipped or archive != target:
        raise ValueError(f"expected gzip creation at {destination}")
    if not operands or any(path != source_root for path in operands):
        raise ValueError("only the declared source directory can be archived")
    return ["tar", *args], cwd


def shell_syntax_reasons(command: str) -> list[str]:
    syntax = subprocess.run(["bash", "--noprofile", "--norc", "-n", "-c", command],
                            env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=5)
    return ["invalid shell syntax: " + syntax.stderr.strip()] if syntax.returncode else []


def check_archive(answer: str, root: Path, before: dict, after: dict, *,
                  source: str = "logs", destination: str = "logs.tar.gz") -> list[str]:
    if after != before:
        return ["nosh -s modified the fixture instead of only suggesting"]
    if reasons := shell_syntax_reasons(answer):
        return reasons
    try:
        args, cwd = archive_command(answer, root, source=source, destination=destination)
    except ValueError as exc:
        return [str(exc)]
    archive = root / destination
    try:
        proc = subprocess.run(args, cwd=cwd, env={"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8"},
                              stdin=subprocess.DEVNULL, capture_output=True, text=True, timeout=10)
        if proc.returncode:
            return ["tar failed: " + proc.stderr.strip()]
        prefix = source.rstrip("/") + "/"
        expected = {name.removeprefix(prefix): item["sha256"] for name, item in before.items()
                    if name.startswith(prefix)}
        actual = {}
        with tarfile.open(archive, "r:gz") as stream:
            for member in stream:
                if member.isdir():
                    continue
                if not member.isfile():
                    return ["archive contains non-regular entries"]
                name = member.name.removeprefix("./").removeprefix(prefix)
                if name in actual:
                    return ["archive contains duplicate files"]
                import hashlib

                with stream.extractfile(member) as content:
                    actual[name] = hashlib.file_digest(content, "sha256").hexdigest()
        return [] if actual == expected else ["archive members or contents do not match the source"]
    except (tarfile.TarError, OSError, subprocess.TimeoutExpired) as exc:
        return [f"archive validation failed: {exc}"]
    finally:
        archive.unlink(missing_ok=True)


def resume_archive_judgment(scenario, answer, facts, root, after, evidence):
    expected = dict(facts["before"])
    expected["archive/report.csv"] = expected.pop("incoming/report.csv")
    if after != expected or (root / "backups").exists():
        return ["only the user's initial move may change the fixture; assistance must not execute the fix"]
    accepted = (evidence or {}).get("assistance") or []
    execution = accepted[0].get("execution") if len(accepted) == 1 else None
    if (not isinstance(execution, dict) or execution.get("command") != scenario["inputs"][0]
            or execution.get("execution_cwd") != str(root)
            or type(execution.get("exit")) is not int or execution["exit"] == 0):
        return ["the original failed compound command was not observed"]
    if reasons := shell_syntax_reasons(answer):
        return reasons
    prefix, separator, command = answer.partition("&&")
    try:
        parts = shell_parts(prefix)
        if not separator or parts[:1] != ["mkdir"]:
            raise ValueError("the repair must prepare the missing backups directory before archiving")
        args = parts[1:]
        while args and args[0] in ("-p", "--parents", "--"):
            args = args[1:]
        if len(args) != 1 or (root / args[0]).resolve() != root / "backups":
            raise ValueError("only the missing backups directory may be prepared")
        argv, cwd = archive_command(command, root, source="archive", destination="backups/reports.tar.gz")
    except ValueError as exc:
        return [str(exc)]
    # Verify the proposal in a copy: the original user-side effect is evidence.
    with tempfile.TemporaryDirectory(prefix="nosh-archive-verifier-") as temporary:
        copied = Path(temporary) / "files"
        shutil.copytree(root, copied, symlinks=True)
        (copied / "backups").mkdir()
        mapped = []
        for arg in argv:
            flag, separator, value = arg.partition("=")
            option_value = separator and flag in ("--file", "--directory")
            path = Path(value if option_value else arg)
            if path.is_absolute() and path.is_relative_to(root):
                replacement = str(copied / path.relative_to(root))
                arg = flag + "=" + replacement if option_value else replacement
            mapped.append(arg)
        program = shlex.join(mapped)
        if cwd != root:
            program = "cd " + shlex.quote(str(copied / cwd.relative_to(root))) + " && " + program
        return check_archive(program, copied, expected, expected,
                             source="archive", destination="backups/reports.tar.gz")


def requested_archive_fields(answer: str) -> set[str]:
    fields = set()
    aliases = {
        "source": r"\b(?:source(?:\s+(?:directory|folder|path))?|directory|folder)\b|源目录|输入目录|目录|文件夹|源",
        "destination": r"\b(?:(?:destination|output)(?:\s+(?:file\s*name|file|path))?|file\s*name)\b|目标文件名|输出文件名|文件名|目标",
    }
    prose = response_prose(answer, keep_inline=True, keep_paths=True)
    for sentence in re.split(r"(?<=[。！？!?；;])|\n|(?<=\.)\s+", prose):
        request = clarification_request(sentence, archive=True)
        if request is None:
            continue
        heads = list(re.finditer(
            r"\b(?:what|which|specify|describe|clarify|provide|share|tell me|let me know)\b", request, re.I,
        ))
        if heads:
            candidates = []
            for index, head in enumerate(heads):
                target = request[head.end():heads[index + 1].start() if index + 1 < len(heads) else len(request)]
                target = re.sub(
                    r"^\s*(?:should|can|could|would)\s+I\s+(?:use|choose)\s+(?:for|as)\s+", "", target, flags=re.I,
                )
                target = re.sub(r"^\s*(?:is|are|should|can|could|would|will|must)\s+", "", target, flags=re.I)
                candidates.extend(re.split(r"\band\b|,", target, flags=re.I))
            for candidate in candidates:
                noun_phrase = re.split(
                    r"\b(?:should|shall|would|do|does|is|are|be|will|can|could|using|save)\b",
                    candidate, maxsplit=1, flags=re.I,
                )[0].strip(" ?")
                for name, pattern in aliases.items():
                    match = re.search(pattern, noun_phrase, re.I)
                    if match:
                        remainder = noun_phrase[match.end():]
                        tail = "" if remainder == "." else remainder.strip()
                        if not tail or re.fullmatch(
                            r"to (?:archive|compress|package|back up)"
                            r"|for (?:(?:the|an?) )?(?:archive|archiving|compression|packaging|backup)",
                            tail, re.I,
                        ):
                            fields.add(name)
        else:
            for clause in re.split(r"[，,]", request):
                mentions = sorted(
                    ((match, name) for name, pattern in aliases.items()
                     for match in re.finditer(pattern, clause, re.I)), key=lambda item: item[0].start(),
                )
                for index, (match, name) in enumerate(mentions):
                    end = mentions[index + 1][0].start() if index + 1 < len(mentions) else len(clause)
                    tail = clause[match.end():end]
                    question = re.search(r"什么|哪(?:个|些|里)", tail)
                    bare = re.fullmatch(r"[\s?？。.!！]*|(?:\s*(?:和|及|以及|还有|与|、|and)\s*)+", tail, re.I)
                    if question or bare:
                        fields.add(name)
    return fields


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
            and isinstance(item["call"]["args"].get("name"), str)
            and Path(item["call"]["args"]["name"]).name == "tar"
            and item.get("state") == "returned"
            and isinstance(item.get("result"), str)
            and re.match(r"^\[query program=(?:[^\n]*/)?tar exit=0 truncated=(?:true|false)(?: topic=[^\n]*)?\]\n", item["result"])
            for item in (evidence or {}).get("executions") or []):
        reasons.append("installed command help was not successfully queried")
    if kind == "assist-archive":
        reasons.extend(check_archive(answer, root, facts["before"], after))
    elif kind == "assist-resume-archive":
        reasons.extend(resume_archive_judgment(scenario, answer, facts, root, after, evidence))
    elif kind == "assist-next-review":
        reasons.extend(next_review_judgment(scenario, answer, facts, root, after, evidence))
    elif kind == "assist-none" and answer:
        reasons.append("no-suggestion result must not contain a command or prose")
    elif kind == "assist-clarify":
        if requested_archive_fields(answer) != {"source", "destination"}:
            reasons.append("clarification does not request the source and destination")

    return reasons
