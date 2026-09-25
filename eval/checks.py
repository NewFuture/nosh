"""Deterministic, deliberately conservative oracles; no model-based judging."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import shlex
import subprocess
import tarfile

from .driver import PROMPT

FILE_NAME = re.compile(r"(?<![\w.-])(?:[\w.-]+/)*[\w.-]+\.(?:bin|txt|md|py|js|rs|sh)(?![\w-]|\.\w)")
LANGUAGES = {
    "python": r"\bpython\b",
    "javascript": r"\b(?:javascript|js)\b",
    "rust": r"\brust\b",
    "shell": r"\b(?:shell|bash|sh)\b",
}


@dataclass
class Verdict:
    passed: bool
    reasons: list[str]


def shell_parts(command: str) -> list[str]:
    if any(ch in command for ch in "$`<>\\\0"):
        raise ValueError("shell expansion/redirection is outside the allowed subset")
    lex = shlex.shlex(command, posix=True, punctuation_chars=";&|")
    lex.whitespace_split = True
    lex.commenters = ""
    return list(lex)


def allow_approval(policy: str, command: str, root: Path, facts: dict) -> bool:
    if policy == "rename":
        command = command.strip()
        if command.startswith("cd "):
            prefix, separator, rest = command.partition("&&")
            try:
                parts = shell_parts(prefix)
            except ValueError:
                return False
            directory = parts[1:]
            if directory[:1] == ["--"]:
                directory = directory[1:]
            if not separator or len(directory) != 1 or (root / directory[0]).resolve() != root:
                return False
            command = rest.strip()
        command = re.sub(r"\s*&&\s*ls(?:\s+(?:-la|-al|-l|-a|--))*\s*$", "", command)
        # Only this bounded loop is allowed; substituted shell expressions or
        # extra commands must not ride along with a fixture approval.
        loop = re.fullmatch(
            r'for ([a-zA-Z_]\w*) in (?:\./)?\*\.txt;\s*do\s+mv(?: -v)?(?: --)? '
            r'"\$\1" "\$\{\1%(?:\.txt|\.\*)\}\.md";?\s*done;?',
            command.strip(),
        )
        if loop:
            return True
        try:
            parts = shell_parts(command)
        except ValueError:
            return False
        groups, group = [], []
        for part in parts:
            if part in (";", "&&"):
                if not group:
                    return False
                groups.append(group)
                group = []
            else:
                group.append(part)
        if group:
            groups.append(group)
        for group in groups:
            if not group or group[0] != "mv":
                return False
            args = group[1:]
            while args and args[0] in ("-v", "--"):
                args = args[1:]
            if len(args) != 2 or facts["renames"].get(args[0]) != args[1]:
                return False
        return bool(groups)
    if policy == "cwd":
        try:
            parts = shell_parts(command)
        except ValueError:
            return False
        if not parts or parts.pop(0) != "cd":
            return False
        if parts and parts[0] == "--":
            parts.pop(0)
        if not parts:
            return False
        target = parts.pop(0)
        if (root / target).resolve() != root / "data":
            return False
        if not parts:
            return True
        if len(parts) < 2 or parts[0] not in ("&&", ";") or parts[1] != "ls":
            return False
        return all(arg in (".", "-a", "-l", "-la", "-al", "-lh", "-lah", "--") for arg in parts[2:])
    return False


def mentioned_files(answer: str, known: dict | list) -> list[str]:
    result = []
    for match in FILE_NAME.finditer(answer):
        name = match.group().strip("/")
        matches = [p for p in known if name == p or name.endswith("/" + p)]
        if not matches:
            matches = [p for p in known if Path(p).name == name]
        canonical = matches[0] if len(matches) == 1 else name
        if canonical not in result:
            result.append(canonical)
    return result


def line_counts(answer: str, facts: dict) -> list[str]:
    reasons = []
    found: dict[str, list[int]] = {name: [] for name in LANGUAGES}
    sections: dict[str, list[dict]] = {name: [] for name in LANGUAGES}
    line_count = r"(?<![\d.-])(\d+)\s*(?:lines?\b|loc\b|行)"
    language_label = "|".join(LANGUAGES.values())
    columns = None
    file_column = None
    section = None
    scope = None
    for raw in answer.lower().splitlines():
        line = raw.replace("**", "").replace("`", "")
        matched_languages = [name for name, pattern in LANGUAGES.items() if re.search(pattern, line)]
        is_total = bool(re.search(r"\b(?:total|overall|altogether)\b|总计|合计|一共|总共|共有", line))
        heading = line.rstrip().endswith(":") or bool(re.match(r"^\s*(?:#{1,6}\s|\*\*)", raw))
        if heading and not is_total:
            section = matched_languages[0] if len(matched_languages) == 1 else None
            scope = {"files": set(), "lines": [], "counts": []} if section else None
            if scope is not None:
                sections[section].append(scope)
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        if len(cells) > 1:
            count_columns = [i for i, c in enumerate(cells)
                             if re.search(r"\blines?\b|\bloc\b|行数|代码行", c)]
            if count_columns and not any(re.search(p, line) for p in LANGUAGES.values()):
                columns = count_columns[0]
                file_column = next((i for i, c in enumerate(cells) if re.search(r"\bfiles?\b|文件", c)), None)
                section = None
                scope = None
        if scope is not None:
            scope["files"].update(mentioned_files(line, facts["before"]))
        for language, pattern in LANGUAGES.items():
            label = re.search(pattern, line)
            if not label:
                continue
            number = None
            if columns is not None and len(cells) > columns and re.fullmatch(r"\d+", cells[columns]):
                number = int(cells[columns])
            else:
                description = re.split(language_label, line[label.end():], maxsplit=1)[0]
                match = re.search(line_count, description)
                if not match:
                    match = re.fullmatch(r"\s*[:：-]\s*(\d+)\s*[.,;。]?\s*", description)
                if match:
                    number = int(match.group(1))
            if number is not None:
                found[language].append(number)
            if file_column is not None and len(cells) > file_column and cells[file_column].isdigit():
                if int(cells[file_column]) != facts["language_files"][language]:
                    reasons.append(f"incorrect {language} file count: {cells[file_column]}")
        if is_total and not matched_languages:
            if re.search(r"\b(?:overall|all languages|grand total)\b|总计|总共|全部", line):
                section = None
                scope = None
            counts = [int(number) for number in re.findall(line_count, line)]
            if columns is not None and len(cells) > columns and re.fullmatch(r"\d+", cells[columns]):
                counts.append(int(cells[columns]))
            files = [int(number) for number in re.findall(r"(\d+)\s*(?:files?\b|个文件|文件)", line)]
            if scope is not None:
                scope["lines"].extend(counts)
                scope["counts"].extend(files)
            else:
                reasons.extend(f"incorrect total line count: {number}" for number in counts
                               if number != facts["total"])
                reasons.extend(f"incorrect total file count: {number}" for number in files
                               if number != facts["file_count"])
    suffixes = {"python": ".py", "javascript": ".js", "rust": ".rs", "shell": ".sh"}
    for language, scopes in sections.items():
        if len(scopes) > 1 and any(item["lines"] or item["counts"] for item in scopes):
            files = [name for item in scopes for name in item["files"]]
            expected_files = {name for name in facts["before"] if Path(name).suffix == suffixes[language]}
            # Add subsection totals only when their named files form a disjoint,
            # complete partition; repeated or omitted scopes must not be hidden.
            if (len(files) != len(set(files)) or set(files) != expected_files
                    or any(not item["files"] or len(item["lines"]) != 1 for item in scopes)):
                reasons.append(f"ambiguous or incomplete {language} subtotal scopes")
                found[language].extend(number for item in scopes for number in item["lines"])
            else:
                found[language].append(sum(item["lines"][0] for item in scopes))
            for item in scopes:
                reasons.extend(f"incorrect {language} subtotal file count: {number}"
                               for number in item["counts"] if number != len(item["files"]))
        else:
            for item in scopes:
                found[language].extend(item["lines"])
                reasons.extend(f"incorrect total line count: {number}" for number in item["lines"]
                               if number != facts["languages"][language])
                reasons.extend(f"incorrect total file count: {number}" for number in item["counts"]
                               if number != facts["language_files"][language])
    for language, expected in facts["languages"].items():
        if not found[language]:
            reasons.append(f"no unambiguous {language} line count")
        elif any(value != expected for value in found[language]):
            reasons.append(f"{language}: expected {expected}, found {found[language]}")
    return reasons


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
            elif (cwd / value).resolve() != root / "logs":
                raise ValueError("tar directory escapes logs")
        elif word.startswith("-") and word != "--":
            flags = word[1:]
            if not flags or any(c not in "czvf" for c in flags) or ("f" in flags and not flags.endswith("f")):
                raise ValueError(f"unsupported tar flags: {word}")
            created |= "c" in flags
            zipped |= "z" in flags
            if "f" in flags:
                i += 1
                if i == len(args) or archive is not None:
                    raise ValueError("invalid archive path")
                archive = (cwd / args[i]).resolve()
        elif word == "--":
            operands.extend(args[i + 1:])
            break
        else:
            operands.append(word)
        i += 1
    if not created or not zipped or archive != root / "logs.tar.gz":
        raise ValueError("expected gzip creation at logs.tar.gz")
    if not operands or any(p not in (".", "./", "logs", "logs/", "./logs", "./logs/") for p in operands):
        raise ValueError("only the fixture logs directory can be archived")
    # A bare '.' at the fixture root would include unrelated files/the archive.
    if cwd == root and any(p in (".", "./") for p in operands) and not any(
        p in ("-C", "--directory", "--directory=logs") for p in args
    ):
        raise ValueError("'.' must refer to logs, not its parent")
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


def judge(scenario: dict, answer: str, facts: dict, root: Path, after: dict, result, metrics: dict) -> Verdict:
    kind = scenario["check"]
    reasons = []
    if result.exit_code != 0:
        reasons.append(f"nosh exit code: {result.exit_code}")
    if metrics.get("task_status") not in ("completed", "local"):
        reasons.append(f"task did not complete: {metrics.get('task_status')}")
    if kind == "largest":
        ranked = []
        reference = False
        for line in answer.splitlines():
            if (re.match(r"^\s*(?:\d+[.)]\s+|[-*]\s+|\|)", line.replace("**", ""))
                    and FILE_NAME.search(line)):
                if not reference or re.match(r"^\s*\d+[.)]\s+", line):
                    ranked.append(line)
            elif ranked and line.strip():
                # Only an explicitly smaller-file reference section is outside
                # the ranking. Other prose must not hide corrected/additional ranks.
                if not line.lstrip().startswith("|"):
                    reference = bool(re.match(
                        r"^\s*(?:for reference\b.*\bsmaller files?\b|(?:other\s+)?smaller files?\b).*:\s*$",
                        line.replace("**", ""), re.I,
                    ))
        names = mentioned_files("\n".join(ranked) if ranked else answer, facts["before"])
        if names != facts["largest"]:
            reasons.append(f"expected ordered top three {facts['largest']}, found {names}")
    elif kind == "port":
        if not re.search(rf"(?<!\d){facts['listener']['pid']}(?!\d)", answer):
            reasons.append("answer does not identify the fixture PID")
        if not re.search(r"\bpython[ \t]*3(?:\.\d+)?\b", answer, re.I):
            reasons.append("answer does not identify the Python listener")
    elif kind == "lines":
        reasons.extend(line_counts(answer, facts))
    elif kind == "rename":
        expected = {facts["renames"].get(name, name): value for name, value in facts["before"].items()}
        if after != expected:
            reasons.append("renamed file set or contents do not match; readme.md must be preserved")
        if not any(a["allowed"] for a in result.approvals):
            reasons.append("no actual rename approval was observed")
    elif kind == "python":
        names = [name for name in mentioned_files(answer, facts["before"]) if name.endswith(".py")]
        python_section = True
        classified_python = set()
        incorrectly_classified = []
        misclassified_python = []
        for line in answer.splitlines():
            if re.search(r"(?:non[- ]|not\s+)python|(?:非|不是|不属于)\s*python|(?:其他|其余).*文件|other.*files", line, re.I):
                python_section = False
            elif re.search(r"python\s*文件|python\s*files|个\s*python", line, re.I):
                python_section = True
            elif re.fullmatch(
                r"目录|目录结构|项目结构|文件树|directory|directory structure|directory listing|project structure|file tree",
                line.strip().strip("*# ").rstrip(":："), re.I,
            ):
                python_section = None
            line_files = mentioned_files(line, facts["before"])
            if python_section is True:
                classified_python.update(name for name in line_files if name.endswith(".py"))
                incorrectly_classified.extend(name for name in line_files if not name.endswith(".py"))
            elif python_section is False:
                misclassified_python.extend(name for name in line_files if name.endswith(".py"))
        if (set(names) != set(facts["python"]) or classified_python != set(facts["python"])
                or incorrectly_classified or misclassified_python):
            reasons.append(f"expected Python files {facts['python']}, found {names}")
            if incorrectly_classified:
                reasons.append(f"non-Python files classified as Python: {incorrectly_classified}")
            if misclassified_python:
                reasons.append(f"Python files classified as non-Python: {misclassified_python}")
        if not re.search(r"[\u4e00-\u9fff]", answer):
            reasons.append("answer is not in Chinese")
        counts = re.findall(r"(\d+)\s*(?:个\s*)?python\s*(?:文件|files?\b)",
                            answer.replace("**", "").replace("`", ""), re.I)
        if any(int(count) != len(facts["python"]) for count in counts):
            reasons.append(f"incorrect Python file count: {counts}")
    elif kind == "typos":
        expected = scenario["corrections"]
        if len(result.turns) != len(expected) or any(
            not turn["edit_line"].startswith(PROMPT + corrected)
            or turn["edit_line"][len(PROMPT + corrected):].strip() not in ("", "confirm")
            for turn, corrected in zip(result.turns, expected)
        ):
            reasons.append("corrected commands were not left in the editable input line")
        if metrics["steps"] != 0:
            reasons.append("local spelling correction invoked the model")
        if any(re.search(r"On branch|nothing to commit|Python \d+\.\d+", turn["output"]) for turn in result.turns):
            reasons.append("a corrected command executed without Enter")
    elif kind == "failure":
        if not result.turns or "FileNotFoundError" not in result.turns[0]["output"]:
            reasons.append("the intended missing-file failure was not observed")
        if "config.json" not in answer:
            reasons.append("answer omits config.json")
        if not re.search(r"missing|not found|not exist|doesn't exist|no such file|FileNotFound|缺少|缺失|不存在|找不到|未找到", answer, re.I):
            reasons.append("answer does not explain the missing file")
        if not re.search(
            r"\b(?:create|provide|add|copy|place|put|correct|fix|change|set|specify|modify)\b"
            r"|创建|提供|添加|复制|放入|放到|放置|修正|修改|设置|指定|切换到|在正确目录(?:中)?运行",
            answer, re.I,
        ):
            reasons.append("answer provides no recognized remedy")
    elif kind == "history":
        aliases = {"pipeline": r"pipeline|管道", "approval": r"approv|确认|审批",
                   "seed": r"seed|种子", "checksum": r"checksum|sha.?256|校验",
                   "truncate": r"truncat|截断", "suggest": r"suggest|建议",
                   "timeout": r"time.?out|超时", "offline": r"offline|离线"}
        blocks = []
        for paragraph in re.split(r"\n\s*\n", answer):
            if "|" in paragraph:
                blocks.extend(paragraph.splitlines())
            else:
                blocks.extend(re.split(r"(?m)^(?=(?:\d+[.)]|[-*])\s)", paragraph))
        components = [component for component, _ in facts["history"]]
        for component, feature in facts["history"]:
            if not any(re.search(rf"\b{component}\b", block, re.I) and re.search(aliases[feature], block, re.I)
                       and sum(bool(re.search(rf"\b{c}\b", block, re.I)) for c in components) == 1
                       for block in blocks):
                reasons.append(f"missing or unassociated commit fact: {component}/{feature}")
    elif kind == "archive":
        reasons.extend(check_archive(answer, root, facts["before"], after))
    elif kind == "cwd":
        if result.pwd != str(root / "data"):
            reasons.append(f"physical pwd is {result.pwd!r}, not data")
        names = mentioned_files(answer, facts["before"])
        if not all(any(Path(name).name == expected for name in names) for expected in facts["data_files"]):
            reasons.append("answer does not list both data files")
    else:
        raise ValueError(f"unknown check: {kind}")
    if kind not in ("rename", "archive") and after != facts["before"]:
        reasons.append("unexpected fixture changes")
    return Verdict(not reasons, reasons)
