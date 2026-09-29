from __future__ import annotations
from pathlib import Path
import sys
import tempfile
import unittest
from eval import approval, checks, driver, fixtures
from eval.checks import command_assist as assist_checks, files as file_checks


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
        self.assertEqual(file_checks.line_counts(text, facts), [])
        self.assertTrue(file_checks.line_counts(text.replace("29 lines", "36 lines"), facts))
        self.assertTrue(file_checks.line_counts(text.replace("6 files", "8 files"), facts))
        self.assertTrue(file_checks.line_counts(text.replace("| 15 |", "| 5 |"), facts))
        self.assertTrue(file_checks.line_counts("Python files: main.py", facts))
        sections = (
            "**Python (.py files):**\n- main.py: 5 lines\n- **Total: 15 lines**\n\n"
            "**Shell script (.sh files):**\n- **Total: 4 lines**\n\n"
            "**Rust:**\n- **Total: 6 lines**\n\n**JavaScript:**\n- **Total: 4 lines**\n\n"
            "**Summary:**\n" + text
        )
        self.assertEqual(file_checks.line_counts(sections, facts), [])
        self.assertTrue(file_checks.line_counts(sections.replace("Total: 15", "Total: 16"), facts))

    def test_largest_checks_optional_sizes_without_requiring_exact_display_precision(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        def grade(answer):
            return checks.judge({"check": "largest"}, answer, facts, root, facts["before"], self.result, self.metrics)
        for sizes in (("21,000,000 bytes", "12,000,000 B", "5000000 字节"),
                      ("21 MB", "12 MB", "5 MB"), ("20.1 MiB", "11.5 MiB", "4.8 MiB")):
            answer = "\n".join(f"{i}. {name}: {size}" for i, (name, size) in
                              enumerate(zip(facts["largest"], sizes), 1))
            self.assertTrue(grade(answer).passed, grade(answer).reasons)
        wrong = "1. data/dump.bin: 1 byte\n2. video.bin: 2 bytes\n3. cache/archive.bin: 3 bytes"
        self.assertFalse(grade(wrong).passed)
        self.assertFalse(grade(wrong.replace("\n", "; ")).passed)
        self.assertFalse(grade("data/dump.bin: -21000000 bytes; video.bin: 12MB; cache/archive.bin: 5MB").passed)
        self.assertTrue(grade("21M data/dump.bin; 12M video.bin; 4.8M cache/archive.bin").passed)

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
                        self.assertEqual(file_checks.line_counts(answer, facts), [])
                        self.assertTrue(file_checks.line_counts(answer.replace(f"15 {unit}", f"16 {unit}"), facts))
                        self.assertTrue(file_checks.line_counts(answer.replace(f"29 {unit}", f"30 {unit}"), facts))
        chinese = "\n".join(
            f"{language}：{facts['language_files'][language]}个文件，{count}行"
            for language, count in facts["languages"].items()
        )
        self.assertEqual(file_checks.line_counts(chinese, facts), [])

    def test_language_counts_do_not_treat_file_counts_as_unitless_loc(self):
        facts = fixtures.create(self.base / "project", "project")
        bare = "\n".join(f"{language}: {count}" for language, count in facts["languages"].items())
        self.assertEqual(file_checks.line_counts(bare, facts), [])
        files_only = "\n".join(f"{language}: {count} files" for language, count in facts["languages"].items())
        reasons = file_checks.line_counts(files_only, facts)
        self.assertTrue(all(f"no unambiguous {language} line count" in reasons for language in facts["languages"]))
        mixed = "Python: 3 files; JavaScript: 1 file, 4 lines\nRust: 6 lines\nShell: 4 lines"
        self.assertIn("no unambiguous python line count", file_checks.line_counts(mixed, facts))
        for invalid in ("-15", "1.15"):
            self.assertIn("no unambiguous python line count", file_checks.line_counts(
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
        self.assertEqual(file_checks.line_counts(sections, facts), [])
        without_subtotals = "\n".join(line for line in sections.splitlines() if not line.startswith("- **Total:"))
        self.assertEqual(file_checks.line_counts(without_subtotals, facts), [])
        self.assertTrue(file_checks.line_counts(without_subtotals.replace("| Python | 15 |", "| Python | 10 |"), facts))
        self.assertTrue(file_checks.line_counts(without_subtotals.replace("29 lines", "34 lines"), facts))
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
                self.assertTrue(file_checks.line_counts(wrong, facts))

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
        for answer in (
            "8080 并不是 python3（PID 12345）占用的，而是 nginx（PID 23456）。",
            "Port 8080 is not listening in python3 (PID 12345).",
            "python3 (PID 12345) is not listening on port 8080.",
            "占用 8080 的不是 python3（PID 12345）。",
        ):
            with self.subTest(answer=answer):
                self.assertFalse(verdict(answer).passed)

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
        for command in (
            "mv -n -- alpha.txt alpha.md", "mv ./alpha.txt ./alpha.md",
            loop.replace('mv "$f"', 'mv --no-clobber -- "$f"'),
        ):
            self.assertTrue(approval.allow_approval("rename", command, root, facts), command)
        for command in (
            "mv -n -- alpha.txt ../alpha.md", "mv ./alpha.txt ./readme.md",
            "mv --backup alpha.txt alpha.md", "mv -n alpha.txt alpha.md && touch extra",
        ):
            self.assertFalse(approval.allow_approval("rename", command, root, facts), command)
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
        invented = list(args)
        invented[1] += " There is also invented.txt."
        self.assertFalse(checks.judge(*invented).passed)
        self.assertTrue(approval.allow_approval("cwd", "cd data && ls -la", root, facts))
        self.assertFalse(approval.allow_approval("cwd", "cd / && ls -la", root, facts))

    def test_cwd_listing_respects_parent_scope_and_negative_references(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        self.result.pwd = str(root / "data")
        def grade(answer):
            return checks.judge({"check": "cwd"}, answer, facts, root, facts["before"], self.result, self.metrics)
        listing = "已进入 data，包含 dump.bin 和 small.bin。"
        for answer in (
            listing + "父目录的 notes.txt 不在当前目录内。",
            listing + "notes.txt 位于父目录。",
            "当前目录 data:\n- dump.bin\n- small.bin\n\n父目录:\n- notes.txt",
            "Changed to data. Files: dump.bin, small.bin. Parent directory: notes.txt.",
            listing + "当前目录没有 invented.txt。",
            listing + "父目录:\n- notes.txt\ndump.bin 比 small.bin 大。",
            listing + "父目录:\n- notes.txt\n\ndump.bin 比 small.bin 大。",
            "Current directory contains dump.bin and small.bin without extra files.",
            "data: dump.bin, small.bin. data does not include notes.txt.",
            "data: dump.bin, small.bin. notes.txt is outside data.",
            "已进入 data，包含`dump.bin`和`small.bin`。",
        ):
            self.assertTrue(grade(answer).passed, grade(answer).reasons)
        for answer in (
            listing + "还包含 invented.txt。",
            listing + "父目录的 invented.txt 不在当前目录内。",
            listing + "当前目录没有 dump.bin。",
            "父目录:\n- dump.bin\n- small.bin",
            "当前目录 data:\n- dump.bin\n- small.bin\n- notes.txt",
            listing + "There is no notes.txt in the parent directory.",
            listing + "dump.bin is outside data.",
            listing + "当前目录不包含`dump.bin`。",
        ):
            self.assertFalse(grade(answer).passed, answer)

    def test_archive_verifies_contents_without_running_arbitrary_shell(self):
        root = self.base / "logs"
        facts = fixtures.create(root, "logs")
        before = facts["before"]
        for command in ("tar -czf logs.tar.gz logs", "cd logs && tar -czvf ../logs.tar.gz .",
                        "tar -czf logs.tar.gz -C logs .", "tar -czf logs.tar.gz logs;"):
            with self.subTest(command=command):
                self.assertEqual(assist_checks.check_archive(command, root, before, before), [])
                self.assertFalse((root / "logs.tar.gz").exists())
        for command in ("echo logs.tar.gz", "tar -czf /tmp/stolen.tar.gz logs",
                        "tar -czf logs.tar.gz .", "tar -czf logs.tar.gz logs; touch owned",
                        "tar --checkpoint-action=exec=touch -czf logs.tar.gz logs",
                        "tar -czf logs.tar.gz $(touch owned)"):
            with self.subTest(command=command):
                self.assertTrue(assist_checks.check_archive(command, root, before, before))
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

    def test_summary_accepts_prose_but_rejects_invented_or_reversed_commits(self):
        root = self.base / "history"
        facts = fixtures.create(root, "history")
        def grade(answer):
            return checks.judge({"check": "history"}, answer, facts, root, facts["before"], self.result, self.metrics)
        prose = " ".join(f"{component}: {subject}." for component, _, subject in fixtures.HISTORY)
        self.assertTrue(grade(prose).passed, grade(prose).reasons)
        listed = "\n".join(f"- {component}: {subject}" for component, _, subject in fixtures.HISTORY)
        self.assertTrue(grade(listed).passed)
        for answer in (listed + "\n- auth: add user authentication",
                       listed + "\n9. **auth** — add user authentication.",
                       listed + "\n9. Added user authentication in the auth module.",
                       listed + "\n9. shell: support shell pipelines",
                       listed.replace("document offline usage", "delete offline documentation"),
                       listed.replace("document offline usage", "删除离线使用说明")):
            self.assertFalse(grade(answer).passed, answer)
        table = "| Component | Feature |\n|---|---|\n" + "\n".join(
            f"| {component} | {subject} |" for component, _, subject in fixtures.HISTORY)
        self.assertTrue(grade(table).passed, grade(table).reasons)
        self.assertFalse(grade(table + "\n| auth | add authentication |").passed)
        self.assertTrue(grade(listed + "\n\nNext steps:\n- auth: add authentication in the future").passed)
        for heading in ("Summary:", "Notes:", "总结：", "备注："):
            with self.subTest(heading=heading):
                self.assertTrue(grade(heading + "\n" + listed).passed)
                self.assertFalse(grade(heading + "\n" + listed + "\n- auth: add authentication").passed)
                self.assertFalse(grade(heading + "\n- auth: add authentication\n" + listed).passed)
        for note in (
            "- auth: add user authentication",
            "- auth — add user authentication",
            "| Component | Feature |\n|---|---|\n| auth | add user authentication |",
        ):
            self.assertFalse(grade("Notes:\n" + note + "\n\nSummary:\n" + listed).passed, note)
            self.assertFalse(grade(listed + "\n\nNotes:\n" + note).passed, note)
        self.assertFalse(grade("Next steps:\n" + listed).passed)
        self.assertTrue(grade(listed + "\n\nNotes:\n- These are local changes.").passed)
        self.assertTrue(grade(listed + "\n\nNotes:\n- Count: 8\n- Branch: fixture").passed)

    def test_missing_config_requires_valid_json_not_an_empty_file(self):
        root = self.base / "failure"
        facts = fixtures.create(root, "failure")
        self.result.turns = [{"exit_code": 1, "output": "FileNotFoundError: config.json"}]
        def grade(answer):
            return checks.judge({"check": "failure"}, answer, facts, root, facts["before"], self.result, self.metrics)
        for answer in (
            "config.json is missing. Create an empty file using touch config.json and rerun.",
            "config.json is missing. Do not create or provide it.",
            "config.json 不存在。创建一个空文件即可。",
        ):
            self.assertFalse(grade(answer).passed, answer)
        self.assertTrue(grade("config.json 不存在，请创建该文件并写入有效 JSON，例如 {}。").passed)
