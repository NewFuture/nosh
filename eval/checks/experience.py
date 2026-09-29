"""Deterministic experience scoring; no model judge."""

from __future__ import annotations
import re
from .common import response_prose


def history_prose(answer: str, facts: dict) -> str:
    log = facts.get("git_log", "")
    subjects = set(re.findall(r"(?m)^ {4}(\S.*)$", log))
    for subject in sorted(subjects, key=len, reverse=True):
        words = []
        for word in subject.split():
            stem = word.rstrip(":,.;!?")
            words.append(r"[`*_]*" + re.escape(stem) + r"[`*_]*" + re.escape(word[len(stem):]))
        # Match the full source text before paths or inline code can erase its identity.
        pattern = r"(?<!\w)" + r"\s+".join(words) + r"[`*_]*(?!\w)"
        answer = re.sub(pattern, "", answer)
    commits = {sha.lower() for sha in facts.get("commit_ids", [])}
    commits.update(re.findall(r"(?m)^commit ([0-9a-f]{40})\b", log))
    return re.sub(
        r"(?<!\w)[0-9a-f]{7,40}(?!\w)",
        lambda match: "" if any(commit.startswith(match[0].lower()) for commit in commits) else match[0],
        response_prose(answer), flags=re.I,
    )


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


def experience(scenario: dict, answer: str, metrics: dict, *, facts: dict | None = None) -> dict | None:
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
        r"|(?:如果|若|如)[你您]?(?:还)?(?:有)?(?:需要?|希望|想)[^。！？!?；;\n]*"
        r"(?:请|可以|随时)[你您]?(?:再|随时)?(?:告诉我|告知我|联系我)"
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
        "", history_prose(answer, facts) if facts is not None else prose,
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
