"""Deterministic command assist scoring; no model judge."""

from __future__ import annotations
from collections import Counter
from dataclasses import dataclass, replace
import glob
import itertools
import os
from pathlib import Path, PurePosixPath
import re
import shutil
import subprocess
import tarfile
import tempfile
import time
from ..approval import shell_parts
from .. import fixtures


ARCHIVE_GLOB_MATCHES = 1024
ARCHIVE_PROGRAM_COMMANDS = 32
ARCHIVE_INFO_FLAGS = {"--help", "--usage", "--version"}


@dataclass(frozen=True)
class _ArchiveWord:
    value: str
    pattern: str | None = None
    operator: bool = False


def _archive_words(command: str) -> list[_ArchiveWord]:
    shell_parts(command)  # Keep the shared ban on substitutions, escapes and redirections.
    words = []
    end = 0
    for match in re.finditer(r"""(?:'[^']*'|"[^"]*"|[^\s;&|'"])+|[;&|]+|\n""", command):
        if command[end:match.start()].strip(" \t"):
            raise ValueError("unsupported shell whitespace")
        end = match.end()
        raw = match[0]
        if re.fullmatch(r"[;&|]+|\n", raw):
            if raw == "\n" and (not words or words[-1].operator):
                continue
            words.append(_ArchiveWord(raw, operator=True))
            continue
        value, pattern, active = [], [], False
        for part in re.findall(r"""'[^']*'|"[^"]*"|[^'"]+""", raw):
            if part[0] in "'\"":
                value.append(part[1:-1])
                pattern.append(glob.escape(part[1:-1]))
            else:
                if any(char in part for char in "[]{}~#()"):
                    raise ValueError("only literal words and * or ? pathname patterns are supported")
                value.append(part)
                pattern.append(part)
                active |= "*" in part or "?" in part
        words.append(_ArchiveWord("".join(value), "".join(pattern) if active else None))
    if command[end:].strip(" \t"):
        raise ValueError("unsupported shell whitespace")
    return words


def _archive_operands(word: _ArchiveWord, cwd: Path, root: Path) -> list[str]:
    if word.pattern is None:
        return [word.value]
    parent, _, name = word.value.rpartition("/")
    pattern_parent, _, pattern = word.pattern.rpartition("/")
    if pattern_parent != glob.escape(parent):
        raise ValueError("wildcards in directory components are outside the archive subset")
    directory = (cwd / (parent or ".")).resolve()
    if not directory.is_relative_to(root):
        raise ValueError("archive glob escapes the fixture")
    matches = list(itertools.islice(
        glob.iglob(pattern, root_dir=directory), ARCHIVE_GLOB_MATCHES + 1,
    ))
    if len(matches) > ARCHIVE_GLOB_MATCHES:
        raise ValueError("archive glob match limit exceeded")
    prefix = word.value[:-len(name)]
    return [prefix + match for match in sorted(matches)] if matches else [word.value]


def _archive_command(parts: list[_ArchiveWord], root: Path, *, source: str,
                     destination: str | None, cwd: Path) -> tuple[list[str], Path, Path]:
    root = root.resolve()
    source_root = (root / source).resolve()
    target = (root / destination).resolve() if destination is not None else None
    if not source_root.is_relative_to(root) or target is not None and not target.is_relative_to(root):
        raise ValueError("archive operands escape the fixture")
    if target is not None and target.is_relative_to(source_root):
        raise ValueError("archive destination must be outside its source")
    if (not parts or parts[0].operator or parts[0].pattern
            or parts[0].value not in ("tar", "/usr/bin/tar", "/bin/tar")):
        raise ValueError("only tar with an optional cd is supported")
    if any(word.operator for word in parts):
        raise ValueError("additional shell commands are not allowed")
    args = parts[1:]
    if (not args or args[0].pattern
            or not (args[0].value.startswith("-") or re.fullmatch(r"[aczvf]+", args[0].value))):
        raise ValueError("tar must start with literal supported options")
    expanded = []
    created = zipped = auto_compress = end_options = False
    archive_name = None
    operands = []
    input_cwd = cwd
    i = 0
    while i < len(args):
        token = args[i]
        word = token.value
        if not end_options and (word.startswith("-") or i == 0 and re.fullmatch(r"[aczvf]+", word)):
            if token.pattern:
                raise ValueError("archive options must be literal")
            expanded.append(word)
            if word in ("--create", "--gzip", "--verbose"):
                created |= word == "--create"
                zipped |= word == "--gzip"
            elif word in ("--auto-compress", "--no-auto-compress"):
                auto_compress = word == "--auto-compress"
            elif word == "--":
                end_options = True
            elif word in ("--file", "--directory", "-C") or word.startswith(("--file=", "--directory=")):
                flag, separator, value = word.partition("=")
                if not separator:
                    i += 1
                    if i == len(args) or args[i].pattern:
                        raise ValueError("missing or nonliteral tar option argument")
                    value = args[i].value
                    expanded.append(value)
                if flag == "--file":
                    if archive_name is not None:
                        raise ValueError("multiple archive paths")
                    archive_name = value
                else:
                    input_cwd = (input_cwd / value).resolve()
                    if input_cwd != source_root:
                        raise ValueError("tar directory escapes the declared source")
            else:
                traditional = not word.startswith("-")
                flags = word if traditional else word[1:]
                if (not flags or any(c not in "aczvf" for c in flags) or flags.count("f") > 1
                        or (not traditional and "f" in flags and not flags.endswith("f"))):
                    raise ValueError(f"unsupported tar flags: {word}")
                created |= "c" in flags
                zipped |= "z" in flags
                auto_compress |= "a" in flags
                if "f" in flags:
                    i += 1
                    if i == len(args) or archive_name is not None or args[i].pattern:
                        raise ValueError("invalid archive path")
                    expanded.append(args[i].value)
                    archive_name = args[i].value
        else:
            # Shell globbing precedes tar's positional -C options.
            values = _archive_operands(token, cwd, root)
            if not end_options and any(value.startswith("-") for value in values):
                raise ValueError("archive glob expands to an option; an explicit -- is required")
            expanded.extend(values)
            operands.extend((input_cwd / value).resolve() for value in values)
        i += 1
    if auto_compress:
        if archive_name is None or not archive_name.endswith((".gz", ".tgz")):
            raise ValueError("auto-compression must select gzip from the archive filename")
        zipped = True
    if not created or not zipped or archive_name is None:
        raise ValueError("expected gzip archive creation with an output filename")
    if archive_name == "-" or ":" in archive_name:
        raise ValueError("archive output must be a local file, not stdout or a remote archive")
    archive = cwd / archive_name
    if target is None:
        if archive.name.startswith((".", "-")) or not archive.name.lower().endswith((".gz", ".tgz")):
            raise ValueError("a default archive needs a visible gzip archive filename (.tar.gz, .tgz or .gz)")
        if archive.is_symlink() or archive.exists() or archive.resolve().parent != root:
            raise ValueError("a default archive must name a new file in the working directory")
    elif archive.resolve() != target:
        raise ValueError(f"expected gzip creation at {destination}")
    archive = archive.resolve()
    if not operands or any(not path.is_relative_to(source_root) for path in operands):
        raise ValueError("only the declared source directory or its contents can be archived")
    if not source_root.is_dir():
        raise ValueError("archive source is not a directory")
    source_files = set()
    for directory, dirs, files in os.walk(source_root, followlinks=False):
        base = Path(directory)
        if any((base / name).is_symlink() for name in dirs + files):
            raise ValueError("archive source contains non-regular entries")
        for name in files:
            path = base / name
            if not path.is_file():
                raise ValueError("archive source contains non-regular entries")
            source_files.add(path)
    covered = Counter()
    for operand in operands:
        if operand.is_dir():
            covered.update(path for path in source_files if path.is_relative_to(operand))
        elif operand.is_file():
            covered[operand] += 1
        else:
            raise ValueError("archive operand is missing or non-regular")
    if covered != Counter({path: 1 for path in source_files}):
        raise ValueError("archive operands must cover each source file exactly once")
    return ["tar", *expanded], cwd, archive


def _program_name(word: _ArchiveWord) -> str | None:
    if word.operator or word.pattern:
        return None
    path = PurePosixPath(word.value)
    if "/" in word.value and path.parent not in (PurePosixPath("/bin"), PurePosixPath("/usr/bin")):
        return None
    return path.name


def help_command_reasons(answer: str) -> list[str]:
    if reasons := shell_syntax_reasons(answer):
        return reasons
    try:
        words = _archive_words(answer.strip())
    except ValueError as exc:
        return [str(exc)]
    if not words or any(word.operator or word.pattern for word in words):
        return ["expected one command that displays tar help"]
    program, args = _program_name(words[0]), [word.value for word in words[1:]]
    if (program == "tar" and args in (["--help"], ["--usage"])
            or program in ("man", "info") and args == ["tar"]):
        return []
    return ["the generated command must display tar help, not perform another task"]


def _diagnostic_command(group: list[_ArchiveWord], root: Path, cwd: Path, *,
                        source: str, destination: str) -> tuple[list[str], Path] | None:
    program = _program_name(group[0])
    if program not in ("ls", "stat"):
        return None
    related = {root, (root / source).resolve(), (root / destination).parent.resolve()}
    args = []
    paths = []
    end_options = False
    for word in group[1:]:
        if word.operator or word.pattern:
            raise ValueError("diagnostic paths must be literal")
        value = word.value
        if not end_options and value == "--":
            end_options = True
        elif not end_options and value.startswith("-"):
            allowed = (bool(re.fullmatch(r"-[alAdhF]+", value)) if program == "ls"
                       else value in ("-L", "--dereference"))
            if not allowed:
                raise ValueError("unsupported diagnostic option")
        else:
            path = (cwd / value).resolve()
            if not value or path not in related or not path.is_dir():
                raise ValueError("diagnostic must inspect the current, source or backup directory")
            paths.append(path)
        args.append(value)
    if not paths and (program != "ls" or cwd not in related):
        raise ValueError("diagnostic needs a related directory")
    return [program, *args], cwd


def _archive_program(
    words: list[_ArchiveWord], root: Path, *, source: str, destination: str | None,
    prepare_directory: str | None = None, allow_diagnostics: bool = False,
    allow_prepare_only: bool = False,
) -> tuple[list[tuple[list[str], Path]], tuple[list[str], Path] | None, Path | None]:
    groups, group = [], []
    for word in words:
        if word.operator:
            if word.value not in ("&&", ";", "\n") or not group:
                raise ValueError("unsupported archive program operator")
            groups.append(group)
            group = []
        else:
            group.append(word)
    if group:
        groups.append(group)
    elif words and words[-1].value == "&&":
        raise ValueError("incomplete archive program")
    if len(groups) > ARCHIVE_PROGRAM_COMMANDS:
        raise ValueError("archive program command limit exceeded")
    root = root.resolve()
    if allow_diagnostics and (
        destination is None
        or not (root / source).resolve().is_relative_to(root)
        or not (root / destination).parent.resolve().is_relative_to(root)
    ):
        raise ValueError("diagnostic source or backup directory escapes the fixture")
    cwd = root
    archive: tuple[list[str], Path] | None = None
    archive_path: Path | None = None
    prepared = False
    diagnostic = False
    program: list[tuple[list[str], Path]] = []
    for group in groups:
        values = [word.value for word in group]
        if values[0] in ("mkdir", "/bin/mkdir", "/usr/bin/mkdir"):
            if (allow_diagnostics and archive is None and len(values) == 3
                    and values[1] in ("-p", "--parents") and not any(word.pattern for word in group)
                    and (cwd / values[2]).resolve() == (root / source).resolve()
                    and (root / source).is_dir()):
                program.append((["mkdir", *values[1:]], cwd))
                continue
            if prepare_directory is None or prepared or archive is not None or any(word.pattern for word in group):
                raise ValueError("only the declared archive parent directory may be prepared")
            args = values[1:]
            while args and args[0] in ("-p", "--parents"):
                args = args[1:]
            if args[:1] == ["--"]:
                args = args[1:]
            if (len(args) != 1 or not args[0] or
                    (cwd / args[0]).resolve() != root / prepare_directory):
                raise ValueError("only the declared archive parent directory may be prepared")
            prepared = True
            program.append((["mkdir", *values[1:]], cwd))
        elif values[0] == "cd":
            directory = group[1:]
            if directory and directory[0].value == "--":
                directory = directory[1:]
            if len(directory) != 1 or not directory[0].value or directory[0].pattern:
                raise ValueError("unsupported cd arguments")
            cwd = (cwd / directory[0].value).resolve()
            if (cwd not in (root, (root / source).resolve()) or not cwd.is_dir()
                    or not os.access(cwd, os.X_OK)):
                raise ValueError("cd escapes the archive fixture or names an inaccessible directory")
        elif (len(group) == 2 and values[0] in ("tar", "/usr/bin/tar", "/bin/tar")
              and values[1] in ARCHIVE_INFO_FLAGS and all(word.pattern is None for word in group)):
            program.append((["tar", values[1]], cwd))
            diagnostic = True
        elif allow_diagnostics and destination is not None and (command := _diagnostic_command(
                group, root, cwd, source=source, destination=destination)) is not None:
            program.append(command)
            diagnostic = True
        else:
            if archive is not None:
                raise ValueError("only one archive creation is allowed")
            if prepare_directory and not prepared and not (root / prepare_directory).is_dir():
                raise ValueError("the repair must prepare the missing archive parent directory")
            args, archive_cwd, archive_path = _archive_command(
                group, root, source=source, destination=destination, cwd=cwd,
            )
            archive = args, archive_cwd
            program.append(archive)
    if archive is None or archive_path is None:
        if not (allow_prepare_only and prepared or allow_diagnostics and diagnostic):
            if allow_prepare_only:
                raise ValueError("the repair must prepare the missing archive parent directory")
            raise ValueError("an archive creation command or supported related diagnostic is required"
                             if allow_diagnostics else "an archive creation command is required")
    return program, archive, archive_path


def archive_command(command: str, root: Path, *, source: str = "logs",
                    destination: str | None = "logs.tar.gz") -> tuple[list[str], Path]:
    _, archive, _ = _archive_program(_archive_words(command.strip()), root,
                                     source=source, destination=destination)
    assert archive is not None
    return archive


def shell_syntax_reasons(command: str) -> list[str]:
    syntax = subprocess.run(["bash", "--noprofile", "--norc", "-n", "-c", command],
                            env={"PATH": "/usr/bin:/bin"}, capture_output=True, text=True, timeout=5)
    return ["invalid shell syntax: " + syntax.stderr.strip()] if syntax.returncode else []


def check_archive(answer: str, root: Path, before: dict, after: dict, *,
                  source: str = "logs", destination: str | None = "logs.tar.gz",
                  prepare_directory: str | None = None, allow_diagnostics: bool = False) -> list[str]:
    if after != before:
        return ["nosh -s modified the fixture instead of only suggesting"]
    if reasons := shell_syntax_reasons(answer):
        return reasons
    try:
        words = _archive_words(answer.strip())
    except ValueError as exc:
        return [str(exc)]
    return _check_archive(words, root, before, source=source, destination=destination,
                          prepare_directory=prepare_directory, allow_diagnostics=allow_diagnostics)


def _map_archive_words(words: list[_ArchiveWord], root: Path, copied: Path) -> list[_ArchiveWord]:
    mapped = []
    end_options = False
    for word in words:
        if word.operator:
            end_options = False
        elif word.value == "--":
            end_options = True
        flag, separator, value = word.value.partition("=")
        option_value = not end_options and separator and flag in ("--file", "--directory")
        path = Path(value if option_value else word.value)
        if not word.operator and path.is_absolute() and path.is_relative_to(root):
            replacement = str(copied / path.relative_to(root))
            pattern = word.pattern
            if pattern is not None:
                prefix = glob.escape(str(root)) + "/"
                if not pattern.startswith(prefix):
                    raise ValueError("archive glob has a nonliteral fixture prefix")
                pattern = glob.escape(str(copied)) + "/" + pattern[len(prefix):]
            word = replace(word, value=flag + "=" + replacement if option_value else replacement,
                           pattern=pattern)
        mapped.append(word)
    return mapped


def _copy_archive_file(source, destination):
    if Path(source).stat().st_nlink != 1:
        raise ValueError("hard-linked files are outside the archive verification subset")
    return shutil.copy2(source, destination)


def _check_archive(words: list[_ArchiveWord], root: Path, before: dict, *,
                   source: str, destination: str | None, prepare_directory: str | None = None,
                   allow_diagnostics: bool = False, allow_prepare_only: bool = False) -> list[str]:
    try:
        with tempfile.TemporaryDirectory(prefix="nosh-archive-verifier-") as temporary:
            copied = Path(temporary) / "files"
            shutil.copytree(root, copied, symlinks=True, copy_function=_copy_archive_file)
            mapped = _map_archive_words(words, root.resolve(), copied)
            program, _, archive_path = _archive_program(
                mapped, copied, source=source, destination=destination, prepare_directory=prepare_directory,
                allow_diagnostics=allow_diagnostics, allow_prepare_only=allow_prepare_only,
            )
            deadline = time.monotonic() + 10
            for args, cwd in program:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise subprocess.TimeoutExpired(args, 10)
                proc = subprocess.run(args, cwd=cwd, env={"PATH": "/usr/bin:/bin", "LC_ALL": "C.UTF-8"},
                                      stdin=subprocess.DEVNULL, capture_output=True, text=True,
                                      timeout=remaining)
                if proc.returncode:
                    return ["archive program command failed: " + proc.stderr.strip()]
            if archive_path is None:
                if allow_prepare_only:
                    if prepare_directory is None or not (copied / prepare_directory).is_dir():
                        return ["repair did not create the missing archive parent directory"]
                    if fixtures.snapshot(copied) != before:
                        return ["directory repair changed existing files"]
                return []
            prefix = source.rstrip("/") + "/"
            expected = {}
            for name, item in before.items():
                if name.startswith(prefix):
                    if "symlink" in item or not isinstance(item.get("sha256"), str):
                        return ["archive source contains non-regular entries"]
                    expected[name.removeprefix(prefix)] = item["sha256"]
            actual = {}
            with tarfile.open(archive_path, "r:gz") as stream:
                for member in stream:
                    if member.isdir():
                        continue
                    if not member.isfile():
                        return ["archive contains non-regular entries"]
                    member_path = PurePosixPath(member.name)
                    if member_path.is_absolute() or ".." in member_path.parts:
                        return ["archive contains an escaping member path"]
                    name = member_path.as_posix()
                    if name in actual:
                        return ["archive contains duplicate files"]
                    import hashlib

                    with stream.extractfile(member) as content:
                        actual[name] = hashlib.file_digest(content, "sha256").hexdigest()
            if actual == expected:
                return []
            for member_prefix in (prefix, str(copied / source).lstrip("/") + "/"):
                if all(name.startswith(member_prefix) for name in actual):
                    rooted = {name.removeprefix(member_prefix): value for name, value in actual.items()}
                    if rooted == expected:
                        return []
            return ["archive members or contents do not match the source"]
    except (ValueError, tarfile.TarError, OSError, subprocess.TimeoutExpired) as exc:
        return [f"archive validation failed: {exc}"]


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
    try:
        words = _archive_words(answer.strip())
    except ValueError as exc:
        return [str(exc)]
    return _check_archive(words, root, expected, source="archive",
                          destination="backups/reports.tar.gz", prepare_directory="backups",
                          allow_prepare_only=True)

def retry_archive_judgment(scenario, answer, facts, root, after, evidence):
    if after != facts["before"] or not (root / "backups").is_dir():
        return ["only the user's prerequisite directory creation may change the fixture"]
    accepted = (evidence or {}).get("assistance") or []
    actual = accepted[0] if len(accepted) == 1 else {}
    execution = actual.get("execution")
    history = actual.get("recent_executions")
    if (not isinstance(execution, dict) or not isinstance(history, list)
            or execution.get("command") != scenario["inputs"][-1]
            or type(execution.get("exit")) is not int or execution["exit"] != 0
            or execution.get("execution_cwd") != str(root)
            or not any(isinstance(item, dict) and item.get("command") == scenario["inputs"][1]
                       and type(item.get("exit")) is int and item["exit"] != 0
                       and item.get("execution_cwd") == str(root) for item in history)):
        return ["the prerequisite and original failed archive must be bound to real execution history"]
    return check_archive(answer, root, facts["before"], after, source="archive",
                         destination="backups/reports.tar.gz", prepare_directory="backups", allow_diagnostics=True)


def assistance_judgment(scenario, answer, facts, root, after, evidence):
    kind = scenario["check"]
    reasons = []
    directories = facts.get("before_directories")
    if not isinstance(directories, list) or not all(isinstance(name, str) for name in directories):
        raise ValueError("assistance requires the initial directory snapshot")
    expected_directories = set(directories)
    if kind == "assist-retry-archive":
        expected_directories.add("backups")
    if set(fixtures.directory_snapshot(root)) != expected_directories:
        reasons.append("unexpected directory changes outside the declared user operations")
    expected = scenario["assistance"]
    accepted = (evidence or {}).get("assistance") or []
    if len(accepted) != 1:
        reasons.append("expected exactly one observed assistance result")
    else:
        actual = accepted[0]
        if (actual.get("status") != "completed" or actual.get("intent") != expected["intent"]
                or actual.get("kind") != expected["result"] or actual.get("background") != expected["automatic"]):
            reasons.append("assistance result does not match the declared intent and result")
    allowed_tools = {"command_help", "read_file", "grep"}
    if any(call["name"] not in allowed_tools
           for call in (evidence or {}).get("tool_calls") or []):
        reasons.append("assistance attempted an unavailable task-execution tool")
    if kind == "assist-help":
        reasons.extend(help_command_reasons(answer))
    elif kind == "assist-archive":
        reasons.extend(check_archive(answer, root, facts["before"], after))
    elif kind == "assist-archive-default-name":
        reasons.extend(check_archive(answer, root, facts["before"], after, destination=None))
    elif kind == "assist-resume-archive":
        reasons.extend(resume_archive_judgment(scenario, answer, facts, root, after, evidence))
    elif kind == "assist-retry-archive":
        reasons.extend(retry_archive_judgment(scenario, answer, facts, root, after, evidence))
    elif kind == "assist-none" and answer:
        reasons.append("no-suggestion result must not contain a command or prose")

    return reasons
