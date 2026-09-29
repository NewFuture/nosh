"""Deterministic agent scoring; no model judge."""

from __future__ import annotations
import re
from ..driver import prompt_matches
from .common import FILE_NAME, has_affirmative_match, mentioned_files
from .command_assist import check_archive
from .files import cwd_file_reasons, file_size_reasons, line_counts, python_count_reasons
from .git import history_summary
from .workflows import agent_workflow_judgment, followup_context_judgment


def agent_judgment(scenario, answer, facts, root, after, result, metrics, evidence=None):
    kind = scenario["check"]
    reasons = []
    if kind == "largest":
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
        reasons.extend(file_size_reasons(answer, facts))
    elif kind == "port":
        if not re.search(rf"(?<!\d){facts['listener']['pid']}(?!\d)", answer):
            reasons.append("answer does not identify the fixture PID")
        python = list(re.finditer(
            r"(?<![a-z0-9_])python(?:[ \t]*(\d+(?:\.\d+)*))?(?![a-z0-9_])", answer, re.I,
        ))
        if not python or any(match[1] is not None and match[1].split(".")[0] != "3" for match in python):
            reasons.append("answer does not identify the Python listener")
        pids = re.findall(r"\bPID\s*(?:[:=：是为]\s*)?(\d+)", answer, re.I)
        if any(int(pid) != facts["listener"]["pid"] for pid in pids):
            reasons.append("answer attributes the listener to another PID")
        for clause in re.split(r"[。！？!?；;\n，,]", answer):
            if not re.search(rf"python|(?<!\d){facts['listener']['pid']}(?!\d)", clause, re.I):
                continue
            if re.search(
                r"(?:不是|并非|并未|没有|未|is not|isn't|does not|doesn't|not)"
                r"\s*(?:(?:由|被|这个|该|the|a|an|currently|actually)\s*)?"
                rf"(?:python|监听|占用|listening|bound|(?:PID\s*)?{facts['listener']['pid']})",
                clause, re.I,
            ):
                reasons.append("answer denies the observed listener")
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
        empty_section = False
        classified_python = set()
        incorrectly_classified = []
        misclassified_python = []
        for line in answer.splitlines():
            plain = line.replace("**", "").replace("`", "").strip()
            line_files = mentioned_files(line, facts["before"])
            heading = (plain.endswith((":", "：")) or re.match(r"^#{1,6}\s", plain)
                       or (line.strip().startswith("**") and line.strip().endswith("**")))
            if heading:
                empty_section = False
            absent = re.search(
                r"(?:\bno\s+|\bwithout\s+|没有\s*|无\s*|未(?:发现|找到)\s*|不存在\s*)"
                r"(?:任何\s*)?python\s*(?:文件|files?\b)"
                r"|python\s*(?:文件|files?\b)\s*[:：]?\s*(?:不存在|没有|未找到|无)"
                r"|(?<![\d.])0\s*(?:个\s*)?python\s*(?:文件|files?\b)",
                plain, re.I,
            )
            if absent:
                classification = False
                if not line_files:
                    empty_section = True
            elif re.search(r"(?:non[- ]|not\s+)python|(?:非|不是|不属于)\s*python|(?:其他|其余).*文件|other.*files", plain, re.I):
                classification = False
                if not line_files:
                    python_section = False
            elif re.search(r"python\s*文件|python\s*files|个\s*python", plain, re.I):
                python_section = classification = True
                empty_section = False
            elif re.fullmatch(
                r"目录|目录结构|项目结构|文件树|directory|directory structure|directory listing|project structure|file tree",
                line.strip().strip("*# ").rstrip(":："), re.I,
            ):
                python_section = classification = None
                empty_section = False
            else:
                classification = False if empty_section else python_section
            if classification is True:
                classified_python.update(name for name in line_files if name.endswith(".py"))
                incorrectly_classified.extend(name for name in line_files if not name.endswith(".py"))
            elif classification is False:
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
        reasons.extend(python_count_reasons(answer, facts))
    elif kind == "typos":
        expected = scenario["corrections"]
        if len(result.turns) != len(expected) or any(
            not prompt_matches(turn["edit_line"], corrected)
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
        if not has_affirmative_match(answer,
            r"\b(?:create|provide|add|copy|place|put|correct|fix|change|set|specify|modify)\b"
            r"|创建|提供|添加|复制|放入|放到|放置|修正|修改|设置|指定|切换到|在正确目录(?:中)?运行",
        ):
            reasons.append("answer provides no recognized remedy")
        if (has_affirmative_match(answer,
                r"\btouch\s+[`'\"]?config\.json"
                r"|\bcreate\s+(?:an?\s+)?empty\b|(?:创建|新建|生成)\s*(?:一个)?空(?:的)?文件")
                and not re.search(r"\{\s*\}|valid\s+JSON|有效.{0,8}JSON|写入.{0,20}JSON|populate", answer, re.I)):
            reasons.append("an empty config.json is not a valid JSON repair")
    elif kind == "history":
        reasons.extend(history_summary(answer, facts))
    elif kind == "archive":
        reasons.extend(check_archive(answer, root, facts["before"], after))
    elif kind in ("cwd", "cwd-follow-up"):
        if result.pwd != str(root / "data"):
            reasons.append(f"physical pwd is {result.pwd!r}, not data")
        reasons.extend(cwd_file_reasons(answer, facts))
        if kind == "cwd-follow-up":
            reasons.extend(followup_context_judgment(scenario, root, evidence))
    elif kind in ("denied-rename", "config-lookup", "config-missing"):
        reasons.extend(agent_workflow_judgment(scenario, answer, facts, root, result, evidence))
    else:
        raise ValueError(f"unknown check: {kind}")

    return reasons
