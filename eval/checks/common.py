"""Deterministic common scoring; no model judge."""

from __future__ import annotations
from dataclasses import dataclass
from pathlib import Path
import re



@dataclass
class Verdict:
    passed: bool
    reasons: list[str]
    details: dict | None = None


FILE_NAME = re.compile(r"(?<![\w.-])(?:[\w.-]+/)*[\w.-]+\.(?:bin|txt|md|py|js|rs|sh)(?![\w-]|\.\w)")

HISTORY_ALIASES = {
    "pipeline": r"pipeline|管道", "approval": r"approv|确认|审批",
    "seed": r"seed|种子", "checksum": r"checksum|sha.?256|校验",
    "truncate": r"truncat|截断", "suggest": r"suggest|建议",
    "timeout": r"time.?out|超时", "offline": r"offline|离线",
}


def has_affirmative_match(answer: str, pattern: str) -> bool:
    previous_end = 0
    previous_negated = False
    for match in re.finditer(pattern, answer, re.I):
        prefix = re.split(r"[。！？!?；;\n，,]|(?<=\.)\s+", answer[:match.start()])[-1]
        suffix = re.split(r"[。！？!?；;\n，,]|(?<=\.)\s+", answer[match.end():], maxsplit=1)[0]
        negated = bool(re.search(
            r"(?:不要|不应|不必|无需|不需要|不能|没有|尚未|并非|不是|未|勿)"
            r"\s*(?:再|去|尝试)?\s*$"
            r"|(?:不要|不应|不能|别)(?:将|把)[^。！？!?；;\n，,]{0,30}$"
            r"|\b(?:not|never|cannot|can't|don't|doesn't|didn't|without)"
            r"\s+(?:have\s+to\s+|need\s+to\s+|try\s+to\s+)?$", prefix, re.I,
        )) or bool(previous_negated and re.fullmatch(
            r"\s*(?:or|and|或|或者|也不要)\s*", answer[previous_end:match.start()], re.I,
        )) or bool(re.search(
            r"(?:并|也|仍然|依然)?(?:不能|不会|无法)(?:解决|修复|奏效)"
            r"|(?:没有用|无效|不起作用|不必要)"
            r"|\b(?:does not|doesn't|will not|won't|cannot|can't)\s+(?:fix|resolve|help|work)\b"
            r"|\b(?:is|would be)\s+(?:ineffective|unnecessary)\b", suffix, re.I,
        ))
        if not negated:
            return True
        previous_end, previous_negated = match.end(), negated
    return False


def history_contradictions(answer: str) -> list[str]:
    """Reject known reversed commit actions, not arbitrary semantic claims."""
    feature = "(?:" + "|".join(HISTORY_ALIASES.values()) + ")"
    removed = r"(?:删除|移除|取消|禁用|撤销|去掉|\b(?:remove[ds]?|delete[ds]?|disable[ds]?|drop(?:s|ped)?)\b)"
    text = answer.replace("`", "").replace("**", "")
    text = re.sub(
        r"(?:没有|并未|未|不要)\s*" + removed
        + r"|\b(?:not|never)\s+" + removed, "", text, flags=re.I,
    )
    if re.search(
        removed + r"\s*(?:(?:the|support for|feature for)\s+)*" + feature
        + "|" + feature + r"[^。！？!?；;\n,，]{0,40}(?:已被|被|已|was |were |is |has been )" + removed
        + r"|(?:没有|并未|未)(?:新增|添加|支持)[^。！？!?；;\n,，]{0,20}" + feature,
        text, re.I,
    ):
        return ["commit description reverses or denies the recorded feature"]
    return []


def response_prose(answer: str, keep_inline: bool = False, keep_paths: bool = False) -> str:
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
    if keep_inline:
        # Literal directory operands must stay distinct from sentence punctuation.
        prose = re.sub(r"(`+)(.*?)\1", lambda m: repr(m[2]) if m[2] in (".", "..") else m[2], prose)
    else:
        prose = re.sub(r"(`+).*?\1", "", prose)
    if not keep_paths:
        prose = re.sub(r"https?://[^\s，。！？]+", "", prose)
        prose = re.sub(r"(?<![\w])(?:[\w.?-]+[/\\])+[\w.?-]*", "", prose)
        prose = re.sub(r"[\w.?-]+\.(?:py|js|rs|md|txt|json|toml|log|bin)(?!\w)", "", prose)
    return prose.strip()


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
