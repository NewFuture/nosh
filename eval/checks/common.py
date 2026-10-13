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
            r"|\b(?:not|never|cannot|can't|don't|doesn't|didn't|isn't|aren't|wasn't|weren't|without)"
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


def completion_contradictions(answer: str, action: str, test_count: int | None = None) -> list[str]:
    """Reject explicit contradictions of an observed success, not arbitrary prose."""
    labels = {
        "build": r"构建|编译|\bbuild\b|\bcompilation\b",
        "test": r"测试|用例|\btests?\b|\btest suite\b",
        "clean": r"清理|\bcleanup\b|\bcleaning\b",
        "rename": r"重命名|改名|\brenam(?:e|ing)\b",
        "commit": r"提交|\bcommit\b",
        "repair": r"修复|配置|\brepair\b|\bconfiguration\b",
    }[action]
    reasons = []
    prose = response_prose(answer)
    clauses = re.split(r"[。！？!?；;\n，,]|(?<=\.)\s+|\bbut\b|但是|但", prose, flags=re.I)
    resolved = (
        rf"(?:{labels})\s*(?:全部|均|都)?(?:现在|目前|现已|已经|已)\s*"
        r"(?:全部|均|都)?(?:成功)?(?:通过|成功|完成)"
        rf"|(?:{labels})\s+(?:(?:are|is|have|has)\s+)?now\s+"
        r"(?:(?:all|successfully)\s+)?(?:pass(?:ed)?|succeed(?:ed)?|complete[ds]?|successful)\b"
    )
    uncertain = (
        r"如果|假如|假设|不确定|未确认|未验证|预计|可能|希望"
        r"|\b(?:if|hypothetical|may|might|could|would|should|will|expect|hope)\b"
    )
    last_resolution = max(
        (index for index, clause in enumerate(clauses)
         if not re.search(uncertain, clause, re.I)
         and has_affirmative_match(clause, resolved)),
        default=-1,
    )
    for index, clause in enumerate(clauses):
        if re.search(
            r"之前|此前|原先|原本|上次|最初|第一次|如果|假如|例如|假设"
            r"|\b(?:previously|earlier|initially|originally|previous|prior|if|example|hypothetical)\b",
            clause, re.I,
        ):
            continue
        # A causal explanation followed by an explicit resolved state describes
        # the earlier failure; an assertion that it still fails does not.
        if (index < last_resolution
                and re.search(r"原因|导致|造成|引起|\b(?:because|caused?|causing|due to)\b", clause, re.I)
                and not re.search(r"仍|依然|目前|当前|现在|再次|\b(?:still|currently|now|again)\b", clause, re.I)):
            continue
        current = re.sub(
            r"(?<![\d.])0\s*(?:(?:个|项)?(?:测试|用例)(?:失败|未通过)"
            r"|(?:tests?\s+)?(?:failed|failures?)\b)",
            "", clause, flags=re.I,
        )
        failed = re.search(
            rf"(?:{labels})\s*(?:任务|操作|执行|运行)?\s*(?:全部|均|都)?"
            r"(?:仍然?|依然|目前|当前|现在|再次)?"
            r"(?:未通过|未成功|未完成|失败)"
            rf"|(?:{labels})\s+(?:(?:has|have|is|are|was|were|all|still|currently|now|again)\s+)*"
            r"(?:failed\b|unsuccessful\b|not (?:run|executed|completed|successful)\b|did not pass\b)",
            current, re.I,
        )
        absent = re.search(
            rf"(?:未|没有|尚未|并未)\s*(?:执行|运行|进行|完成|开始)?"
            rf"\s*(?:任何|全部|本次|相关|项目)?\s*(?:{labels})"
            rf"|\b(?:did not|didn't|have not|haven't|could not|couldn't|cannot|can't)\s+"
            rf"(?:run|execute|perform|complete|finish|create|make)\s+(?:(?:any|the|a|new)\s+)*(?:{labels})",
            current, re.I,
        )
        if failed or absent:
            reasons.append(f"answer contradicts the successful {action}: {clause.strip()}")
        if action == "test" and test_count is not None:
            totals = re.findall(
                r"(?:共|总计|一共|运行了?|执行了?)\s*(\d+)\s*(?:个|项)?(?:测试|用例)"
                r"|\b(?:ran|executed|total)\s+(\d+)\s+(?:tests?|cases?)\b",
                clause, re.I,
            )
            if any(int(left or right) != test_count for left, right in totals):
                reasons.append(f"answer reports an incorrect test count: {clause.strip()}")
            if not re.search(r"审批|权限|approval|permission", clause, re.I):
                counts = [(state.lower(), int(number)) for number, state in re.findall(
                    r"(?<![\d.])(\d+)\s*(?:(?:tests?|cases?)\s+)?(passed|failed|skipped|ignored)\b",
                    clause, re.I,
                )]
                counts.extend((state, int(number)) for state, number in re.findall(
                    r"(通过|失败|跳过|忽略)\s*[:：]\s*(\d+)", clause,
                ))
                counts.extend((state, int(number)) for number, state in re.findall(
                    r"(?<![\d.])(\d+)\s*(?:个|项)?(?:测试|用例)\s*(通过|失败|跳过|忽略)", clause,
                ))
                if any(number != (test_count if state in ("passed", "通过") else 0)
                       for state, number in counts):
                    reasons.append(f"answer reports incorrect test results: {clause.strip()}")
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
