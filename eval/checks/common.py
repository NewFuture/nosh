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


def response_prose(answer: str, keep_inline: bool = False) -> str:
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
    prose = re.sub(r"`+", "", prose) if keep_inline else re.sub(r"(`+).*?\1", "", prose)
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
