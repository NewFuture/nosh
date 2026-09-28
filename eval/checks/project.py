"""Deterministic project scoring; no model judge."""

from __future__ import annotations
from pathlib import Path
import re
import subprocess
from .. import driver, fixtures
from ..approval import PROJECT_POLICIES, command_groups, project_actions, shell_parts
from ..fixtures import artifact, protected_files
from ..suite import NATIVE_CHECKS as PROJECT_CHECKS
from .common import HISTORY_ALIASES, mentioned_files, response_prose
from .experience import clarification_request, history_prose


PROJECT_CHECKS = PROJECT_CHECKS - {"assist-archive", "assist-none", "assist-clarify"}


VERSION_FLAGS = {
    "cargo": {"--version", "-V", "version"},
    "node": {"--version", "-v"},
    "python3": {"--version", "-V"},
}


VERSION_ALIASES = {"cargo": r"cargo", "node": r"node(?:\.js)?", "python3": r"python3?"}


def fixture_state(scenario: dict, facts: dict, root: Path, after: dict, result) -> dict:
    kind = scenario["check"]
    state = {"files": after, "cwd": result.pwd}
    if kind in PROJECT_CHECKS:
        state["files"] = protected_files(after, kind)
        state["artifacts"] = sorted({
            re.sub(r"(?<=eval_math-)[0-9a-f]+", "<hash>", name)
            for name, item in after.items() if artifact(name, item, kind)
        })
        if kind in ("git-diff", "git-commit", "recent-history"):
            state["git"] = fixtures.git_state(root)
    return state


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
        if (call["name"] != "run_command" or execution.get("exit_code") != 0
                or execution.get("timed_out") or execution.get("interrupted")):
            continue
        command = call["args"].get("command")
        if not isinstance(command, str):
            continue
        if action in project_actions(action, command, root, facts):
            matches.append(execution)
    return matches


def change_entries(answer: str) -> dict[str, list[dict]]:
    entries = {"maths.py": [], "README.md": []}
    scope = None
    current = None
    negative = r"\bunstaged\b|not\s+(?:yet\s+)?staged|未(?:被|加入)?暂存(?:区)?|尚未暂存|没有暂存|不在暂存区"
    positive = r"\bstaged\b|已(?:经)?(?:加入)?暂存|暂存区"
    filenames = r"(?<![\w.-])(?:maths\.py|README\.md)(?![\w.-])"

    def staging(text):
        unstaged = bool(re.search(negative, text, re.I))
        staged = bool(re.search(positive, re.sub(negative, "", text, flags=re.I), re.I))
        return "contradictory" if staged and unstaged else "staged" if staged else "unstaged" if unstaged else None

    clauses = re.split(r"\n|[;；。]|[,，](?=[^,，;；。\n]*" + filenames + ")",
                       answer.replace("`", "").replace("**", ""))
    for line in clauses:
        matches = list(re.finditer(filenames, line))
        if not matches:
            label = staging(line)
            heading = re.sub(negative + "|" + positive, "", line, flags=re.I)
            heading = re.sub(r"\b(?:changes?|files?)\b|的|改动|修改|更改|变更|文件|[\s#*\-:：()（）]", "",
                             heading, flags=re.I)
            if label is not None:
                if not heading:
                    scope, current = label, None
                elif current is not None:
                    current["text"] += "\n" + line
                    current["stage"] = "contradictory" if current["stage"] not in (None, label) else label
            elif line.strip().endswith((":", "：")) or re.match(r"^\s*#", line):
                scope, current = None, None
            elif current is not None:
                current["text"] += "\n" + line
            continue
        for i, match in enumerate(matches):
            end = matches[i + 1].start() if i + 1 < len(matches) else len(line)
            suffix = line[match.end():end]
            prefix = line[:match.start()] if i == 0 else ""
            label = staging(prefix + suffix)
            current = {"stage": "contradictory" if label and scope and label != scope else label or scope,
                       "text": prefix + line[match.start():end]}
            entries[match[0]].append(current)
    return entries


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
        state = evidence["final_state"]["git"]
        before = facts["git_before"]
        if kind != "git-commit" and state != before:
            reasons.append("read-only task changed git HEAD, index or worktree")
        if kind == "git-commit":
            if state["parents"] != [before["head"]] or state["commits"] != before["commits"] + 1:
                reasons.append("expected exactly one new commit on the original HEAD")
            if state["status"]:
                reasons.append("git index/worktree is not clean after commit")
            changes = fixtures.git(root, "diff", "--name-only", before["head"], "HEAD").splitlines()
            if sorted(changes) != facts["changed_files"]:
                reasons.append("commit contains the wrong changed-file set")
            for name, item in facts["before"].items():
                blob = subprocess.run(["git", "show", f"HEAD:{name}"], cwd=root,
                                      env=fixtures.project_environment(root.parent / "home"),
                                      capture_output=True, timeout=5)
                import hashlib

                if blob.returncode or hashlib.sha256(blob.stdout).hexdigest() != item["sha256"]:
                    reasons.append(f"committed content differs from requested change: {name}")
        elif kind == "git-diff":
            entries = change_entries(answer)
            for name, fact, stage in (("maths.py", r"subtract|减法|相减", "staged"),
                                      ("README.md", r"unittest|测试", "unstaged")):
                contradiction = (
                    r"未修改|没有改动|无改动|\bunchanged\b|\bnot (?:changed|modified)\b|\bno changes?\b"
                    r"|(?:未|没有|并未)(?:新增|添加|补充)|\b(?:did not add|not added)\b"
                    r"|(?:\b(?:remove[ds]?|delete[ds]?)\b|(?<!未)(?<!没有)(?:删除|移除|删去|去掉))"
                    rf"[^。！？;\n]*(?:{fact})"
                    rf"|(?:{fact})[^。！？;\n]*(?:已删除|被删除|\bremoved\b|\bdeleted\b)"
                )
                if (not any(item["stage"] in (stage, None) and re.search(fact, item["text"], re.I) for item in entries[name])
                        or any(item["stage"] not in (stage, None)
                               or re.search(contradiction, item["text"], re.I)
                               for item in entries[name])):
                    reasons.append(f"missing, contradictory or incorrectly staged change: {name}")
        else:
            blocks = re.split(r"\n\s*\n|(?=^[ \t]*(?:[-*]|\d+[.)])[ \t]+)", answer, flags=re.M)
            blocks = [line for block in blocks for line in (block.splitlines() if "|" in block else [block])]
            order = []
            listed = set()
            listing = True
            history = list(reversed(facts["history"]))
            for position, raw in enumerate(blocks):
                item = re.match(r"^[ \t]*(?:[-*]|\d+[.)])[ \t]+", raw)
                block = raw[item.end():] if item else raw
                block = block.replace("`", "").replace("**", "").strip()
                if not block or re.fullmatch(r"[\s|:-]+", block):
                    continue
                if re.match(r"^(?:#{1,6}\s*)?(?:note|summary|next steps|注意|备注|说明|总结|建议)\s*[:：]", block, re.I):
                    listing = False
                    continue
                if "|" in raw:
                    if position + 1 < len(blocks) and re.fullmatch(r"[\s|:-]+", blocks[position + 1]):
                        continue
                    cells = [cell.strip().lower() for cell in block.strip("|").split("|")]
                    if all(cell in {
                        "#", "commit", "commits", "sha", "hash", "component", "subject", "message",
                        "description", "feature", "提交", "哈希", "组件", "说明", "描述", "功能", "序号",
                    } for cell in cells):
                        continue
                components = [i for i, (component, _) in enumerate(history) if re.search(rf"\b{component}\b", block, re.I)]
                feature_matches = [i for i, (_, feature) in enumerate(history)
                                   if re.search(HISTORY_ALIASES[feature], block, re.I)]
                label = re.match(r"^\s*([a-zA-Z_-]+)\s*[:：]", block)
                if label and label[1].lower() not in {c for c, _ in history} | {"note", "summary"}:
                    reasons.append(f"unknown commit component: {label[1]}")
                index = None
                if len(components) == 1:
                    index = components[0]
                    if not re.search(HISTORY_ALIASES[history[index][1]], block, re.I):
                        reasons.append(f"incorrect/unrecognized recent commit fact: {history[index][0]}")
                        index = None
                elif not components and len(feature_matches) == 1:
                    index = feature_matches[0]
                if index is not None:
                    if item or "|" in raw:
                        if index in listed:
                            reasons.append(f"duplicate recent commit entry: {history[index][0]}")
                        listed.add(index)
                    if index not in order:
                        order.append(index)
                    listing = True
                elif (item or "|" in raw) and listing:
                    reasons.append(f"unrecognized recent commit entry: {block[:120]}")
                elif block.endswith((":", "：")):
                    listing = bool(re.search(r"提交|commits?|history", block, re.I))
            if not order or order[0] != 0 or order != sorted(order):
                reasons.append("recent history must identify the newest commit and keep newest-first order")
            for sha in re.findall(r"\b[0-9a-f]{7,40}\b", answer):
                if not any(commit.startswith(sha) for commit in facts["commit_ids"]):
                    reasons.append(f"unknown commit hash: {sha}")
            prose = history_prose(answer, facts)
            for clause in re.split(r"[。！？!?；;\n，,]", prose):
                counts = re.findall(r"(?<![\d.])(\d+)\s*(?:个|条|次)?\s*(?:提交|commits?\b|记录)", clause, re.I)
                counts.extend(re.findall(
                    r"(?:提交|commits?)\s*[（(]\s*(?:共|total(?:\s+of)?\s*:?)?\s*(\d+)\s*(?:个|条|次)?\s*[）)]",
                    clause, re.I,
                ))
                displayed = bool(re.search(
                    r"最近|最新|以下|列出|展示|\b(?:last|latest|recent|following|shown|listed)\b",
                    clause, re.I,
                ))
                inventory = not displayed and bool(re.search(
                    r"仓库|全部|所有|共|合计|总计|\b(?:repository|repo|total)\b|entire history", clause, re.I,
                ))
                expected_count = len(history) if inventory else len(order)
                for count in set(map(int, counts)):
                    if count > len(history) or (
                        (inventory or displayed or clause.endswith((":", "："))) and count != expected_count
                    ):
                        reasons.append(f"incorrect recent commit count: {count}, expected {expected_count}")
    elif kind == "versions":
        queried = set()
        for execution in evidence.get("executions") or []:
            call = execution["call"]
            if (call["name"] != "run_command" or execution.get("exit_code") != 0
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
        if clarification_request(response_prose(answer)) is None:
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
            remedy = r"改为|改成|替换|转换|解析|parse|replace|convert"
        elif kind == "test-failure":
            if not all(re.search(p, answer, re.I) for p in (r"maths\.py|add", r"减|subtract|a\s*-\s*b", r"加|addition|a\s*\+\s*b")):
                reasons.append("answer does not explain subtraction instead of addition")
            remedy = r"改为|改成|修改|修正|替换|replace|change|fix"
        else:
            if str(facts["listener"]["port"]) not in answer or not re.search(r"占用|冲突|already in use|bind", answer, re.I):
                reasons.append("answer does not explain the occupied port")
            remedy = r"换|更改|修改|其他端口|另.*端口|空闲|change|another port|free port|停止|关闭|stop"
        if not re.search(remedy, answer, re.I):
            reasons.append("answer provides no recognized repair action")
    return reasons
