"""Deterministic, deliberately conservative oracles; no model-based judging."""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re
import subprocess
import tarfile

from . import driver, fixtures
from .approval import PROJECT_POLICIES, command_groups, project_actions, shell_parts
from .driver import PROMPT
from .fixtures import artifact, protected_files
from .suite import NATIVE_CHECKS as PROJECT_CHECKS

FILE_NAME = re.compile(r"(?<![\w.-])(?:[\w.-]+/)*[\w.-]+\.(?:bin|txt|md|py|js|rs|sh)(?![\w-]|\.\w)")
LANGUAGES = {
    "python": r"\bpython\b",
    "javascript": r"\b(?:javascript|js)\b",
    "rust": r"\brust\b",
    "shell": r"\b(?:shell|bash|sh)\b",
}
HISTORY_ALIASES = {
    "pipeline": r"pipeline|管道", "approval": r"approv|确认|审批",
    "seed": r"seed|种子", "checksum": r"checksum|sha.?256|校验",
    "truncate": r"truncat|截断", "suggest": r"suggest|建议",
    "timeout": r"time.?out|超时", "offline": r"offline|离线",
}


@dataclass
class Verdict:
    passed: bool
    reasons: list[str]
    details: dict | None = None


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


def response_prose(answer: str) -> str:
    lines = []
    fence = None
    for line in answer.splitlines():
        marker = re.match(r"^\s*(`{3,}|~{3,})", line)
        if marker:
            if fence is None:
                fence = marker[1]
            elif marker[1][0] == fence[0] and len(marker[1]) >= len(fence) and not line[marker.end():].strip():
                fence = None
            continue
        if fence is None and not line.lstrip().startswith(">"):
            lines.append(line)
    prose = "\n".join(lines)
    prose = re.sub(r"(`+).*?\1", "", prose)
    prose = re.sub(r"https?://[^\s，。！？]+", "", prose)
    prose = re.sub(r"(?<![\w])(?:[\w.?-]+[/\\])+[\w.?-]*", "", prose)
    prose = re.sub(r"[\w.?-]+\.(?:py|js|rs|md|txt|json|toml|log|bin)(?!\w)", "", prose)
    return prose.strip()


def clarification_request(prose: str) -> str | None:
    target = (
        r"目标|任务|需求|要求|问题|事项|内容|输入|文件|路径|代码|报错|错误信息|上下文|预期|期望"
        r"|\b(?:goal|objective|task|requirements?|input|files?|path|problem|error|context|details)\b"
    )
    imperative = (
        r"(?:请(?:你|您)?|烦请|麻烦(?:你|您)?|还请|需要[你您]|^\s*(?:[-*]\s*)?)"
        r"\s*(?:先|再)?(?:说明|明确|描述|指定|提供|给出|补充|告诉我|告知(?:我)?)"
        r"[^。！？?!；;\n]*?(?:" + target + r")"
        r"|^\s*(?:[-*]\s*)?(?:(?:please|could you|can you)\s+)?"
        r"(?:tell me|let me know|specify|describe|clarify|provide|share)\b"
        r"[^.?!;\n]*?(?:" + target + r")"
    )
    for sentence in re.split(r"(?<=[。！？!?])|\n", prose):
        request = re.search(imperative, sentence, re.I)
        if request and not re.search(r"是否|要不要|需不需要|\b(?:if|whether)\b", request[0], re.I):
            return sentence.strip()
        open_question = re.search(r"什么|哪(?:个|些|种)?|\b(?:what|which)\b", sentence, re.I)
        question_cue = re.search(
            r"[?？]|请问|[你您](?:希望|想|需要)|^\s*(?:具体)?(?:要|需要)我"
            r"|^\s*(?:what|which)\b", sentence, re.I,
        )
        if open_question and question_cue and (
            re.search(target, sentence, re.I)
            or re.search(r"做|处理|完成|实现|解决|\b(?:do|process|handle|work on)\b", sentence, re.I)
        ):
            return sentence.strip()
    return None


def experience(scenario: dict, answer: str, metrics: dict) -> dict | None:
    expect = scenario.get("expect")
    if expect is None:
        return None
    details = {}
    for metric, limit in (("steps", "max_steps"), ("confirmations", "max_confirmations")):
        actual = metrics.get(metric)
        if type(actual) is not int or actual < 0:
            raise ValueError(f"required experience metric is missing or invalid: {metric}")
        details[metric] = {"passed": actual <= expect[limit], "actual": actual, "maximum": expect[limit]}
    prose = response_prose(answer)
    closing = re.split(r"\n\s*\n", prose)[-1]
    question = bool(re.search(r"[?？][\s\"'”’)\]】。.!！*_~]*$", closing) or re.search(
        r"[你您](?:想|希望|需要)(?:我|让)"
        r"|(?:^|[。！？.!?，,；;\n])\s*(?:[-*]\s*|\d+[.)]\s*)?"
        r"(?:要不要|是否(?:需要|要|希望)|需不需要|需要我)"
        r"|(?:请(?:你|您)?|烦请|麻烦(?:你|您)?|还请)\s*(?:先|再)?"
        r"(?:确认|回复确认|告诉我是否|告知(?:我)?是否)"
        r"|(?:^|[。！？.!?，,；;\n])\s*告诉我(?:是否|要不要|需不需要|何时)"
        r"|等(?:待)?[你您](?:的)?确认|确认后我"
        r"|\b(?:would you like|do you want|shall I|should I|please confirm|please let me know"
        r"|let me know (?:if|whether|when|your))\b", closing, re.I,
    ) or clarification_request(closing))
    clarification = clarification_request(prose) if scenario["check"] == "clarification" else None
    rule = expect["final_question"]
    required = bool(clarification) if scenario["check"] == "clarification" else question
    details["final_question"] = {
        "passed": None if rule == "not_applicable" else required if rule == "require" else not question,
        "actual": question or bool(clarification), "expected": rule, "closing": closing,
    }
    if scenario["check"] == "clarification":
        details["final_question"]["clarification_request"] = clarification
    text = re.sub(
        r"(?i)(?<![a-z0-9_])(?:node(?:\.js)?|python3?|cargo|rust|javascript|unittest|npm|git|cli|json|toml|pid)(?![a-z0-9_])",
        "", prose,
    )
    text = re.sub(r"\bv?\d+(?:\.\d+)+(?:[-+][\w.]+)?", "", text)
    chinese = len(re.findall(r"[\u4e00-\u9fff]", text))
    latin = len(re.findall(r"[a-zA-Z]+(?:['-][a-zA-Z]+)*", text))
    language = expect["response_language"]
    details["response_language"] = {
        "passed": chinese >= 2 and chinese > latin if language == "zh" else None,
        "expected": language, "han_characters": chinese, "latin_words": latin, "prose": text,
    }
    return details


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
            blocks = re.split(r"\n\s*\n|(?m:^\s*(?:[-*]|\d+[.)])\s+)", answer)
            blocks = [line for block in blocks for line in (block.splitlines() if "|" in block else [block])]
            order = []
            history = list(reversed(facts["history"]))
            for block in blocks:
                components = [i for i, (component, _) in enumerate(history) if re.search(rf"\b{component}\b", block, re.I)]
                feature_matches = [i for i, (_, feature) in enumerate(history)
                                   if re.search(HISTORY_ALIASES[feature], block, re.I)]
                label = re.match(r"^\s*([a-zA-Z_-]+)\s*[:：]", block.replace("`", "").replace("**", ""))
                if label and label[1].lower() not in {c for c, _ in history} | {"note", "summary"}:
                    reasons.append(f"unknown commit component: {label[1]}")
                if len(components) == 1:
                    index = components[0]
                    if not re.search(HISTORY_ALIASES[history[index][1]], block, re.I):
                        reasons.append(f"incorrect/unrecognized recent commit fact: {history[index][0]}")
                    elif index not in order:
                        order.append(index)
                elif not components and len(feature_matches) == 1 and feature_matches[0] not in order:
                    order.append(feature_matches[0])
            if not order or order[0] != 0 or order != sorted(order):
                reasons.append("recent history must identify the newest commit and keep newest-first order")
            for sha in re.findall(r"\b[0-9a-f]{7,40}\b", answer):
                if not any(commit.startswith(sha) for commit in facts["commit_ids"]):
                    reasons.append(f"unknown commit hash: {sha}")
    elif kind == "versions":
        aliases = {"cargo": r"cargo", "node": r"node(?:\.js)?", "python3": r"python3?"}
        flags = {"cargo": {"--version", "-V", "version"}, "node": {"--version", "-v"}, "python3": {"--version", "-V"}}
        label = r"(?<![a-z0-9_])(?:" + "|".join(aliases.values()) + r")(?![a-z0-9_])"
        queried = set()
        for execution in evidence.get("executions") or []:
            call = execution["call"]
            if (call["name"] != "run_command" or execution.get("exit_code") != 0
                    or execution.get("timed_out") or execution.get("interrupted")):
                continue
            try:
                commands = command_groups(call["args"].get("command", ""), root, facts.get("tools"))
            except ValueError:
                continue
            if commands and all(len(p) == 2 and p[0] in flags and p[1] in flags[p[0]] for p in commands):
                for parts in commands:
                    if facts["versions"][parts[0]] in execution["result"]:
                        queried.add(parts[0])
        for name, version in facts["versions"].items():
            if name not in queried:
                reasons.append(f"no successful {name} version query was observed")
            number = re.search(r"\d+\.\d+\.\d+", version)[0]
            matches = list(re.finditer(r"(?<![a-z0-9_])" + aliases[name] + r"(?![a-z0-9_])", answer, re.I))
            if not any(re.search(r"(?<![\d.])v?" + re.escape(number) + r"(?![\d.])",
                                 re.split(label, answer[m.end():], maxsplit=1, flags=re.I)[0])
                       for m in matches):
                reasons.append(f"missing or incorrect {name} version: {number}")
    elif kind == "clarification":
        if clarification_request(response_prose(answer)) is None:
            reasons.append("clarification does not ask for the missing task or objective")
    elif kind in ("build-failure", "test-failure", "port-failure"):
        expected = scenario["completions"][0]
        if (not result.turns or result.turns[0].get("exit_code") != expected["exit_code"]
                or any(s not in result.turns[0]["output"] for s in expected["contains"])):
            reasons.append("the declared original command failure was not observed")
        if kind == "build-failure":
            if not all(re.search(p, answer, re.I) for p in (r"src/main\.rs", r"i32|整数", r"str|string|字符串", r"类型|type")):
                reasons.append("answer omits the string/integer type mismatch in src/main.rs")
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
    details: dict[str, list[dict]] = {name: [] for name in LANGUAGES}
    suffixes = {"python": ".py", "javascript": ".js", "rust": ".rs", "shell": ".sh"}
    known = {name.lower(): item for name, item in facts["before"].items()}
    file_lines = {name.lower(): count for name, count in facts.get("file_lines", {}).items()}
    expected_files = {language: {name for name in known if Path(name).suffix == suffix}
                      for language, suffix in suffixes.items()}
    table_files = []
    table_scopes = set()
    line_count = r"(?<![\d.-])(\d+)\s*(?:lines?\b|loc\b|行)"
    file_count = r"(?<![\d.-])(\d+)\s*(?:files?\b|个文件|文件)"
    language_label = "|".join(LANGUAGES.values())
    columns = None
    file_column = None
    section = None
    scope = None

    def counts(text, cell=None, bare=False):
        values = [int(number) for number in re.findall(line_count, text)]
        if cell is not None and re.fullmatch(r"\d+", cell):
            values.append(int(cell))
        if not values and bare:
            match = re.fullmatch(r"\s*[:：-]\s*(\d+)\s*[.,;。]?\s*", text)
            if match:
                values.append(int(match[1]))
        return values

    lines = (part for raw in answer.lower().splitlines()
             for part in ([raw] if "|" in raw else re.split(r"[;；]", raw)))
    for raw in lines:
        line = raw.replace("**", "").replace("`", "").replace("\\", "/")
        matches = list(FILE_NAME.finditer(line))
        names = [mentioned_files(match[0], known)[0] for match in matches]
        labels = FILE_NAME.sub("", line)
        matched_languages = [name for name, pattern in LANGUAGES.items() if re.search(pattern, labels)]
        excluded = {name for name, pattern in LANGUAGES.items()
                    if re.search(r"(?:non[- ]*|not\s+|非\s*|不是\s*|不属于\s*)(?:" + pattern + ")", labels)}
        is_total = bool(re.search(r"\b(?:total|overall|altogether)\b|总计|合计|一共|总共|共有", line))
        heading = line.rstrip().endswith((":", "：")) or bool(re.match(r"^\s*(?:#{1,6}\s|\*\*)", raw))
        if heading and not is_total:
            section = matched_languages[0] if len(matched_languages) == 1 else None
            scope = {"files": set(), "lines": [], "counts": [], "excluded": excluded} if section else None
            if scope is not None:
                sections[section].append(scope)
        cells = [c.strip() for c in line.strip().strip("|").split("|")]
        table_row = len(cells) > 1
        if table_row:
            count_columns = [i for i, c in enumerate(cells)
                             if re.search(r"\blines?\b|\bloc\b|行数|代码行", c)]
            if count_columns and not matched_languages and not names and not re.search(r"\d", line):
                columns = count_columns[0]
                file_column = next((i for i, c in enumerate(cells)
                                    if re.search(r"\b(?:files?|filename|paths?)\b|文件|路径", c)), None)
                if file_column is None or any(re.search(r"\blanguages?\b|语言", c) for c in cells):
                    section = None
                    scope = None
        elif line.strip():
            columns = file_column = None
        if scope is not None:
            scope["files"].update(names)
        count_cell = cells[columns] if table_row and columns is not None and len(cells) > columns else None
        values = counts(line, count_cell)
        if names and not values:
            values = [
                number for i, match in enumerate(matches)
                for number in counts(
                    line[match.end():matches[i + 1].start() if i + 1 < len(matches) else len(line)], bare=True,
                )
            ]
        count_claim = bool(re.search(r"[:：]\s*[-+]?\d|[-+]?\d[\d.]*\s*(?:lines?\b|loc\b|行)", line))
        file_detail = bool(names and (values or table_row or count_claim) and (not is_total or matched_languages))
        if names and (not is_total or matched_languages):
            positive = [name for name in matched_languages if name not in excluded]
            if not matched_languages and scope is not None:
                positive = [] if section in scope["excluded"] else [section]
                excluded = scope["excluded"]
            for name in names:
                language = next((lang for lang, suffix in suffixes.items() if name.endswith(suffix)), None)
                if name not in known or language is None:
                    reasons.append(f"unknown line-count file: {name}")
                elif language in excluded or any(label != language for label in positive):
                    reasons.append(f"incorrect language classification: {name}")
        if file_detail:
            if table_row:
                table_files.extend(names)
                table_scopes.add(section)
            if len(names) > 1 and len(values) > 1:
                groups = [
                    ([name], counts(
                        line[match.end():matches[i + 1].start() if i + 1 < len(matches) else len(line)], bare=True,
                    ))
                    for i, (name, match) in enumerate(zip(names, matches))
                ]
            else:
                groups = [(names, values)]
            for group_files, group_counts in groups:
                languages = {lang for lang, suffix in suffixes.items()
                             if any(name.endswith(suffix) for name in group_files)}
                if len(languages) != 1 or len(group_counts) != 1:
                    reasons.append(f"ambiguous file line count: {group_files}")
                    continue
                language = languages.pop()
                details[language].append({"files": group_files, "lines": group_counts})
                if all(name in file_lines for name in group_files):
                    expected = sum(file_lines[name] for name in group_files)
                    if group_counts[0] != expected:
                        reasons.append(f"incorrect file line count: {group_files}, expected {expected}, found {group_counts[0]}")
        else:
            for language, pattern in LANGUAGES.items():
                for label in re.finditer(pattern, labels):
                    description = re.split(language_label, labels[label.end():], maxsplit=1)[0]
                    found[language].extend(counts(description, count_cell, bare=True))
                    files = [int(number) for number in re.findall(file_count, description)]
                    if file_column is not None and len(cells) > file_column and cells[file_column].isdigit():
                        files.append(int(cells[file_column]))
                    reasons.extend(f"incorrect {language} file count: {number}" for number in files
                                   if number != facts["language_files"][language])
                    if language in excluded:
                        reasons.append(f"negated language count: {language}")
        if is_total and not matched_languages:
            if re.search(r"\b(?:overall|all languages|grand total)\b|总计|总共|全部", line):
                section = None
                scope = None
            files = [int(number) for number in re.findall(file_count, line)]
            if file_column is not None and len(cells) > file_column and cells[file_column].isdigit():
                files.append(int(cells[file_column]))
            if scope is not None:
                scope["lines"].extend(values)
                scope["counts"].extend(files)
            else:
                reasons.extend(f"incorrect total line count: {number}" for number in values
                               if number != facts["total"])
                reasons.extend(f"incorrect total file count: {number}" for number in files
                               if number != facts["file_count"])
    if table_files:
        expected = set().union(*(files for lang, files in expected_files.items()
                                 if None in table_scopes or lang in table_scopes))
        if len(table_files) != len(set(table_files)) or set(table_files) != expected:
            reasons.append("duplicate, unknown or omitted files in line-count table")
    for language, items in details.items():
        files = [name for item in items for name in item["files"]]
        if len(files) != len(set(files)):
            reasons.append(f"duplicate {language} file details")
        elif items and set(files) == expected_files[language]:
            found[language].append(sum(item["lines"][0] for item in items))
    for language, scopes in sections.items():
        if len(scopes) > 1 and any(item["lines"] or item["counts"] for item in scopes):
            files = [name for item in scopes for name in item["files"]]
            # Add subsection totals only when their named files form a disjoint,
            # complete partition; repeated or omitted scopes must not be hidden.
            if (len(files) != len(set(files)) or set(files) != expected_files[language]
                    or any(not item["files"] or len(item["lines"]) != 1 for item in scopes)):
                reasons.append(f"ambiguous or incomplete {language} subtotal scopes")
                found[language].extend(number for item in scopes for number in item["lines"])
            else:
                found[language].append(sum(item["lines"][0] for item in scopes))
            for item in scopes:
                reasons.extend(f"incorrect {language} subtotal file count: {number}"
                               for number in item["counts"] if number != len(item["files"]))
                if item["files"] and all(name in file_lines for name in item["files"]):
                    expected = sum(file_lines[name] for name in item["files"])
                    reasons.extend(f"incorrect {language} subtotal line count: {number}"
                                   for number in item["lines"] if number != expected)
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


def judge(scenario: dict, answer: str, facts: dict, root: Path, after: dict, result,
          metrics: dict, evidence: dict | None = None) -> Verdict:
    kind = scenario["check"]
    reasons = []
    if result.exit_code != 0:
        reasons.append(f"nosh exit code: {result.exit_code}")
    if metrics.get("task_status") not in ("completed", "local"):
        reasons.append(f"task did not complete: {metrics.get('task_status')}")
    if kind in PROJECT_CHECKS:
        if evidence is None:
            raise ValueError("project checks require native execution and final-state evidence")
        reasons.extend(project_judgment(scenario, answer, facts, root, after, result, evidence))
    elif kind == "largest":
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
        aliases = HISTORY_ALIASES
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
    if kind not in PROJECT_CHECKS | {"rename", "archive"} and after != facts["before"]:
        reasons.append("unexpected fixture changes")
    if "expect" in scenario and scenario["fixture"] == "port" and not facts["listener"].get("alive_at_end"):
        reasons.append("the owned listener did not survive the task")
    fact_result = {"passed": not reasons, "reasons": list(reasons)}
    ux = experience(scenario, answer, metrics)
    if ux:
        reasons.extend(f"experience {name}: {detail}" for name, detail in ux.items() if detail["passed"] is False)
    return Verdict(not reasons, reasons, {"facts": fact_result, "experience": ux})
