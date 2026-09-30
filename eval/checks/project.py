"""Deterministic project scoring; no model judge."""

from __future__ import annotations
from pathlib import Path
import re
from .. import driver
from ..approval import PROJECT_POLICIES, command_groups, project_actions, shell_parts
from ..fixtures import protected_files
from .common import has_affirmative_match, mentioned_files, response_prose
from .experience import clarification_request
from .git import git_judgment


VERSION_FLAGS = {
    "cargo": {"--version", "-V", "version"},
    "node": {"--version", "-v"},
    "python3": {"--version", "-V"},
}


VERSION_ALIASES = {"cargo": r"cargo", "node": r"node(?:\.js)?", "python3": r"python3?"}


def version_queries(command: str, root: Path, facts: dict) -> set[str]:
    """Recognize version-query evidence, without extending any approval policy."""
    segments = []
    current = []
    quote = operator = None
    i = 0
    while i < len(command):
        char = command[i]
        if quote is not None:
            if char == quote:
                quote = None
        elif char in ("'", '"'):
            quote = char
        elif (i > 0 and command[i - 1].isspace()
              and (redirect := re.match(r"2(?:>&1|>[ \t]*/dev/null)(?=\s|&&|\|\||;|$)", command[i:]))):
            current.append(" ")
            i += redirect.end()
            continue
        elif char in ";&|":
            separator = command[i:i + 2] if command[i:i + 2] in ("&&", "||") else char
            if separator not in (";", "&&", "||") or not "".join(current).strip():
                raise ValueError("unsupported version query operator")
            segments.append((operator, "".join(current).strip()))
            current = []
            operator = separator
            i += len(separator)
            continue
        current.append(char)
        i += 1
    if quote is not None:
        raise ValueError("unterminated version query quote")
    if "".join(current).strip():
        segments.append((operator, "".join(current).strip()))
    elif operator != ";":
        raise ValueError("incomplete version query")
    if not segments:
        raise ValueError("missing version query")
    if shell_parts(segments[0][1])[:1] == ["cd"]:
        if len(segments) < 2 or segments[1][0] != "&&":
            raise ValueError("version query must follow a successful cd")
        segments[1] = (None, segments[0][1] + " && " + segments[1][1])
        segments.pop(0)
    queries = set()
    may_fallback = False
    for operator, segment in segments:
        if operator == "||":
            parts = shell_parts(segment)
            if not may_fallback or parts[:1] != ["echo"] or len(parts) < 2:
                raise ValueError("unsupported version query fallback")
            if any(version in " ".join(parts[1:]) for version in facts["versions"].values()):
                raise ValueError("fallback text cannot supply version evidence")
            may_fallback = False
            continue
        groups = command_groups(segment, root, facts.get("tools"))
        if (len(groups) != 1 or len(groups[0]) != 2 or groups[0][0] not in VERSION_FLAGS
                or groups[0][1] not in VERSION_FLAGS[groups[0][0]]):
            raise ValueError("expected a version-only query")
        queries.add(groups[0][0])
        may_fallback = True
    return queries


def version_claims(answer: str) -> dict[str, list[str]]:
    claims: dict[str, list[str]] = {name: [] for name in VERSION_ALIASES}
    version = r"(?<![a-z0-9_.])v?(\d+(?:\.\d+)+)(?![a-z0-9_]|\.\d)"
    label = r"(?<![a-z0-9_])(?:" + "|".join(VERSION_ALIASES.values()) + r")(?![a-z0-9_])"
    horizontal = {}
    version_column = None
    pending = None

    def tool(text):
        return next((name for name, alias in VERSION_ALIASES.items()
                     if re.fullmatch(rf"\s*(?:{alias})(?:\s*(?:version|版本))?\s*[:：]?\s*", text, re.I)), None)

    def record(name, text, *, required=False):
        text = re.sub(r"(?:[<>]=?|[≥≤])\s*v?\d+(?:\.\d+)+", "", text, flags=re.I)
        values = re.findall(version, text, re.I)
        if ((required and not values)
                or re.search(r"\b(?:unknown|unavailable|not found|not installed|n/a)\b|未知|未安装|未找到", text, re.I)):
            values.append("unavailable")
        claims[name].extend(values)
        return bool(values)

    for raw in answer.splitlines():
        line = raw.replace("**", "").replace("__", "").replace("`", "").strip()
        if not line:
            horizontal, version_column = {}, None
            continue
        if "|" in line:
            pending = None
            cells = [cell.strip() for cell in line.strip("|").split("|")]
            if not any(cells) or all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells):
                continue
            columns = {index: name for index, cell in enumerate(cells) if (name := tool(cell)) is not None}
            if columns and (len(columns) > 1 or len(cells) == 1) and not re.search(version, line, re.I):
                horizontal, version_column = columns, None
                continue
            value_column = next((i for i, cell in enumerate(cells)
                                 if re.fullmatch(r"(?:installed\s+)?version|(?:当前)?版本", cell, re.I)), None)
            if value_column is not None and not columns:
                horizontal, version_column = {}, value_column
                continue
            if horizontal:
                for index, name in horizontal.items():
                    record(name, cells[index] if index < len(cells) else "", required=True)
                continue
            if len(columns) == 1:
                index, name = next(iter(columns.items()))
                if version_column is None:
                    value = " ".join(cell for i, cell in enumerate(cells) if i != index)
                else:
                    value = cells[version_column] if version_column < len(cells) else ""
                record(name, value, required=True)
                continue
        else:
            horizontal, version_column = {}, None
        labels = list(re.finditer(label, line, re.I))
        if not labels and pending is not None and re.fullmatch(version, line.strip("- :：。."), re.I):
            record(pending, line)
        pending = None
        for index, match in enumerate(labels):
            name = tool(match[0])
            description = line[match.end():labels[index + 1].start() if index + 1 < len(labels) else len(line)]
            if not record(name, description) and not description.strip(" :：=-"):
                pending = name
    return claims


def completed_commands(evidence: dict, root: Path, facts: dict, action: str, after: dict) -> list[dict]:
    if protected_files(after, action) != protected_files(facts["before"], action):
        return []
    matches = []
    for execution in evidence.get("executions") or []:
        call = execution["call"]
        if (call["name"] != "exec" or execution.get("exit_code") != 0
                or execution.get("timed_out") or execution.get("interrupted")):
            continue
        command = call["args"].get("command")
        if not isinstance(command, str):
            continue
        if action in project_actions(action, command, root, facts):
            matches.append(execution)
    return matches


def project_judgment(scenario: dict, answer: str, facts: dict, root: Path, after: dict, result, evidence: dict) -> list[str]:
    kind = scenario["check"]
    reasons = []
    if protected_files(after, kind) != protected_files(facts["before"], kind):
        reasons.append("source, tests, configuration or unexpected paths changed")
    commands = completed_commands(evidence, root, facts, kind, after) if kind in PROJECT_POLICIES else []
    if kind in PROJECT_POLICIES:
        if not commands:
            reasons.append(f"no successfully executed {kind} command was observed")
    if kind == "rust-build":
        binaries = [root / "target" / profile / "eval_math" for profile in ("debug", "release")]
        binaries = [p for p in binaries if p.is_file() and not p.is_symlink()]
        if not binaries:
            reasons.append("Rust build produced no executable")
        elif not reasons:
            for binary in binaries:
                proc = driver.run_cli([str(binary)], root, {"PATH": "/usr/bin:/bin"}, 5)
                if proc.error:
                    raise driver.DriverError("build artifact verification: " + proc.error)
                if proc.exit_code != 0 or proc.stdout.strip() != "5":
                    reasons.append("built Rust calculator did not produce 5")
    elif kind == "node-build":
        for name in ("main.js", "math.js"):
            if after.get("dist/" + name) != facts["before"]["src/" + name]:
                reasons.append(f"incorrect or missing build artifact: dist/{name}")
    elif kind in ("rust-test", "node-test", "python-test"):
        patterns = {
            "rust-test": r"test result: ok\. 2 passed; 0 failed; 0 ignored;",
            "node-test": r"(?m)^# tests 2\s*$[\s\S]*^# pass 2\s*$[\s\S]*^# fail 0\s*$",
            "python-test": r"Ran 2 tests in [\d.]+s[\r\n]+OK(?:\s|$)",
        }
        if not any(re.search(patterns[kind], c["result"]) for c in commands):
            reasons.append("the full two-test suite was not observed passing")
    elif kind == "rust-clean":
        if not any(name.startswith("target/") for name in facts["before"]):
            raise ValueError("clean fixture did not contain actual build artifacts")
        if any(name.startswith("target/") for name in after) or (root / "target").is_symlink():
            reasons.append("build artifacts remain after cleanup")
    elif kind in ("git-diff", "git-commit", "recent-history"):
        reasons.extend(git_judgment(kind, answer, facts, root, evidence))
    elif kind == "versions":
        queried = set()
        for execution in evidence.get("executions") or []:
            call = execution["call"]
            if (call["name"] != "exec" or execution.get("exit_code") != 0
                    or execution.get("timed_out") or execution.get("interrupted")
                    or not isinstance(call["args"].get("command"), str)
                    or not isinstance(execution.get("result"), str)):
                continue
            try:
                commands = version_queries(call["args"]["command"], root, facts)
            except ValueError:
                continue
            output_lines = {line.strip() for line in execution["result"].splitlines()}
            queried.update(name for name in commands if facts["versions"][name] in output_lines)
        claims = version_claims(answer)
        for name, version in facts["versions"].items():
            if name not in queried:
                reasons.append(f"no successful {name} version query was observed")
            number = re.search(r"\d+(?:\.\d+)+", version)[0]
            if not claims[name] or any(value != number for value in claims[name]):
                reasons.append(
                    f"missing, incorrect or contradictory {name} version: expected {number}, found {claims[name]}"
                )
    elif kind == "clarification":
        expected_answers = [
            reply for completion in scenario.get("completions", [])
            for reply in completion.get("answers", [])
        ]
        if expected_answers:
            questions = evidence.get("questions") or []
            if ([q.get("answer") for q in questions] != expected_answers
                    or any(q.get("state") != "answered" for q in questions)):
                reasons.append("declared clarification answers were not observed in the model conversation")
            for question in questions:
                text = question.get("call", {}).get("args", {}).get("question", "")
                if not isinstance(text, str) or clarification_request(response_prose(text)) is None:
                    reasons.append("ask_user does not ask for the missing task or objective")
            asked = False
            for execution in evidence.get("executions") or []:
                if execution["call"]["name"] == "ask_user":
                    asked = True
                elif asked:
                    reasons.append("tools were called after the user's pause answer")
            if not asked:
                reasons.append("clarification did not use ask_user")
        elif clarification_request(response_prose(answer)) is None:
            reasons.append("clarification does not ask for the missing task or objective")
    elif kind in ("build-failure", "test-failure", "port-failure"):
        expected = scenario["completions"][0]
        if (not result.turns or result.turns[0].get("exit_code") != expected["exit_code"]
                or any(s not in result.turns[0]["output"] for s in expected["contains"])):
            reasons.append("the declared original command failure was not observed")
        if kind == "build-failure":
            if "src/main.rs" not in mentioned_files(answer, facts["before"]):
                reasons.append("answer does not identify the affected file src/main.rs")
            if not all(re.search(p, answer, re.I) for p in (r"i32|整数", r"str|string|字符串", r"类型|type")):
                reasons.append("answer omits the string/integer type mismatch")
            diagnosis = re.split(
                r"修复|解决|改法|\b(?:fix|repair|solution|remedy)\b", answer, maxsplit=1, flags=re.I,
            )[0]
            diagnosis = diagnosis.replace("`", "").replace("**", "")
            if re.search(
                r"(?:^|[。！？.!?；;\n])\s*(?:[-*]\s*)?(?:编译器|(?:the\s+)?compiler)"
                r"\s*(?:expected|expects?|期望(?:赋值)?(?:的)?(?:类型)?(?:是|为)?)"
                r"\s*(?:type\s+)?(?:&?str\b|string\b|字符串)", diagnosis, re.I,
            ):
                reasons.append("answer reverses the compiler's expected i32 and actual string types")
            if has_affirmative_match(diagnosis,
                r"(?:要求|预期|期望)(?:的)?(?:类型)?(?:是|为)?\s*(?:&?str\b|string\b|字符串)"
                r"|(?:将|把)\s*(?:i32\s*)?整数\s*(?:赋(?:值)?给|赋予|放入)\s*字符串"
                r"|\b(?:expected?|expects?|requires?)\s+(?:an?\s+)?(?:&?str\b|string\b)",
            ):
                reasons.append("answer reverses the expected integer and actual string")
            remedy = r"改为|改成|替换|转换|解析|parse|replace|convert"
        elif kind == "test-failure":
            if not all(re.search(p, answer, re.I) for p in (r"maths\.py|add", r"减|subtract|a\s*-\s*b", r"加|addition|a\s*\+\s*b")):
                reasons.append("answer does not explain subtraction instead of addition")
            if has_affirmative_match(answer,
                r"(?:改为|改成|换成|替换为)\s*(?:减法|a\s*-\s*b)"
                r"|\b(?:replace|change|convert)\b[^。！？!?；;\n]{0,60}"
                r"(?:addition|a\s*\+\s*b)\s+(?:with|to|into)\s+(?:subtraction|a\s*-\s*b)"
                r"|(?:测试|预期)\s*(?:要求|应为|是)\s*(?:减法|a\s*-\s*b)"
                r"|\bexpect(?:ed|s)?\s+subtraction\b"
            ):
                reasons.append("answer reverses the required addition and erroneous subtraction")
            remedy = r"改为|改成|修改|修正|替换|replace|change|fix"
        else:
            if str(facts["listener"]["port"]) not in answer or not re.search(r"占用|冲突|already in use|bind", answer, re.I):
                reasons.append("answer does not explain the occupied port")
            if re.search(
                r"(?:并未|没有|未|并非|不是)\s*(?:被)?\s*(?:占用|冲突)"
                r"|\b(?:not|isn't|is not)\s+(?:already\s+)?(?:in use|occupied|bound)", answer, re.I,
            ):
                reasons.append("answer denies the observed address conflict")
            remedy = (
                r"(?:换|更改|修改|改用|选择|使用)[^。！？!?；;\n]{0,20}(?:端口|port)"
                r"|其他端口|另[^。！？!?；;\n]{0,10}端口|空闲端口"
                r"|\b(?:change[^.?!;\n]{0,20}port|another port|free port)\b"
                r"|(?:停止|关闭|结束)[^。！？!?；;\n]{0,20}(?:进程|服务|监听)"
                r"|\bstop\s+(?:(?:the|existing|listening)\s+)*(?:process|server|service|listener)\b"
            )
        if not has_affirmative_match(answer, remedy):
            reasons.append("answer provides no recognized repair action")
    return reasons
