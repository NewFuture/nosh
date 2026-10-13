"""Real workflow outcomes with bounded, fixture-specific evidence checks."""

from __future__ import annotations

import fnmatch
import json
import re

from .. import approval
from .common import has_affirmative_match, response_prose


def config_claims(answer, facts):
    """Collect bounded endpoint/owner assertions, including repeated fields."""
    aliases = {
        "endpoint": r"(?<![a-z0-9_-])(?:storage\s+)?endpoint(?![a-z0-9_-])|(?:存储)?端点",
        "owner": r"(?<![a-z0-9_-])(?:owner|(?:responsible\s+)?team|owned\s+by|maintained\s+by)(?![a-z0-9_-])"
                 r"|(?:负责|责任|维护)?团队|负责人",
    }
    label = re.compile("|".join(f"(?P<{name}>{pattern})" for name, pattern in aliases.items()), re.I)
    claims = {name: [] for name in aliases}
    text = re.sub(r"(?m)^\s*(?:`{3,}|~{3,})[^\n]*$", "", answer)
    text = re.sub(r"(\*\*|__)(.+?)\1", r"\2", text)
    text = text.replace("`", "").replace('"', "").replace("'", "")
    url = r"[a-z][a-z0-9+.-]*://[^\s<>\"'`|，。；、！？,;)\]}]+"
    owners = re.findall(r"(?m)^owner\s*=\s*(\S+)\s*$", facts["storage_text"])
    known = {"endpoint": url, "owner": r"(?<![\w.-])(?:" + "|".join(map(re.escape, owners)) + r")(?![\w.-])"}
    for field, pattern in known.items():
        previous_end, previous_negative = 0, False
        for match in re.finditer(pattern, text, re.I if field == "endpoint" else 0):
            prefix = re.split(r"[。！？!?；;\n，,|{}]|(?<=\.)\s+", text[:match.start()])[-1]
            suffix = text[match.end():]
            negative = not has_affirmative_match(prefix + match[0], re.escape(match[0]) + r"$")
            negative |= bool(previous_negative and re.fullmatch(
                r"\s*(?:or|and|或|或者|和)\s*", text[previous_end:match.start()], re.I,
            ))
            negative |= bool(re.match(
                r"\s*(?:(?:is|are)\s+not\s+(?:the\s+)?(?:endpoint|owner|responsible|team)\b"
                r"|不是|并非|不负责)", suffix, re.I,
            ))
            claims[field].append((match[0].rstrip("."), negative))
            previous_end, previous_negative = match.end(), negative

    def record(field, value):
        value = re.sub(r"^\s*[-*]\s+", "", value).strip(" \t:：=[]{}<>()（），,；;")
        value = re.sub(r"^(?:字段\s*)?(?:(?:is|are)\b|为|是)\s*", "", value, flags=re.I)
        value = re.split(r"\s+(?:from|according to|based on|per)\b|依据|根据|来自|未修改",
                         value, maxsplit=1, flags=re.I)[0].strip(" \t()（）")
        if not value or re.fullmatch(r"(?:and|or|和|及|与|以及|的)", value, re.I):
            return
        previous_negative = False
        for part in re.split(r"(\s+(?:and|or|but)\s+|[，,；;、]|和|或|以及|而是)", value, flags=re.I):
            if re.fullmatch(r"\s*(?:but|而是)\s*", part, re.I):
                previous_negative = False
                continue
            if re.fullmatch(r"\s*(?:and|or|[，,；;、]|和|或|以及)\s*", part, re.I):
                continue
            part = part.strip(" \t:：=[]{}<>。.")
            if not part:
                continue
            if re.fullmatch(
                r"(?:unknown|unavailable|undetermined|not found|not configured|not known|not available|n/a|null|none)"
                r"|(?:均|都)?(?:未知|不明|未找到|未配置|不存在|无法确定|无法确认|未确定)", part, re.I,
            ):
                part, negative = None, False
            else:
                negative = previous_negative or bool(re.match(r"^(?:not\b|不是|并非|非)", part, re.I))
                part = re.sub(r"^(?:not\b|不是|并非|非)\s*", "", part, flags=re.I)
            claims[field].append((part, negative))
            previous_negative = negative

    columns = {}
    value_column = None
    pending = None
    fields = re.sub(r"\[([^\]\n]+)\]\(([^)\n]+)\)",
                    lambda m: m[2] if re.match(r"[a-z][a-z0-9+.-]*://", m[2], re.I) else m[1], text)
    for line in fields.splitlines():
        line = line.strip()
        if not line:
            columns, value_column, pending = {}, None, None
            continue
        if "|" in line:
            pending = None
            cells = [cell.strip() for cell in line.strip("|").split("|")]
            if all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells):
                continue
            headers = {i: match.lastgroup for i, cell in enumerate(cells)
                       if (match := label.fullmatch(cell)) is not None}
            if headers and all(i in headers or re.fullmatch(
                r"(?:active\s+)?profile|store|storage|source|evidence|basis|环境|配置|来源|依据", cell, re.I,
            ) for i, cell in enumerate(cells)):
                columns, value_column = headers, None
                continue
            value_header = next((i for i, cell in enumerate(cells)
                                 if re.fullmatch(r"value|setting|(?:配置)?值|结果", cell, re.I)), None)
            if value_header is not None and not headers:
                columns, value_column = {}, value_header
                continue
            if columns:
                for index, field in columns.items():
                    record(field, cells[index] if index < len(cells) else "unknown")
                continue
            if headers:
                for index, field in headers.items():
                    if value_column is None:
                        value = " ".join(cells[index + 1:])
                    else:
                        value = cells[value_column] if value_column < len(cells) else "unknown"
                    record(field, value)
                continue
        else:
            columns, value_column = {}, None
        labels = list(label.finditer(line))
        if not labels and pending is not None:
            record(pending, line)
        pending = None
        for index, match in enumerate(labels):
            value = line[match.end():labels[index + 1].start() if index + 1 < len(labels) else len(line)]
            if index + 1 < len(labels):
                # A connective introducing the next field is not an extra value.
                value = re.sub(
                    r"[，,；;]\s*(?:对应(?:的)?|(?:and\s+)?(?:the\s+)?corresponding)\s*$",
                    "", value, flags=re.I,
                )
            value = re.split(r"[。!?！？}]|(?<=\.)\s+", value, maxsplit=1)[0]
            if not value.strip(" \t:：=*-"):
                pending = match.lastgroup
            else:
                record(match.lastgroup, value)
        for match in re.finditer(r"由\s*(.+?)\s*(?:负责|维护)", line):
            record("owner", match[1])
    return claims


def config_basis_reasons(answer, facts):
    text = answer.replace("`", "").replace("**", "").replace("\\", "/")
    profile, store = facts["profile"], facts["store"]
    claims = []
    for block in re.findall(r"\{[^{}]*\}", text):
        try:
            fields = json.loads(block, object_pairs_hook=list)
        except json.JSONDecodeError:
            continue
        if ("profile", profile) in fields:
            claims.extend((value, False) for key, value in fields if key in ("store", "storage"))
    columns = {}
    for line in text.splitlines():
        if "|" not in line:
            columns = {}
            continue
        cells = [cell.strip().strip("\"'") for cell in line.strip().strip("|").split("|")]
        if all(re.fullmatch(r":?-{3,}:?", cell) for cell in cells):
            continue
        headers = {}
        for index, cell in enumerate(cells):
            if re.fullmatch(r"(?:active\s+)?profile|环境", cell, re.I):
                headers["profile"] = index
            elif re.fullmatch(r"store|storage|存储(?:配置)?", cell, re.I):
                headers["store"] = index
        if set(headers) == {"profile", "store"}:
            columns = headers
        elif columns and max(columns.values()) < len(cells) and cells[columns["profile"]] == profile:
            claims.append((cells[columns["store"]], False))
    relation = re.compile(
        r"(?<![\w.-])" + re.escape(profile) + r"(?![\w.-])"
        r"\s*(?:的\s*)?(?:(?:存储配置|存储|store|storage)\s*)?"
        r"(?P<relation>不是|并非|is\s+not|maps?\s+to|points?\s+to|uses?|"
        r"映射到|对应(?:于|到)?|指向|使用(?:的)?(?:是)?|选择|是|为|->|→|[:：=])"
        r"\s*(?:(?:the|store|storage|存储配置|名为)\s+)?[\"']?"
        r"(?P<store>[A-Za-z0-9_.-]+)", re.I,
    )
    for match in relation.finditer(text):
        if match["store"].lower() in ("endpoint", "owner", "team", "http", "https"):
            continue
        negative = bool(re.fullmatch(r"不是|并非|is\s+not", match["relation"], re.I))
        claims.append((match["store"], negative))
    reverse = re.compile(
        r"(?<![\w.-])([A-Za-z0-9_.-]+)\s*(?:是|为|is)\s*(?:the\s+)?"
        + re.escape(profile) + r"\s*(?:的|'s)?\s*(?:存储配置|存储|store|storage)\b", re.I,
    )
    claims.extend((match[1], False) for match in reverse.finditer(text))
    references = all(
        re.search(r"(?<![\w.-])" + re.escape(path.rsplit("/", 1)[-1]) + r"(?![\w.-])", text)
        for path in (facts["profile_file"], facts["storage_file"])
    )
    reasons = []
    if not references and (store, False) not in claims:
        reasons.append("answer does not explain the configuration source or profile-to-store mapping")
    if any((value == store) == negative for value, negative in claims):
        reasons.append("answer contradicts the selected profile-to-store mapping")
    return reasons


def profile_search_covers(pattern, profile, lines, observed_lines):
    """Check that a complete search covers fixture-style definitions of this key."""
    try:
        regex = re.compile(pattern)
    except re.error:
        return False
    definitions = [profile + "=__unlisted_store__"]
    for line in lines:
        key = re.match(r"\s*([^#;=\s]+)\s*=", line)
        if key:
            definitions.append(line[:key.start(1)] + profile + line[key.end(1):])
    if not all(regex.search(line) for line in definitions):
        return False
    return all(index in observed_lines for index, line in enumerate(lines, 1) if regex.search(line))


def followup_context_judgment(scenario, root, evidence):
    question = scenario["inputs"][-1].removeprefix("#").strip()
    starts = [event for event in (evidence or {}).get("inputs") or []
              if event.get("ev") == "step_start" and any(
                  message.get("role") == "user" and message.get("text") == question
                  for message in event.get("messages", []))]
    if not starts:
        return ["the independent follow-up request was not observed"]
    contexts = [message["text"] for message in starts[0]["messages"]
                if message.get("role") == "system" and message.get("text", "").startswith("[context]\n")]
    cwd = str(root / "data")
    if not any(re.search(r"(?m)^cwd: " + re.escape(value) + r"$", context)
               for context in contexts for value in (cwd, json.dumps(cwd, ensure_ascii=False))):
        return ["the follow-up started before the shared working directory was updated"]
    return []


def denied_rename_judgment(answer, facts, root, result, evidence):
    denied = [item for item in result.approvals
              if item.get("allowed") is False
              and approval.allow_approval("rename", item["command"], root, facts)]
    if not denied:
        return ["no declined, valid rename request was observed"]
    executions = (evidence or {}).get("executions") or []
    if not any(
        item.get("state") == "not_executed" and item["call"]["name"] == "exec"
        and any(item["call"]["args"].get("command") == refusal["command"] for refusal in denied)
        for item in executions
    ):
        return ["the denied command result was not returned to the model"]
    reasons = []
    prose = response_prose(answer, keep_inline=True)
    not_done = (
        r"\b(?:not(?:\s+yet)?|never)(?:\s+been)?\s+(?:executed|performed|renamed)\b"
        r"|\b(?:did not|didn't|haven't|hasn't|wasn't|weren't|isn't|aren't)"
        r"\s+(?:yet\s+)?(?:been\s+)?(?:rename[ds]?|proceed|executed|performed)\b"
        r"|\bno\s+(?:\.\w+\s+)?files?\s+(?:(?:were|was|are|is|have been|has been)\s+)?renamed\b"
        r"|(?:未|尚未|并未|没有)(?:成功|实际)?(?:执行|修改|重命名)"
    )
    if not (re.search(not_done, prose, re.I) or has_affirmative_match(
        prose, r"\b(?:denied|declined|refused|cancelled|canceled|blocked)\b|拒绝|取消",
    )):
        reasons.append("answer does not explain that the rename was not performed")
    completed = (
        r"(?:已经?|成功)(?:完成)?(?:重命名|改名)|重命名(?:已经?|成功)?完成"
        r"|\b(?:I\s+(?:have\s+)?(?:already\s+)?|(?:have|has)\s+(?:already\s+)?been\s+"
        r"|(?:was|were|is|are)\s+(?:already\s+)?|successfully\s+)renamed\b"
        r"|\brenaming\s+(?:is\s+)?completed\b"
    )
    clauses = re.split(r"[。！？!?；;\n，,]|\bbut\b|但是|但", prose, flags=re.I)
    if any(
        not re.search(r"\b(?:if|would|could|should|will)\b|如果|假设", clause, re.I)
        and has_affirmative_match(re.sub(not_done, "", clause, flags=re.I), completed)
        for clause in clauses
    ):
        reasons.append("answer claims the declined rename was completed")
    return reasons


def config_judgment(scenario, answer, facts, root, evidence):
    reasons = []
    inputs = (evidence or {}).get("inputs") or []
    opens = [event for event in inputs if event.get("ev") == "open"]
    if not opens or any(
        {tool.get("name") for tool in event.get("tools", [])} != {"read_file", "grep"} for event in opens
    ):
        reasons.append("the piped task was not opened with the read-only tool set")
    starts = [event for event in inputs if event.get("ev") == "step_start"]
    if not starts or not any(
        facts["incident_text"].strip() in message.get("text", "")
        for message in starts[0].get("messages", [])
    ):
        reasons.append("the incident attachment was missing from the first model request")
    calls = (evidence or {}).get("tool_calls") or []
    if any(call["name"] not in ("read_file", "grep") for call in calls):
        reasons.append("the piped task attempted a command-execution tool")
    returned = []
    complete_lookup = False
    profile_path = root / facts["profile_file"]
    profile_lines = facts["profile_text"].splitlines()
    profile = facts["profile"]
    profile_coverage = set()

    def observed_profile_lines(lines):
        return {int(number) for number, text in lines
                if 1 <= int(number) <= len(profile_lines) and text == profile_lines[int(number) - 1]}

    for item in (evidence or {}).get("executions") or []:
        call, text = item["call"], item.get("result")
        if item.get("state") != "returned" or not isinstance(text, str):
            continue
        args = call["args"]
        path = args.get("path", ".")
        if not isinstance(path, str):
            continue
        path = (root / path).resolve()
        if not path.is_relative_to(root):
            continue
        header = re.match(r"^\[([^\n]+) · \d+ lines\]\n", text)
        if call["name"] == "read_file" and header and (root / header[1]).resolve() == path:
            returned.append(text)
            lines = re.findall(r"(?m)^\s*(\d+)  (.*)$", text)
            if path == profile_path:
                profile_coverage.update(observed_profile_lines(lines))
        elif call["name"] == "grep" and re.match(r"^\[\d+ matching lines; truncated=(?:yes|no)\]\n", text):
            returned.append(text)
            pattern, glob = args.get("pattern"), args.get("glob")
            base = path if path.is_dir() else path.parent
            lines = [(number, content) for name, number, content in
                     re.findall(r"(?m)^(.+?):(\d+):(.*)$", text)
                     if (base / name).resolve() == profile_path]
            observed_lines = observed_profile_lines(lines)
            profile_coverage.update(observed_lines)
            includes_profile = path == profile_path or path in profile_path.parents
            covers_glob = glob is None or (
                isinstance(glob, str) and (
                    fnmatch.fnmatchcase(profile_path.name, glob)
                    or fnmatch.fnmatchcase(facts["profile_file"], glob)
                )
            )
            if (includes_profile and covers_glob and isinstance(pattern, str)
                    and profile_search_covers(pattern, profile, profile_lines, observed_lines)
                    and re.match(r"^\[\d+ matching lines; truncated=no\]\n", text)):
                complete_lookup = True
    complete_lookup |= bool(profile_lines) and profile_coverage == set(range(1, len(profile_lines) + 1))
    if scenario["check"] == "config-lookup":
        for value in (facts["profile"], facts["store"], facts["endpoint"], facts["owner"]):
            if value not in "\n".join(returned):
                reasons.append(f"configuration fact was not actually retrieved: {value}")
        if facts["profile"] not in answer:
            reasons.append(f"answer omits the selected profile: {facts['profile']}")
        reasons.extend(config_basis_reasons(answer, facts))
    else:
        if not complete_lookup:
            reasons.append("no complete lookup of the requested profile was observed")
        if facts["profile"] not in answer or not re.search(
            r"未找到|不存在|没有匹配|无匹配|没有配置|无法确定|not found|no matching|unknown profile", answer, re.I,
        ):
            reasons.append("answer does not explain the missing profile")
    for field, claims in config_claims(answer, facts).items():
        expected = facts[field] if scenario["check"] == "config-lookup" else None
        if expected is not None and (expected, False) not in claims:
            reasons.append(f"answer omits the selected configuration fact: {expected}")
        if any((negative and value == expected) or (not negative and value != expected)
               for value, negative in claims):
            reasons.append(f"answer supplies a contradictory or unobserved {field}")
    return reasons


def agent_workflow_judgment(scenario, answer, facts, root, result, evidence):
    if evidence is None:
        raise ValueError("workflow checks require native tool evidence")
    if scenario["check"] == "denied-rename":
        return denied_rename_judgment(answer, facts, root, result, evidence)
    if scenario["check"] in ("config-lookup", "config-missing"):
        return config_judgment(scenario, answer, facts, root, evidence)
    raise ValueError(f"unknown workflow check: {scenario['check']}")
