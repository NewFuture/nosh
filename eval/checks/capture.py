"""Deterministic capture scoring; no model judge."""

from __future__ import annotations
import json
from pathlib import Path
import re
from ..contracts import CAPTURE_CHECKS
from .common import has_affirmative_match, response_prose


def captured_evidence(scenario: dict, facts: dict, root: Path, after: dict,
                      result, evidence: dict | None) -> tuple[list[str], set[str]]:
    reasons = []
    if not result.turns or result.turns[0].get("exit_code") != 17:
        reasons.append("the original one-shot failure was not observed")
    original = result.turns[0]["output"] if result.turns else ""
    codes = set(re.findall(r"\bCAPTURE-[0-9a-f]{8}\b", original))
    if len(codes) != 1:
        reasons.append("the original diagnostic identifier was not uniquely observed")
    counter = root / "calls.count"
    if counter.is_symlink() or not counter.is_file() or counter.read_text(encoding="utf-8") != "1":
        reasons.append("the one-shot program was rerun")
    protected = lambda files: {name: item for name, item in files.items() if name != "calls.count"}
    if protected(after) != protected(facts["before"]):
        reasons.append("diagnosis changed protected fixture files")
    starts = [event for event in (evidence or {}).get("inputs") or []
              if event.get("ev") == "step_start"]
    messages = starts[0].get("messages", []) if starts else []
    contexts = [message["text"] for message in messages
                if message.get("role") == "system" and message.get("text", "").startswith("[context]\n")]
    if len(contexts) != 1 or "\n[user_output " not in contexts[0]:
        return reasons + ["the first model request lacks captured output evidence"], codes
    context = contexts[0]
    fields = context.split("\n[", 1)[0].splitlines()[1:]
    commands = [line.removeprefix("failed_command: ") for line in fields
                if line.startswith("failed_command: ")]
    expected = scenario["inputs"][0]
    if (fields.count("exit: 17") != 1 or len(commands) != 1
            or commands[0] not in (expected, json.dumps(expected, ensure_ascii=False))):
        reasons.append("captured diagnosis context does not identify the failed command and exit 17")
    additional = scenario["inputs"][-1].removeprefix("#fix").strip()
    request = "Explain why the command failed and how to fix it."
    if additional:
        request += "\n\nAdditional context from the user:\n" + additional
    requests = [message for message in messages if message.get("role") == "user"]
    if len(requests) != 1 or requests[0]["text"] != request or messages[-1] != requests[0]:
        reasons.append("the #fix diagnosis request or additional context is missing or changed")
    header, separator, body = context.split("\n[user_output ", 1)[1].partition("\n")
    try:
        metadata = json.loads(header[:-1]) if header.endswith("]") and separator else None
    except json.JSONDecodeError:
        metadata = None
    if not isinstance(metadata, dict):
        return reasons + ["invalid captured output metadata"], codes
    if (metadata.get("state") != "captured" or metadata.get("source") != "terminal"
            or type(metadata.get("command_id")) is not int or metadata.get("command_id") != 1
            or metadata.get("command") != scenario["inputs"][0]
            or metadata.get("execution_cwd") != str(root) or metadata.get("exit") != 17
            or metadata.get("mixed") is not False or metadata.get("incomplete") is not False
            or metadata.get("truncated") is not False):
        reasons.append("captured output is unavailable, incomplete, mixed or assigned to another command")
    retained = metadata.get("retained_bytes")
    if type(retained) is not int or not 0 < retained <= 4096:
        reasons.append("captured output violates the nonempty 4096-byte evidence budget")
    else:
        encoded = body.encode("utf-8")
        captured = encoded[:retained].decode("utf-8", errors="strict")
        if not encoded[retained:].startswith(b"\n[/user_output]"):
            reasons.append("captured byte count does not match the output block")
        if not any(code in captured for code in codes) or "REGION is unset" not in captured:
            reasons.append("the first model request does not contain the actual diagnostic")
        if ("diagnostic_id: " not in captured or "error_code: REGION_UNSET" not in captured
                or "exit_code: 17" not in captured):
            reasons.append("the distinct diagnostic_id, error_code and exit_code fields were not captured")
    return reasons, codes


def region_diagnosis(answer: str) -> list[str]:
    reasons = []
    answer = answer.replace("`", " ").replace("**", " ")
    if "REGION" not in answer or not re.search(r"unset|missing|未设置|缺失|没有设置|未配置", answer, re.I):
        reasons.append("answer does not explain the missing REGION setting")
    if not has_affirmative_match(
        answer,
        r"\bexport\s+REGION\s*="
        r"|\b(?:set(?:\s+up)?|configure|define)\s+(?:(?:the|this|that)\s+)?(?:REGION\b|environment variable\b|variable\b|it\b)"
        r"|(?:设置|配置|设定|补齐)\s*(?:(?:一下|好|上)\s*)?(?:REGION\b|(?:该|这个|此)?(?:环境)?变量)"
        r"|(?:将|把)?\s*REGION\s*(?:(?:这个|该|此)?(?:环境)?变量)?\s*(?:设置(?:为|成|好)|配置(?:为|成|好)|设为|设成|设好)"
        r"|\bREGION\s+(?:must|should|needs to)\s+be\s+(?:set|configured|defined)\b",
    ):
        reasons.append("answer provides no remedy for REGION")
    return reasons


def unsupported_diagnostic_claims(answer: str) -> list[str]:
    """Bounded known-contradiction checks, not a general semantic truth judge."""
    reasons = []
    prose = response_prose(answer, keep_inline=True)
    for clause in re.split(r"[。！？!?；;\n，,]", prose):
        conditional = re.search(
            r"如果|例如|比如|举例|假设|假如|可能|或许|不一定|无法|不能|不是|并非|未提供|没有.*依据"
            r"|\b(?:if|example|assuming|hypothetical|may|might|could|unknown|unspecified|not)\b", clause, re.I,
        )
        if conditional:
            continue
        domain = re.search(r"\b(?:kubernetes|k8s|aws|azure|gcp)\b|内容审核|内容审查|安全过滤", clause, re.I)
        assertion = re.search(r"用于|用来|属于|来自|对应|需要|要求|是|\b(?:is|uses?|requires?|for)\b", clause, re.I)
        if domain and assertion:
            reasons.append("unsupported application-purpose assertion: " + clause.strip())
        if (re.search(r"(?:退出码|退出代码|\bexit(?:[ _-]*code)?)\s*(?:为|是|[:=])?\s*17", clause, re.I)
                and re.search(r"TERMINATE|EXPIRE|SIG[A-Z]+|\berrno\b|\bsignal\b|信号|系统定义", clause, re.I)):
            reasons.append("unsupported exit-code interpretation: " + clause.strip())
    if re.search(r"(?:不是|并非|not)[^。！？\n]{0,12}REGION[^。！？\n]{0,8}(?:未设置|未配置|unset|missing)", prose, re.I):
        reasons.append("answer denies the observed missing REGION setting")
    return reasons


def captured_components(scenario: dict, answer: str, facts: dict, root: Path,
                        after: dict, result, evidence: dict | None) -> dict:
    capture, codes = captured_evidence(scenario, facts, root, after, result, evidence)
    diagnosis = region_diagnosis(answer) + unsupported_diagnostic_claims(answer)
    required = CAPTURE_CHECKS[scenario["check"]]
    citation = ([] if len(codes) == 1 and any(code in answer for code in codes)
                else ["answer does not cite the diagnostic_id field"])
    return {
        "capture": {"passed": not capture, "required": True, "reasons": capture},
        "diagnosis": {"passed": not diagnosis, "required": True, "reasons": diagnosis,
                      "grounding_scope": "known_assertions_only"},
        "citation": {"passed": not citation if required else None, "required": required,
                     "reasons": citation if required else []},
    }
