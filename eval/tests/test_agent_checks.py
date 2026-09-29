from __future__ import annotations
import json
from pathlib import Path
import sys
import tempfile
import unittest
from eval import approval, checks, driver, fixtures, run


@unittest.skipUnless(sys.platform == "linux", "Linux fixture tools")
class CheckTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name)
        self.result = driver.Result(exit_code=0)
        self.metrics = {"task_status": "completed", "steps": 1}

    def tearDown(self):
        self.temp.cleanup()

    def judge(self, kind, fixture, answer):
        root = self.base / fixture
        facts = fixtures.create(root, fixture)
        return checks.judge({"check": kind}, answer, facts, root, facts["before"], self.result, self.metrics)

    def test_largest_order_and_no_echo_shortcut(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        def verdict(answer):
            return checks.judge({"check": "largest"}, answer, facts, root, facts["before"], self.result, self.metrics)
        self.assertTrue(verdict("1. data/dump.bin\n2. video.bin\n3. cache/archive.bin").passed)
        self.assertTrue(verdict("1. data/dump.bin\n2. video.bin\n3. cache/archive.bin\n\n"
                                "These exclude notes.txt, which is only 100 bytes.").passed)
        self.assertFalse(verdict("dump.bin, archive.bin, video.bin").passed)
        self.assertFalse(verdict("Done.").passed)

    def test_largest_separates_reference_list_but_keeps_all_ranked_items(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        def verdict(answer):
            return checks.judge({"check": "largest"}, answer, facts, root, facts["before"], self.result, self.metrics)
        ranking = "1. data/dump.bin\n\n2. video.bin\n\n3. cache/archive.bin"
        reference = "\n\nFor reference, there's also a smaller file:\n- notes.txt - 100 bytes"
        self.assertTrue(verdict(ranking + reference).passed)
        self.assertFalse(verdict(ranking + "\n4. notes.txt" + reference).passed)
        self.assertFalse(verdict(ranking + "\n\nAdditional entries:\n4. notes.txt").passed)
        self.assertFalse(verdict(ranking + "\n\nAdditional ranked files:\n- notes.txt").passed)
        self.assertFalse(verdict(ranking + "\n\nAdditional ranked files:\n"
                                "| File | Size |\n|---|---|\n| notes.txt | 100 bytes |").passed)
        correction = "\n\nActually, the corrected ranking is:\n- notes.txt\n- video.bin\n- data/small.bin"
        self.assertFalse(verdict(ranking + correction).passed)
        self.assertFalse(verdict(ranking + reference + correction).passed)
        for indent in ("    ", "\t"):
            with self.subTest(indent=indent):
                indented_correction = "\n".join(indent + line if line else "" for line in correction.splitlines())
                self.assertFalse(verdict(ranking + reference + indented_correction).passed)
                self.assertFalse(verdict(ranking + reference + f"\n\n{indent}Additional ranked files:\n"
                                        f"{indent}- notes.txt").passed)
                indented_reference = "\n".join(indent + line if line else "" for line in reference.splitlines())
                self.assertTrue(verdict(ranking + indented_reference).passed)
        self.assertFalse(verdict(ranking + "\n- notes.txt" + reference).passed)
        self.assertFalse(verdict(ranking.replace("2. video.bin\n\n3. cache/archive.bin",
                                               "2. cache/archive.bin\n\n3. video.bin") + reference).passed)
        self.assertFalse(verdict(ranking.replace("cache/archive.bin", "notes.txt") + reference).passed)
        bullets = "- data/dump.bin\n- video.bin\n- cache/archive.bin"
        self.assertTrue(verdict(bullets + reference).passed)
        self.assertFalse(verdict(bullets + "\n- notes.txt" + reference).passed)
        table = "| File | Size |\n|---|---|\n| data/dump.bin | 21M |\n| video.bin | 12M |\n| cache/archive.bin | 4.8M |"
        self.assertTrue(verdict(table + reference).passed)
        self.assertFalse(verdict(table + "\n| notes.txt | 100 bytes |").passed)

    def test_language_counts_and_wrong_total(self):
        facts = fixtures.create(self.base / "project", "project")
        text = "| Language | Files | Lines |\n| Python | 3 | 15 |\n| JavaScript | 1 | 4 |\n| Rust | 1 | 6 |\n| Shell | 1 | 4 |\nTotal: 29 lines in 6 files."
        self.assertEqual(checks.line_counts(text, facts), [])
        self.assertTrue(checks.line_counts(text.replace("29 lines", "36 lines"), facts))
        self.assertTrue(checks.line_counts(text.replace("6 files", "8 files"), facts))
        self.assertTrue(checks.line_counts(text.replace("| 15 |", "| 5 |"), facts))
        self.assertTrue(checks.line_counts("Python files: main.py", facts))
        sections = (
            "**Python (.py files):**\n- main.py: 5 lines\n- **Total: 15 lines**\n\n"
            "**Shell script (.sh files):**\n- **Total: 4 lines**\n\n"
            "**Rust:**\n- **Total: 6 lines**\n\n**JavaScript:**\n- **Total: 4 lines**\n\n"
            "**Summary:**\n" + text
        )
        self.assertEqual(checks.line_counts(sections, facts), [])
        self.assertTrue(checks.line_counts(sections.replace("Total: 15", "Total: 16"), facts))

    def test_language_counts_match_line_units_after_file_counts(self):
        facts = fixtures.create(self.base / "project", "project")
        for unit in ("lines", "LOC", "行"):
            for separator in ("\n", "; "):
                for files_first in (True, False):
                    with self.subTest(unit=unit, separator=separator, files_first=files_first):
                        rows = []
                        for language, count in facts["languages"].items():
                            files = f"{facts['language_files'][language]} files"
                            lines = f"{count} {unit}"
                            values = f"{files}, {lines}" if files_first else f"{lines}, {files}"
                            rows.append(f"{language}: {values}")
                        answer = separator.join(rows) + f"\nTotal: 6 files, 29 {unit}"
                        self.assertEqual(checks.line_counts(answer, facts), [])
                        self.assertTrue(checks.line_counts(answer.replace(f"15 {unit}", f"16 {unit}"), facts))
                        self.assertTrue(checks.line_counts(answer.replace(f"29 {unit}", f"30 {unit}"), facts))
        chinese = "\n".join(
            f"{language}：{facts['language_files'][language]}个文件，{count}行"
            for language, count in facts["languages"].items()
        )
        self.assertEqual(checks.line_counts(chinese, facts), [])

    def test_language_counts_do_not_treat_file_counts_as_unitless_loc(self):
        facts = fixtures.create(self.base / "project", "project")
        bare = "\n".join(f"{language}: {count}" for language, count in facts["languages"].items())
        self.assertEqual(checks.line_counts(bare, facts), [])
        files_only = "\n".join(f"{language}: {count} files" for language, count in facts["languages"].items())
        reasons = checks.line_counts(files_only, facts)
        self.assertTrue(all(f"no unambiguous {language} line count" in reasons for language in facts["languages"]))
        mixed = "Python: 3 files; JavaScript: 1 file, 4 lines\nRust: 6 lines\nShell: 4 lines"
        self.assertIn("no unambiguous python line count", checks.line_counts(mixed, facts))
        for invalid in ("-15", "1.15"):
            self.assertIn("no unambiguous python line count", checks.line_counts(
                f"Python: 3 files, {invalid} lines\nJavaScript: 4 lines\nRust: 6 lines\nShell: 4 lines", facts,
            ))

    def test_language_subtotals_require_disjoint_complete_file_scopes(self):
        facts = fixtures.create(self.base / "project", "project")
        sections = (
            "**Python (.py files):**\n- main.py: 5 lines\n- tools/report.py: 5 lines\n"
            "- **Total: 10 lines in 2 files**\n\n"
            "**Shell:**\n- scripts/check.sh: 4 lines\n- **Total: 4 lines**\n\n"
            "**Rust:**\n- src/main.rs: 6 lines\n- **Total: 6 lines**\n\n"
            "**JavaScript:**\n- web/app.js: 4 lines\n- **Total: 4 lines**\n\n"
            "**Python (in lib):**\n- lib/maths.py: 5 lines\n- **Total: 5 lines in 1 file**\n\n"
            "**Overall totals by language:**\n"
            "| Language | Lines |\n| Python | 15 |\n| Shell | 4 |\n| Rust | 6 |\n| JavaScript | 4 |\n"
            "**Grand total: 29 lines across 6 files**"
        )
        self.assertEqual(checks.line_counts(sections, facts), [])
        without_subtotals = "\n".join(line for line in sections.splitlines() if not line.startswith("- **Total:"))
        self.assertEqual(checks.line_counts(without_subtotals, facts), [])
        self.assertTrue(checks.line_counts(without_subtotals.replace("| Python | 15 |", "| Python | 10 |"), facts))
        self.assertTrue(checks.line_counts(without_subtotals.replace("29 lines", "34 lines"), facts))
        for wrong in (
            sections.replace("29 lines", "34 lines"),
            sections.replace("6 files", "5 files"),
            sections.replace("Total: 10 lines", "Total: 11 lines"),
            sections.replace("Total: 5 lines", "Total: 4 lines"),
            sections.replace("2 files", "3 files"),
            sections.replace("lib/maths.py", "main.py"),
            sections.replace("- tools/report.py: 5 lines\n", ""),
            sections.replace("- lib/maths.py: 5 lines\n", ""),
            sections.replace("Total: 5 lines in 1 file", "Total: 5 lines and 5 lines in 1 file"),
            sections.replace("**Python (in lib):**", "**Python (in lib):**\n- main.py: 5 lines"),
            sections.replace("| Python | 15 |", "| Python | 10 |"),
            sections.replace("- **Total: 10 lines in 2 files**\n", "")
                    .replace("Total: 5 lines in 1 file", "Total: 15 lines"),
            sections.replace("- **Total: 5 lines in 1 file**\n", "")
                    .replace("Total: 10 lines in 2 files", "Total: 15 lines"),
        ):
            with self.subTest(answer=wrong):
                self.assertTrue(checks.line_counts(wrong, facts))

    def test_python_files_language_and_extras(self):
        root = self.base / "project"
        facts = fixtures.create(root, "project")
        def verdict(text):
            return checks.judge({"check": "python"}, text, facts, root, facts["before"], self.result, self.metrics)
        good = "Python 文件有 main.py、lib/maths.py 和 tools/report.py。"
        self.assertTrue(verdict(good).passed)
        self.assertFalse(verdict(good + " missing.py").passed)
        self.assertFalse(verdict("main.py, lib/maths.py, tools/report.py").passed)
        self.assertTrue(verdict(good + "\n另外还有一些非 Python 文件：\n- scripts/check.sh\n- src/main.rs\n- web/app.js").passed)
        self.assertFalse(verdict(good + " web/app.js").passed)
        self.assertFalse(verdict(good + "\n共 **4个** Python 文件。").passed)

    def test_python_files_reject_inverted_classification(self):
        root = self.base / "project"
        facts = fixtures.create(root, "project")
        files = "\n".join(f"- {name}" for name in facts["python"])
        for heading in ("这些都不是 Python 文件：", "Non-Python files:", "These are not Python files:"):
            with self.subTest(heading=heading):
                answer = f"文件清单：\n{heading}\n{files}"
                verdict = checks.judge(
                    {"check": "python"}, answer, facts, root, facts["before"], self.result, self.metrics,
                )
                self.assertFalse(verdict.passed)
                self.assertTrue(any(reason.startswith("Python files classified as non-Python:")
                                    for reason in verdict.reasons))

    def test_python_files_separate_directory_overview_from_classification(self):
        root = self.base / "project"
        facts = fixtures.create(root, "project")
        overview = "\n".join(f"- {name}" for name in facts["before"])
        python = "\n".join(f"- {name}" for name in facts["python"])
        def verdict(answer):
            return checks.judge({"check": "python"}, answer, facts, root, facts["before"], self.result, self.metrics)
        for heading in ("**目录：**", "## Directory structure"):
            with self.subTest(heading=heading):
                text = f"{heading}\n{overview}\n\n**Python 文件（共 3 个）：**\n{python}"
                self.assertTrue(verdict(text).passed)
                self.assertFalse(verdict(text + "\n- scripts/check.sh").passed)
                self.assertFalse(verdict(f"{heading}\n{overview}").passed)
                self.assertFalse(verdict(text.rsplit("\n", 1)[0]).passed)
                self.assertFalse(verdict(text.replace("**Python 文件（共 3 个）：**",
                                                     "**非 Python 文件：**")).passed)
        self.assertFalse(verdict(f"包含以下 Python 文件：\n{python}\n- scripts/check.sh（Shell 脚本）").passed)

    def test_failure_explanation_requires_a_repair_action(self):
        root = self.base / "failure"
        facts = fixtures.create(root, "failure")
        self.result.turns = [{"output": "FileNotFoundError: config.json"}]
        def verdict(answer):
            return checks.judge(
                {"check": "failure"}, answer, facts, root, facts["before"], self.result, self.metrics,
            )
        for answer in (
            "config.json is missing from the current directory/relative path.",
            "config.json is missing at this address.",
            "config.json 的相对路径不存在。",
        ):
            with self.subTest(answer=answer):
                self.assertIn("answer provides no recognized remedy", verdict(answer).reasons)
        for answer in (
            "config.json is missing. Create the file with valid JSON content.",
            "config.json is missing. Correct the relative path in broken.py.",
            "config.json is missing. Change to the directory containing it.",
            "config.json 不存在，请创建该文件并写入有效的 JSON。",
            "config.json 不存在，请修正脚本中的相对路径。",
            "config.json 不存在，请切换到包含该文件的目录。",
            "config.json 不存在，请在正确目录运行脚本。",
        ):
            with self.subTest(answer=answer):
                self.assertTrue(verdict(answer).passed)

    def test_python_and_failure_baseline_verdicts_are_preserved(self):
        baseline = json.loads(
            (run.HERE / "baselines" / "main-7c57a88" / "report.json").read_text(encoding="utf-8")
        )
        scenarios = {s["id"]: s for s in baseline["metadata"]["scenarios"]}
        trials = [t for t in baseline["trials"] if t["scenario_id"] in ("chinese-python", "explain-failure")]
        self.assertEqual(len(trials), 10)
        for trial in trials:
            with self.subTest(scenario=trial["scenario_id"], seed=trial["seed"]):
                scenario = scenarios[trial["scenario_id"]]
                root = self.base / f"{trial['scenario_id']}-{trial['seed']}"
                facts = fixtures.create(root, scenario["fixture"])
                historical_facts = {key: value for key, value in facts.items() if key != "file_lines"}
                self.assertEqual(historical_facts, trial["facts"])
                result = driver.Result(exit_code=trial["exit_code"], turns=trial["turns"])
                verdict = checks.judge(
                    scenario, trial["answer"], facts, root,
                    trial["final_state"]["files"], result, trial["metrics"],
                )
                self.assertEqual(verdict.passed, trial["status"] == "pass")

    def test_port_identity_allows_a_spelled_out_python_version_but_requires_pid(self):
        root = self.base / "port"
        facts = fixtures.create(root, "port")
        facts["listener"] = {"pid": 12345, "port": 8080, "process": "python3"}
        def verdict(answer):
            return checks.judge({"check": "port"}, answer, facts, root, facts["before"], self.result, self.metrics)
        self.assertTrue(verdict("A Python 3 process (PID 12345).").passed)
        self.assertTrue(verdict("python3.14, PID 12345").passed)
        self.assertFalse(verdict("python3, PID 23456").passed)
        self.assertFalse(verdict("python3 -m http.server 8080").passed)

    def test_rename_contents_and_approval(self):
        root = self.base / "rename"
        facts = fixtures.create(root, "rename")
        scenario = {"check": "rename"}
        for source, destination in facts["renames"].items():
            (root / source).rename(root / destination)
        after = fixtures.snapshot(root)
        self.assertFalse(checks.judge(scenario, "done", facts, root, after, self.result, self.metrics).passed)
        self.result.approvals = [{"allowed": True}]
        self.assertTrue(checks.judge(scenario, "done", facts, root, after, self.result, self.metrics).passed)
        (root / "readme.md").write_text("changed")
        self.assertFalse(checks.judge(scenario, "done", facts, root, fixtures.snapshot(root), self.result, self.metrics).passed)
        loop = 'for f in *.txt; do mv "$f" "${f%.txt}.md"; done'
        self.assertTrue(approval.allow_approval("rename", loop, root, facts))
        self.assertTrue(approval.allow_approval("rename", f"cd {root} && {loop}", root, facts))
        self.assertTrue(approval.allow_approval("rename", f"cd {root} && mv alpha.txt alpha.md && ls -la", root, facts))
        self.assertTrue(approval.allow_approval("rename", loop.replace("*.txt", "./*.txt").replace("%.txt", "%.*"), root, facts))
        self.assertFalse(approval.allow_approval("rename", f"cd /tmp && {loop}", root, facts))
        self.assertFalse(approval.allow_approval("rename", loop + "; touch /tmp/other", root, facts))
        self.assertFalse(approval.allow_approval("rename", "mv alpha.txt /tmp/a.md", root, facts))

    def test_cwd_requires_physical_probe(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        self.result.pwd = str(root)
        args = ({"check": "cwd"}, "Changed to data: dump.bin and small.bin.", facts, root, facts["before"], self.result, self.metrics)
        self.assertFalse(checks.judge(*args).passed)
        self.result.pwd = str(root / "data")
        self.assertTrue(checks.judge(*args).passed)
        self.assertTrue(approval.allow_approval("cwd", "cd data && ls -la", root, facts))
        self.assertFalse(approval.allow_approval("cwd", "cd / && ls -la", root, facts))

    def test_archive_verifies_contents_without_running_arbitrary_shell(self):
        root = self.base / "logs"
        facts = fixtures.create(root, "logs")
        before = facts["before"]
        for command in ("tar -czf logs.tar.gz logs", "cd logs && tar -czvf ../logs.tar.gz .",
                        "tar -czf logs.tar.gz -C logs ."):
            with self.subTest(command=command):
                self.assertEqual(checks.check_archive(command, root, before, before), [])
                self.assertFalse((root / "logs.tar.gz").exists())
        for command in ("echo logs.tar.gz", "tar -czf /tmp/stolen.tar.gz logs",
                        "tar -czf logs.tar.gz .", "tar -czf logs.tar.gz logs; touch owned",
                        "tar --checkpoint-action=exec=touch -czf logs.tar.gz logs",
                        "tar -czf logs.tar.gz $(touch owned)"):
            with self.subTest(command=command):
                self.assertTrue(checks.check_archive(command, root, before, before))
                self.assertFalse((root / "owned").exists())

    def test_git_summary_associates_features_with_their_components_across_lines(self):
        root = self.base / "history"
        facts = fixtures.create(root, "history")
        answer = "\n\n".join(f"{i}. **Commit**\n   - Feature: {subject}\n   - Component: `{component}`"
                             for i, (component, _, subject) in enumerate(fixtures.HISTORY, 1))
        verdict = checks.judge({"check": "history"}, answer, facts, root, facts["before"],
                               self.result, self.metrics)
        self.assertTrue(verdict.passed)
        self.assertNotIn("components", verdict.details["facts"],
                         "history component names are not capture evaluation state")
        swapped = answer.replace("Component: `llm`", "Component: `hub`", 1)
        self.assertFalse(checks.judge({"check": "history"}, swapped, facts, root, facts["before"],
                                     self.result, self.metrics).passed)
