"""Deterministic agent scoring; no model judge."""

from __future__ import annotations
from pathlib import Path
import re
from ..driver import prompt_matches
from .common import FILE_NAME, HISTORY_ALIASES, mentioned_files
from .command_assist import check_archive


LANGUAGES = {
    "python": r"\bpython\b",
    "javascript": r"\b(?:javascript|js)\b",
    "rust": r"\brust\b",
    "shell": r"\b(?:shell|bash|sh)\b",
}


def python_count_reasons(answer: str, facts: dict) -> list[str]:
    directories = {parent.as_posix() for name in facts["before"] for parent in Path(name).parents} - {"."}
    headings = []
    paragraph = None
    paragraph_has_body = False
    reasons = []
    patterns = (
        r"(?<![\d.])([+-]?\d+(?:\.\d+)?)\s*(?:个\s*)?python\s*(?:文件|files?\b)",
        r"python\s*(?:文件|files?\b)\s*(?:[（(]\s*(?:共|共有)\s*|[:：]\s*|共有\s*)([+-]?\d+(?:\.\d+)?)",
    )

    def scope_in(text, default, heading=False):
        markers = []
        for pattern, directory in (
            (r"全项目|整个项目|整个仓库|全部目录|项目中|project[- ]wide|whole project|entire project|overall|grand total", None),
            (r"项目根目录|根目录|project root|root directory", "."),
        ):
            markers.extend((match.end(), directory) for match in re.finditer(pattern, text, re.I))
        for directory in directories:
            pattern = r"(?<![a-zA-Z0-9_./-])(?:\./)?" + re.escape(directory) + r"(/?)(?![a-zA-Z0-9_./-])"
            for match in re.finditer(pattern, text):
                if (match[1] or re.match(r"\s*(?:目录|文件夹|directory|folder)", text[match.end():], re.I)
                        or re.search(r"(?:目录|文件夹|directory|folder)\s*$", text[:match.start()], re.I)
                        or (heading and text.strip(" #*:：/") == directory)):
                    markers.append((match.end(), directory))
        recursive = bool(re.search(r"递归|含子目录|包含子目录|recursiv|including subdirectories", text, re.I))
        if not markers:
            return default[0], default[1] or recursive
        directory = max(markers, key=lambda marker: marker[0])[1]
        return directory, recursive

    for raw in answer.splitlines():
        line = raw.replace("**", "").replace("`", "").strip()
        if not line:
            if paragraph_has_body:
                paragraph = None
            continue
        has_count = any(re.search(pattern, line, re.I) for pattern in patterns)
        heading = re.match(r"^(#{1,6})\s+", line)
        if heading:
            level = len(heading[1])
            while headings and headings[-1][0] >= level:
                headings.pop()
            inherited = headings[-1][1] if headings else (None, False)
            headings.append((level, scope_in(line, inherited, heading=True)))
            paragraph = None
        current = headings[-1][1] if headings else (None, False)
        if not heading and not has_count and (
            line.endswith((":", "：")) or (raw.strip().startswith("**") and raw.strip().endswith("**"))
        ):
            paragraph = scope_in(line, current, heading=True)
            paragraph_has_body = False
        else:
            paragraph_has_body = True
        current = paragraph if paragraph is not None else current
        for clause in re.split(r"[，,；;。]", line):
            matches = sorted((match for pattern in patterns for match in re.finditer(pattern, clause, re.I)),
                             key=lambda match: match.start())
            for match in matches:
                count_text = match[1]
                if not re.fullmatch(r"\+?\d+", count_text):
                    reasons.append(f"invalid Python file count: {count_text}")
                    continue
                directory, recursive = scope_in(clause if len(matches) == 1 else clause[:match.start()], current)
                if directory is None:
                    expected = len(facts["python"])
                elif recursive:
                    expected = sum(directory == "." or name.startswith(directory + "/") for name in facts["python"])
                else:
                    expected = sum(Path(name).parent.as_posix() == directory for name in facts["python"])
                count = int(count_text)
                if count != expected:
                    reasons.append(f"incorrect Python file count: {count} in {directory or 'project'}, expected {expected}")
    return reasons


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
    line_unit = r"(?:lines?\b|loc\b|行)"
    line_count = r"(?<![\d.-])(\d+)\s*" + line_unit
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

    def language_counts(text, cell):
        def share(match):
            total = int(match["total"])
            if total != facts["total"]:
                reasons.append(f"incorrect total line count: {total}")
            return f"{match['part']} lines"
        for pattern in (
            rf"(?<![\d.-])(?P<part>\d+)(?:\s*{line_unit})?\s+(?:out\s+of|of)\s+"
            rf"(?:the\s+)?(?:total(?:\s+of)?\s+)?(?P<total>\d+)\s*{line_unit}",
            r"(?<![\d.-])(?P<total>\d+)\s*行\s*(?:中|内)(?:的|占|有)?\s*(?P<part>\d+)\s*行",
        ):
            text = re.sub(pattern, share, text)
        return counts(text, cell, bare=True)

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
                    found[language].extend(language_counts(description, count_cell))
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


def agent_judgment(scenario, answer, facts, root, after, result, metrics):
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
    elif kind == "port":
        if not re.search(rf"(?<!\d){facts['listener']['pid']}(?!\d)", answer):
            reasons.append("answer does not identify the fixture PID")
        python = list(re.finditer(
            r"(?<![a-z0-9_])python(?:[ \t]*(\d+(?:\.\d+)*))?(?![a-z0-9_])", answer, re.I,
        ))
        if not python or any(match[1] is not None and match[1].split(".")[0] != "3" for match in python):
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
        history_components = [component for component, _ in facts["history"]]
        for component, feature in facts["history"]:
            if not any(re.search(rf"\b{component}\b", block, re.I) and re.search(aliases[feature], block, re.I)
                       and sum(bool(re.search(rf"\b{c}\b", block, re.I)) for c in history_components) == 1
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

    return reasons
