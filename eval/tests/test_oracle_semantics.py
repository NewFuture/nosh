from __future__ import annotations

import copy
from pathlib import Path
import tempfile
import unittest

from eval import checks, driver, fixtures


class FixtureOracleTestCase(unittest.TestCase):
    def prepare(self, kind):
        temporary = tempfile.TemporaryDirectory(prefix="nosh-oracle-semantics-")
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name) / "fixture"
        self.facts = fixtures.create(self.root, kind)
        self.result = driver.Result(exit_code=0)
        self.metrics = {"task_status": "completed", "steps": 1, "confirmations": 0}

    def grade(self, answer, *, after=None, evidence=None):
        return checks.judge(
            self.scenario, answer, self.facts, self.root,
            self.facts["before"] if after is None else after,
            self.result, self.metrics, evidence if evidence is not None else self.evidence,
        )


class LineCountSemanticsTests(FixtureOracleTestCase):
    TABLE = (
        "| Language | File | Lines |\n"
        "| --- | --- | ---: |\n"
        "| Python | main.py | 5 |\n"
        "| Python | lib/maths.py | 5 |\n"
        "| Python | tools/report.py | 5 |\n"
        "| JS | web/app.js | 4 |\n"
        "| Rust | src/main.rs | 6 |\n"
        "| Shell | scripts/check.sh | 4 |"
    )
    SUMMARY = "Python: 15 lines; JS: 4 lines; Rust: 6 lines; Shell: 4 lines\nTotal: 29 lines in 6 files."
    SCOPES = (
        "**Python (.py files):**\n- main.py: 5 lines\n- tools/report.py: 5 lines\n"
        "- Total: 10 lines in 2 files\n\n"
        "**Python (in lib):**\n- lib/maths.py: 5 lines\n- Total: 5 lines in 1 file\n\n"
        "**Summary:**\n"
    )

    def setUp(self):
        self.prepare("project")
        self.facts["file_lines"] = {
            name: len((self.root / name).read_text(encoding="utf-8").splitlines())
            for name in self.facts["before"]
        }

    def test_per_file_table_with_language_and_overall_totals(self):
        self.assertEqual(checks.line_counts(self.TABLE + "\n" + self.SUMMARY, self.facts), [])
        self.assertEqual(checks.line_counts(
            self.TABLE + "\n| Python | 3 | 15 |\n| JS | 1 | 4 |\n| Rust | 1 | 6 |"
            "\n| Shell | 1 | 4 |\n| Total | 6 | 29 |", self.facts,
        ), [])

    def test_complete_per_file_table_can_supply_the_language_totals(self):
        for answer in (self.TABLE, self.TABLE + "\nOverall: 29 lines across 6 files."):
            with self.subTest(answer=answer):
                self.assertEqual(checks.line_counts(answer, self.facts), [])

    def test_column_order_units_aliases_and_scoped_tables(self):
        reversed_columns = "\n".join(
            "| " + " | ".join(reversed([cell.strip() for cell in row.strip("|").split("|")])) + " |"
            for row in self.TABLE.splitlines()
        )
        chinese = self.TABLE.replace("Language", "语言").replace("File", "文件").replace("Lines", "行数")
        chinese = chinese.replace("| 5 |", "| 5 行 |").replace("| 4 |", "| 4 行 |").replace("| 6 |", "| 6 行 |")
        scoped = "\n".join(
            f"## {language}\n| File | Lines |\n" + "\n".join(
                f"| {name} | {count} |" for name, count in self.facts["file_lines"].items()
                if Path(name).suffix == suffix
            )
            for language, suffix in (("Python", ".py"), ("JavaScript", ".js"), ("Rust", ".rs"), ("Bash", ".sh"))
        )
        for answer in (reversed_columns, chinese, scoped, self.TABLE.replace("/", "\\")):
            with self.subTest(answer=answer):
                self.assertEqual(checks.line_counts(answer, self.facts), [])

    def test_grouped_file_rows_and_inline_file_details(self):
        grouped = self.TABLE.replace(
            "| Python | main.py | 5 |\n| Python | lib/maths.py | 5 |\n| Python | tools/report.py | 5 |",
            "| Python | main.py, lib/maths.py, tools/report.py | 15 |",
        )
        inline = (
            "Python: main.py: 5 lines, lib/maths.py: 5 lines, tools/report.py: 5 lines\n"
            "JS: web/app.js: 4 lines; Rust: src/main.rs: 6 lines; Shell: scripts/check.sh: 4 lines"
        )
        self.assertEqual(checks.line_counts(grouped, self.facts), [])
        self.assertEqual(checks.line_counts(inline, self.facts), [])
        self.assertEqual(checks.line_counts(inline.replace(" lines", ""), self.facts), [])

    def test_named_file_counts_are_checked_even_in_optional_prose(self):
        for suffix in ("5 lines", "5"):
            answer = f"**Python:**\n- main.py: {suffix}\n\n**Summary:**\n" + self.SUMMARY
            self.assertEqual(checks.line_counts(answer, self.facts), [])
            for wrong in ("4", "-5", "1.5"):
                with self.subTest(suffix=suffix, wrong=wrong):
                    self.assertTrue(checks.line_counts(answer.replace(f"main.py: {suffix}", f"main.py: {wrong}"), self.facts))

    def test_wrong_file_counts_cannot_hide_behind_correct_aggregates(self):
        for wrong in (
            self.TABLE.replace("| main.py | 5 |", "| main.py | 4 |"),
            self.TABLE.replace("| main.py | 5 |", "| main.py | 4 |").replace("| lib/maths.py | 5 |", "| lib/maths.py | 6 |"),
            self.TABLE.replace("| main.py | 5 |", "| main.py | -5 |"),
            self.TABLE.replace("| main.py | 5 |", "| main.py | 1.5 |"),
            self.TABLE.replace("| main.py | 5 |", "| main.py | unknown |"),
            self.TABLE.replace("| main.py | 5 |", "| main.py | 5 lines and 6 lines |"),
        ):
            with self.subTest(answer=wrong):
                self.assertTrue(checks.line_counts(wrong + "\n" + self.SUMMARY, self.facts))

    def test_duplicate_omitted_and_unknown_files_are_not_a_complete_table(self):
        for wrong in (
            self.TABLE.replace("| Python | lib/maths.py | 5 |\n", ""),
            self.TABLE.replace("lib/maths.py", "main.py"),
            self.TABLE + "\n| Python | main.py | 5 |",
            self.TABLE.replace("lib/maths.py", "missing.py"),
            self.TABLE.replace("| Shell | scripts/check.sh | 4 |", ""),
            self.TABLE.replace("main.py | 5", "main.py, main.py | 10", 1),
        ):
            for summary in ("", "\n" + self.SUMMARY):
                with self.subTest(answer=wrong, summary=bool(summary)):
                    self.assertTrue(checks.line_counts(wrong + summary, self.facts))

    def test_explicit_and_negated_file_classifications_are_checked(self):
        for wrong in (
            self.TABLE.replace("| Python | main.py", "| Rust | main.py"),
            self.TABLE.replace("| JS | web/app.js", "| Python | web/app.js"),
            self.TABLE.replace("| Python |", "| Non-Python |"),
            self.TABLE.replace("| Python |", "| 不是 Python |"),
            "**非 Python 文件：**\n- main.py: 5 lines\n\n**Summary:**\n" + self.SUMMARY,
        ):
            with self.subTest(answer=wrong):
                self.assertTrue(checks.line_counts(wrong + "\n" + self.SUMMARY, self.facts))

    def test_wrong_aggregates_and_contradictory_numbers_still_fail(self):
        for wrong in (
            self.SUMMARY.replace("15 lines", "16 lines"),
            self.SUMMARY.replace("29 lines", "30 lines"),
            self.SUMMARY.replace("6 files", "7 files"),
            self.SUMMARY.replace("15 lines", "15 lines, actually 14 lines"),
            self.SUMMARY.replace("Python: 15 lines", "Python: 4 files, 15 lines"),
            self.SUMMARY + "\n| Language | Files | Lines |\n| Total | 7 | 29 |",
            self.SUMMARY + "\n| Language | Files | Lines |\n| Total | 6 | 30 |",
        ):
            with self.subTest(answer=wrong):
                self.assertTrue(checks.line_counts(self.TABLE + "\n" + wrong, self.facts))

    def test_legacy_aggregates_optional_details_and_split_subtotals(self):
        answers = (
            self.SUMMARY,
            "**Python:**\n- main.py: 5 lines\n- Total: 15 lines\n\n**Summary:**\n" + self.SUMMARY,
            self.SCOPES + self.SUMMARY,
            self.TABLE,
            self.TABLE + "\n" + self.SUMMARY,
        )
        legacy = {key: value for key, value in self.facts.items() if key != "file_lines"}
        for facts in (self.facts, legacy):
            for answer in answers:
                with self.subTest(file_lines="file_lines" in facts, answer=answer):
                    self.assertEqual(checks.line_counts(answer, facts), [])

    def test_subtotals_cannot_offset_errors_or_omit_named_scope_members(self):
        for wrong in (
            self.SCOPES.replace("10 lines", "9 lines").replace("Total: 5 lines", "Total: 6 lines"),
            self.SCOPES.replace("- tools/report.py: 5 lines\n", ""),
            self.SCOPES.replace("lib/maths.py", "main.py"),
            self.SCOPES.replace("- Total: 10 lines in 2 files\n", ""),
            self.SCOPES.replace("Total: 5 lines", "Total: 5 lines and 5 lines"),
        ):
            with self.subTest(answer=wrong):
                self.assertTrue(checks.line_counts(wrong + self.SUMMARY, self.facts))


class GitDiffSemanticsTests(FixtureOracleTestCase):
    ANSWER = "maths.py 新增 subtract 减法函数；README.md 补充 unittest 测试说明。"

    def setUp(self):
        self.prepare("python")
        self.facts["git_before"] = {"head": "original", "index": "captured staged maths.py", "status": "dirty"}
        self.scenario = {"check": "git-diff"}
        self.evidence = {"executions": [], "final_state": {"git": dict(self.facts["git_before"])}}

    def test_staging_labels_are_optional_supporting_information(self):
        for answer in (
            self.ANSWER,
            self.ANSWER.replace("；", "\n"),
            self.ANSWER.replace("maths.py 新增", "maths.py 已暂存，新增"),
            self.ANSWER.replace("README.md 补充", "README.md 未暂存，补充"),
            "已暂存：\n- maths.py：新增 subtract 减法函数。\n\n未暂存：\n- README.md：补充 unittest 测试说明。",
            "已暂存 maths.py 新增 subtract；未暂存 README.md 补充 unittest 测试说明。",
            "maths.py 已暂存，新增 subtract，README.md 补充 unittest 测试说明。",
            "已暂存：\nmaths.py 新增 subtract。\n\n其他改动：\nREADME.md 补充 unittest 测试说明。",
            "maths.py：\n- 新增 subtract 减法函数（已暂存）。\nREADME.md：\n- 补充 unittest 测试说明（未暂存）。",
            "| File | Staged changes | Description |\n| maths.py | staged | 新增 subtract |"
            "\n| README.md | unstaged | 补充 unittest 测试说明 |",
        ):
            with self.subTest(answer=answer):
                verdict = self.grade(answer)
                self.assertTrue(verdict.passed, verdict.reasons)

    def test_wrong_or_contradictory_staging_is_rejected(self):
        for answer in (
            self.ANSWER.replace("maths.py 新增", "maths.py 未暂存，新增"),
            self.ANSWER.replace("README.md 补充", "README.md 已暂存，补充"),
            self.ANSWER.replace("maths.py 新增", "maths.py staged and unstaged，新增"),
            self.ANSWER.replace("maths.py 新增", "未暂存 maths.py 已暂存，新增"),
            "未暂存：\nmaths.py 已暂存，新增 subtract；README.md 补充 unittest 测试说明。",
            "maths.py 新增 subtract。\n该文件未暂存。\nREADME.md 补充 unittest 测试说明。",
            self.ANSWER + "\nREADME.md 已暂存。",
            "未暂存：\nmaths.py 新增 subtract。\n已暂存：\nREADME.md 补充 unittest 测试说明。",
        ):
            with self.subTest(answer=answer):
                self.assertFalse(self.grade(answer).details["facts"]["passed"])

    def test_changed_facts_must_stay_associated_with_the_expected_file(self):
        for answer in (
            "maths.py 补充 unittest 测试说明；README.md 新增 subtract 减法函数。",
            "maths.py 和 README.md 有修改。\n新增 subtract，补充 unittest 测试说明。",
            "新增 subtract 减法函数；README.md 补充 unittest 测试说明。",
            "maths.py 新增 subtract 减法函数。",
            "maths.py 与 README.md 都有改动。",
            self.ANSWER.replace("maths.py", "othermaths.py"),
            self.ANSWER.replace("README.md", "README.md.bak"),
        ):
            with self.subTest(answer=answer):
                self.assertFalse(self.grade(answer).details["facts"]["passed"])

    def test_negated_or_removed_changes_do_not_count_as_the_expected_additions(self):
        for answer in (
            self.ANSWER.replace("maths.py 新增", "maths.py 没有新增"),
            self.ANSWER.replace("maths.py 新增", "maths.py 删除"),
            self.ANSWER.replace("README.md 补充", "README.md 移除"),
            self.ANSWER.replace("README.md 补充", "unchanged README.md 补充"),
            self.ANSWER + "\nmaths.py 没有改动。",
            self.ANSWER + "\nREADME.md unittest documentation was deleted.",
        ):
            with self.subTest(answer=answer):
                self.assertFalse(self.grade(answer).details["facts"]["passed"])

    def test_summary_still_requires_unchanged_source_and_git_state(self):
        changed = copy.deepcopy(self.facts["before"])
        changed["maths.py"]["sha256"] = "modified"
        self.assertFalse(self.grade(self.ANSWER, after=changed).details["facts"]["passed"])
        evidence = copy.deepcopy(self.evidence)
        evidence["final_state"]["git"]["index"] = "different index"
        self.assertFalse(self.grade(self.ANSWER, evidence=evidence).details["facts"]["passed"])


class ClarificationSemanticsTests(FixtureOracleTestCase):
    def setUp(self):
        self.prepare("python")
        self.scenario = {
            "check": "clarification", "fixture": "python",
            "expect": {"max_steps": 2, "max_confirmations": 0, "response_language": "zh", "final_question": "require"},
        }
        self.evidence = {"executions": []}

    def test_missing_goal_or_input_can_be_requested_imperatively(self):
        for answer in (
            "请说明具体目标，我再处理。",
            "请提供需要处理的文件和预期结果。",
            "告诉我具体任务，我再继续。",
            "需要你补充报错信息。",
            "请给出具体任务目标。",
            "你希望我处理什么具体任务？",
            "请问具体需求是什么",
            "请说明具体目标。\n\n我可以编译或测试。",
        ):
            with self.subTest(answer=answer):
                verdict = self.grade(answer)
                self.assertTrue(verdict.passed, verdict.reasons)
                detail = verdict.details["experience"]["final_question"]
                self.assertTrue(detail["actual"])
                self.assertTrue(detail["clarification_request"])
                self.assertTrue({"passed", "actual", "expected", "closing"} <= detail.keys())

    def test_empty_generic_and_specific_continuation_requests_are_not_clarification(self):
        for answer in (
            "", "准备好了。", "随时可以帮忙。", "你的目标尚不明确。", "请提供帮助。",
            "是否需要我继续？", "您希望我继续构建吗？", "要不要运行测试？",
            "请告诉我是否需要编译项目。", "请确认是否继续运行测试。",
        ):
            with self.subTest(answer=answer):
                verdict = self.grade(answer)
                self.assertFalse(verdict.details["facts"]["passed"])
                self.assertFalse(verdict.details["experience"]["final_question"]["passed"])

    def test_quoted_and_code_requests_are_not_the_assistants_clarification(self):
        for answer in (
            "> 请说明具体目标，我再处理。",
            "准备好了。\n```text\n请说明具体目标。\n```",
            "准备好了。`你希望我处理什么任务？`",
        ):
            with self.subTest(answer=answer):
                verdict = self.grade(answer)
                self.assertFalse(verdict.details["facts"]["passed"])
                self.assertFalse(verdict.details["experience"]["final_question"]["passed"])

    def test_semantic_clarification_does_not_bypass_chinese_body_requirement(self):
        verdict = self.grade("Please specify the task.")
        self.assertTrue(verdict.details["facts"]["passed"])
        self.assertTrue(verdict.details["experience"]["final_question"]["passed"])
        self.assertFalse(verdict.details["experience"]["response_language"]["passed"])
        self.assertFalse(verdict.passed)

    def test_clear_tasks_still_reject_statement_style_followup_requests(self):
        scenario = copy.deepcopy(self.scenario)
        scenario["check"] = "python-test"
        scenario["expect"]["final_question"] = "forbid"
        for answer in (
            "测试完成。请确认。",
            "测试完成。请确认是否继续构建。",
            "测试完成。请回复确认，我再继续。",
            "测试完成。告诉我是否继续。",
            "测试完成。确认后我再进行下一步。",
            "测试完成。请说明具体目标，我再处理。",
            "测试完成。Please confirm before I continue.",
        ):
            with self.subTest(answer=answer):
                self.assertFalse(checks.experience(scenario, answer, self.metrics)["final_question"]["passed"])
        for answer in (
            "测试完成。下一步建议：检查配置是否需要更新。",
            "测试完成。\n\n```text\n请确认是否继续。\n```\n> 你希望我处理什么任务？",
        ):
            with self.subTest(answer=answer):
                self.assertTrue(checks.experience(scenario, answer, self.metrics)["final_question"]["passed"])


if __name__ == "__main__":
    unittest.main()
