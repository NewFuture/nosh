"""Git changes, historical facts and committed state."""

from __future__ import annotations

from pathlib import Path
import re
import subprocess

from .. import fixtures
from .common import HISTORY_ALIASES, history_contradictions
from .experience import history_prose


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


def history_summary(answer: str, facts: dict) -> list[str]:
    reasons = []
    aliases = HISTORY_ALIASES
    blocks = []
    for paragraph in re.split(r"\n\s*\n", answer):
        if "|" in paragraph:
            blocks.extend(paragraph.splitlines())
        else:
            blocks.extend(re.split(r"(?m)^(?=(?:\d+[.)]|[-*])\s)", paragraph))
    history_components = [component for component, _ in facts["history"]]
    blocks = [
        sentence for block in blocks
        for sentence in (
            re.split(r"(?<=[。！？!?])\s*|(?<=\.)\s+", block)
            if sum(bool(re.search(rf"\b{c}\b", block, re.I)) for c in history_components) > 1
            else [block]
        )
    ]
    sections = [("commits", [])]
    for block in blocks:
        plain = block.replace("`", "").replace("**", "").strip()
        heading = re.match(
            r"^(?:#{1,6}\s*)?(notes?|summary|next steps|commits?|history|注意|备注|说明|总结|建议|提交)\s*[:：]",
            plain, re.I,
        )
        if heading:
            name = heading[1].lower()
            section_kind = "advice" if name in ("next steps", "建议") else (
                "notes" if name in ("note", "notes", "注意", "备注", "说明") else "commits")
            sections.append((section_kind, []))
            plain = plain[heading.end():].strip()
        if plain:
            sections[-1][1].append(plain)
    listed = set()
    fact_blocks = []
    for section_kind, entries in sections:
        if section_kind == "advice":
            continue
        fact_blocks.extend(entries)
        listing = section_kind == "commits" or any(
            re.search(rf"\b{component}\b", block, re.I) and re.search(aliases[feature], block, re.I)
            for block in entries for component, feature in facts["history"]
        ) or any(
            (label := re.match(r"^\s*(?:[-*]|\d+[.)])?\s*([a-zA-Z_-]+)\s*[:：—-]\s+", block))
            and label[1].lower() not in {
                "note", "notes", "date", "author", "branch", "count", "total", "status",
            }
            for block in entries
        ) or any(
            re.search(r"\|\s*(?:component|组件)\s*\|", block, re.I) for block in entries
        )
        if not listing:
            continue
        for position, block in enumerate(entries):
            label = re.match(r"^\s*(?:[-*]|\d+[.)])?\s*([a-zA-Z_-]+)\s*:", block)
            if label and label[1].lower() not in set(history_components) | {
                "commit", "component", "feature", "features", "components",
            }:
                reasons.append(f"unknown commit component: {label[1]}")
            item = bool(re.match(r"^\s*(?:[-*]|\d+[.)])\s+", block))
            table = "|" in block
            if table and (
                re.fullmatch(r"[\s|:-]+", block)
                or (position + 1 < len(entries) and re.fullmatch(r"[\s|:-]+", entries[position + 1]))
                or all(cell.strip().lower() in {
                    "#", "commit", "commits", "sha", "hash", "component", "feature",
                    "subject", "message", "description",
                    "提交", "哈希", "组件", "功能", "说明", "描述", "序号",
                } for cell in block.strip("|").split("|"))
            ):
                continue
            components = [component for component in history_components
                          if re.search(rf"\b{component}\b", block, re.I)]
            if item or table:
                if len(components) != 1:
                    reasons.append("unrecognized commit entry: " + block[:120])
                elif components[0] in listed:
                    reasons.append("duplicate commit entry: " + components[0])
                else:
                    listed.add(components[0])
    reasons.extend(history_contradictions("\n".join(fact_blocks)))
    for component, feature in facts["history"]:
        if not any(re.search(rf"\b{component}\b", block, re.I) and re.search(aliases[feature], block, re.I)
                   and sum(bool(re.search(rf"\b{c}\b", block, re.I)) for c in history_components) == 1
                   for block in fact_blocks):
            reasons.append(f"missing or unassociated commit fact: {component}/{feature}")
    return reasons


def git_judgment(kind: str, answer: str, facts: dict, root: Path, evidence: dict) -> list[str]:
    reasons = []
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
        for name, fact, stage in (("maths.py", r"subtract|减法|相减|求差|差值", "staged"),
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
        reasons.extend(history_contradictions(answer))
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
                if (re.search(r"最近|最新|\b(?:last|latest|recent)\b", clause, re.I)
                        and order != list(range(count))):
                    reasons.append("the declared most recent commits are not a contiguous history prefix")
    return reasons
