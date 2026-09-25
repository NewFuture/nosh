from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import socket
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from eval import checks, driver, fixtures, report, run


class ContractTests(unittest.TestCase):
    def test_source_fingerprint_ignores_platform_line_endings(self):
        with tempfile.TemporaryDirectory() as temporary:
            a, b = Path(temporary) / "a.py", Path(temporary) / "b.py"
            a.write_bytes(b"print(1)\r\n")
            b.write_bytes(b"print(1)\n")
            self.assertEqual(fixtures.source_hash(a), fixtures.source_hash(b))
            self.assertNotEqual(fixtures.file_hash(a), fixtures.file_hash(b))

    def test_default_suite_and_seeds(self):
        suite = run.load_suite(run.HERE / "scenarios.json")
        self.assertEqual(len(suite["scenarios"]), 10)
        self.assertEqual(suite["seeds"], [0, 1, 2, 3, 4])
        self.assertEqual(run.seeds([0, 2**64 - 1]), [0, 2**64 - 1])
        for bad in ([], [True], [-1], [2**64], [0, 0], ["0"], None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                run.seeds(bad)

    def test_invalid_suite_rejected(self):
        original = json.loads((run.HERE / "scenarios.json").read_text())
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "suite.json"
            for field, value in (("id", "../outside"), ("mode", "unknown"), ("approval", "yolo"),
                                 ("fixture", "host"), ("fixture", "project"), ("inputs", ["# x\nrm x"]),
                                 ("unknown_option", True)):
                suite = copy.deepcopy(original)
                suite["scenarios"][0][field] = value
                path.write_text(json.dumps(suite))
                with self.subTest(field=field), self.assertRaises(ValueError):
                    run.load_suite(path)

    def test_native_suggestion_observes_actual_usage_not_stdout_latency(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 1.5}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 4}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "input"}]},
                {"ev": "step_end", "sid": 1, "text": "", "tool_calls": [],
                 "usage": {"ttft_s": 0.125}},
            ]
            trace.write_text("\n".join(json.dumps(dict(e, schema_version=1)) for e in events))
            result = driver.Result(stdout="tar -czf logs.tar.gz logs\n", exit_code=0, total_s=9)
            scenario = {"mode": "suggest", "check": "archive"}
            observed = run.observe(result, scenario, trace, False, 4)
            self.assertEqual(observed["metrics"]["ttft_s"], 0.125)
            self.assertEqual(observed["metrics"]["total_s"], 9)
            self.assertEqual(observed["metrics"]["load_s"], 1.5)
            self.assertEqual(observed["answer"], result.stdout.strip())
            legacy = run.observe(result, scenario, trace, True, 4)
            self.assertIsNone(legacy["metrics"]["ttft_s"])
            self.assertIsNone(legacy["inputs"])
            with self.assertRaises(ValueError):
                run.observe(result, scenario, trace, False, 5)
            trace.write_text(json.dumps({"schema_version": 1, "ev": "step_error", "error": "context full"}) + "\n")
            with self.assertRaisesRegex(RuntimeError, "context full"):
                run.observe(result, scenario, trace, False, 4)
            trace.unlink()
            with self.assertRaises(ValueError):
                run.observe(result, scenario, trace, False, 4)

    def test_isolated_config_uses_the_existing_string_contract(self):
        import tomllib

        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            env = run.environment(home, 4, None)
            cfg = tomllib.loads((Path(env["NOSH_HOME"]) / "config.toml").read_text())
            self.assertEqual(cfg["model"]["thinking"], "off")
            result = driver.Result(stdout="tar -czf logs.tar.gz logs", exit_code=0,
                                   stderr="nosh: /tmp/config.toml: model.thinking: expected a string\n")
            with self.assertRaises(ValueError):
                run.observe(result, {"mode": "suggest", "check": "archive"}, home / "trace", True, 0)

    def test_legacy_answer_excludes_echo_tools_and_intermediate_answers(self):
        text = (
            "__NOSH_EVAL_PROMPT__ # expected.py\n"
            "┃ Let me inspect.\n┃ ⚙ list_dir SAFE\n┃   expected.py\n┃   exit 0\n"
            "┃ Actual final answer.\n┃ ✔ 2 steps · 1.0 s\n┃ stats: ttft 0.12s\n"
        )
        self.assertEqual(run.legacy_answer(text), "Actual final answer.")
        denied = ("┃ I will change it.\n┃ ╭─ run_command · MUTATING\n┃ │ $ mv a b\n"
                  "┃ ╰─ [y] run [n] deny › n\n┃ reason (optional, Enter to skip):\n"
                  "┃ I did not change it.\n┃ ⚠ 2 steps · 1.0 s\n")
        self.assertEqual(run.legacy_answer(denied), "I did not change it.")


@unittest.skipUnless(sys.platform == "linux", "Linux fixtures and process interfaces")
class FixtureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.base = Path(self.temp.name)

    def tearDown(self):
        self.temp.cleanup()

    def test_fixture_rebuild_and_exact_facts(self):
        with fixtures.Workspace(self.base / "work") as workspace:
            scenario = {"id": "project", "fixture": "project"}
            root, _, first = workspace.prepare(scenario)
            (root / "main.py").write_text("changed")
            root, _, second = workspace.prepare(scenario)
            self.assertEqual(first, second)
            self.assertEqual(second["languages"], {"python": 15, "javascript": 4, "rust": 6, "shell": 4})
            self.assertEqual(second["total"], 29)
            self.assertEqual(len(second["python"]), 3)
            self.assertTrue(all(int(p.stat().st_mtime) == fixtures.EPOCH for p in root.rglob("*")))

    def test_fixed_git_history(self):
        a, b = self.base / "a", self.base / "b"
        first = fixtures.create(a, "history")
        second = fixtures.create(b, "history")
        self.assertEqual(first, second)
        self.assertEqual(fixtures.git(a, "rev-list", "--count", "HEAD").strip(), "8")

    def test_large_files_are_materialized_not_sparse(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        allocated = {name: (root / name).stat().st_blocks * 512 for name in facts["before"]}
        self.assertTrue(all(size > 0 for size in allocated.values()))
        self.assertEqual(sorted(allocated, key=allocated.get, reverse=True)[:3], facts["largest"])
        self.assertEqual((root / "data" / "dump.bin").stat().st_size, 21_000_000)

    def test_ownership_lock_and_symlink_boundaries(self):
        outsider = self.base / "outside"
        outsider.mkdir()
        (outsider / "keep").write_text("keep")
        with self.assertRaises(ValueError):
            with fixtures.Workspace(outsider):
                pass
        public = self.base / "public"
        public.mkdir(mode=0o755)
        with self.assertRaisesRegex(ValueError, "private"):
            with fixtures.Workspace(public):
                pass
        with fixtures.Workspace(self.base / "work") as workspace:
            with self.assertRaises(RuntimeError):
                with fixtures.Workspace(workspace.root):
                    pass
            with self.assertRaises(ValueError):
                workspace.clean("../outside")
            (workspace.root / "linked").symlink_to(outsider, target_is_directory=True)
            workspace.clean("linked")
            self.assertEqual((outsider / "keep").read_text(), "keep")
        with self.assertRaises(ValueError):
            workspace.clean("linked")
        (self.base / "link").symlink_to(outsider, target_is_directory=True)
        with self.assertRaises(ValueError):
            with fixtures.Workspace(self.base / "link" / "child"):
                pass

    def test_listener_owns_pid_and_never_replaces_occupied_port(self):
        with socket.socket() as occupied:
            occupied.bind(("127.0.0.1", 0))
            occupied.listen()
            port = occupied.getsockname()[1]
            with self.assertRaises(RuntimeError):
                with fixtures.listener(self.base, port):
                    pass
        with fixtures.listener(self.base, port) as info:
            self.assertEqual(info["port"], port)
            self.assertTrue(Path(f"/proc/{info['pid']}").exists())
        self.assertFalse(Path(f"/proc/{info['pid']}").exists())


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
                self.assertEqual(facts, trial["facts"])
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
        self.assertTrue(checks.allow_approval("rename", loop, root, facts))
        self.assertTrue(checks.allow_approval("rename", f"cd {root} && {loop}", root, facts))
        self.assertTrue(checks.allow_approval("rename", f"cd {root} && mv alpha.txt alpha.md && ls -la", root, facts))
        self.assertTrue(checks.allow_approval("rename", loop.replace("*.txt", "./*.txt").replace("%.txt", "%.*"), root, facts))
        self.assertFalse(checks.allow_approval("rename", f"cd /tmp && {loop}", root, facts))
        self.assertFalse(checks.allow_approval("rename", loop + "; touch /tmp/other", root, facts))
        self.assertFalse(checks.allow_approval("rename", "mv alpha.txt /tmp/a.md", root, facts))

    def test_cwd_requires_physical_probe(self):
        root = self.base / "big"
        facts = fixtures.create(root, "big")
        self.result.pwd = str(root)
        args = ({"check": "cwd"}, "Changed to data: dump.bin and small.bin.", facts, root, facts["before"], self.result, self.metrics)
        self.assertFalse(checks.judge(*args).passed)
        self.result.pwd = str(root / "data")
        self.assertTrue(checks.judge(*args).passed)
        self.assertTrue(checks.allow_approval("cwd", "cd data && ls -la", root, facts))
        self.assertFalse(checks.allow_approval("cwd", "cd / && ls -la", root, facts))

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
        self.assertTrue(checks.judge({"check": "history"}, answer, facts, root, facts["before"],
                                    self.result, self.metrics).passed)
        swapped = answer.replace("Component: `llm`", "Component: `hub`", 1)
        self.assertFalse(checks.judge({"check": "history"}, swapped, facts, root, facts["before"],
                                     self.result, self.metrics).passed)


@unittest.skipUnless(sys.platform == "linux", "Linux PTY and wait4")
class DriverTests(unittest.TestCase):
    def test_partial_pty_setup_failure_reaps_the_started_child(self):
        import pty

        started = []
        fork = pty.fork
        def track_fork():
            pid, fd = fork()
            if pid:
                started.append(pid)
            return pid, fd
        with patch("pty.fork", side_effect=track_fork), patch("fcntl.ioctl", side_effect=PermissionError("blocked ioctl")):
            with self.assertRaisesRegex(PermissionError, "blocked ioctl"):
                driver.Child([sys.executable, "-c", "import time; time.sleep(60)"],
                             Path.cwd(), {"PATH": "/usr/bin:/bin"}, True)
        self.assertEqual(len(started), 1)
        with self.assertRaises(ChildProcessError):
            os.waitpid(started[0], os.WNOHANG)

    def test_screen_fragmented_queries_and_saved_cursor(self):
        screen = driver.Screen()
        self.assertEqual(screen.feed("abc\x1b["), b"")
        self.assertEqual(screen.feed("6n"), b"\x1b[1;4R")
        screen.feed("\x1b7\x1b[1;100Hconfirm\x1b8")
        self.assertEqual(screen.feed("\x1b[6n"), b"\x1b[1;4R")
        screen.feed("\r\x1b[K" + driver.PROMPT + "git status")
        self.assertEqual(screen.line(), driver.PROMPT + "git status")

    def test_cli_drains_both_pipes_and_has_no_controlling_tty(self):
        script = (
            "import os,sys; data=sys.stdin.buffer.read(); print(len(data)); "
            "sys.stderr.write('stderr\\n'); "
            "print(os.isatty(0), os.isatty(1)); "
            "\ntry: os.open('/dev/tty', os.O_RDWR)\nexcept OSError: print('no-tty')"
        )
        result = driver.run_cli([sys.executable, "-c", script], Path.cwd(), {"PATH": "/usr/bin:/bin"}, 5, b"a" * 100_000)
        self.assertIsNone(result.error)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(result.stdout, "100000\nFalse False\nno-tty\n")
        self.assertEqual(result.stderr, "stderr\n")
        self.assertGreater(result.peak_rss_mib, 0)

    def test_timeout_escalates_and_keeps_partial_output(self):
        code = "import signal,time; signal.signal(signal.SIGTERM,signal.SIG_IGN); print('ready',flush=True); time.sleep(60)"
        result = driver.run_cli([sys.executable, "-c", code], Path.cwd(), {"PATH": "/usr/bin:/bin"}, .3)
        self.assertIn("did not finish", result.error)
        self.assertEqual(result.stdout, "ready\n")
        self.assertEqual(result.exit_code, -9)

    def test_output_limit_is_an_explicit_failure(self):
        with patch.object(driver, "OUTPUT_LIMIT", 4096):
            result = driver.run_cli([sys.executable, "-c", "print('x' * 10000)"],
                                    Path.cwd(), {"PATH": "/usr/bin:/bin"}, 5)
        self.assertIn("output exceeded", result.error)

    def test_cleanup_finds_a_detached_owned_descendant(self):
        code = (
            "import subprocess,sys; p=subprocess.Popen([sys.executable,'-c','import time; time.sleep(60)'],"
            "stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL,start_new_session=True);"
            "print(p.pid,flush=True)"
        )
        result = driver.run_cli([sys.executable, "-c", code], Path.cwd(), {"PATH": "/usr/bin:/bin"}, 5)
        self.assertIsNone(result.error)
        status = Path(f"/proc/{int(result.stdout.strip())}/status")
        for _ in range(100):
            try:
                state = status.read_text()
            except FileNotFoundError:
                return
            if "State:\tZ" in state:
                return
            time.sleep(.01)
        self.fail("owned detached child survived cleanup")

    def test_pty_waits_for_split_approval_and_task_completion(self):
        script = r'''
import os, tty, time
tty.setraw(0)
def out(text):
    os.write(1, text.encode())
def line():
    data = b""
    while not data.endswith(b"\r"):
        data += os.read(0, 1)
    return data
out("__NOSH_EVAL_PROMPT__ ")
line()
out("\r\n┃ inspecting\r\n┃ ╭─ run_command · MUTATING\r\n┃ │ $ touch fixture\r\n┃ ╰─ [y] run")
time.sleep(.01)
out("  [n] deny  [e] edit › ")
assert os.read(0, 1) == b"y"
out("y\r\n┃ answer\r\n┃ ✔ 1 steps · 0.1 s\r\n┃ stats: ttft 0.01s\r\n__NOSH_EVAL_PROMPT__ ")
assert b"exit 0" in line()
'''
        scenario = {"inputs": ["# task"], "check": "largest"}
        result = driver.run_repl([sys.executable, "-c", script], Path.cwd(),
                                 {"PATH": "/usr/bin:/bin"}, 5, scenario,
                                 lambda command, card: command == "touch fixture")
        self.assertIsNone(result.error, result.transcript)
        self.assertEqual(result.exit_code, 0)
        self.assertEqual(len(result.approvals), 1)
        self.assertEqual(result.approvals[0]["answer"], "y")


class ReportTests(unittest.TestCase):
    def sample(self):
        return {
            "schema_version": 1,
            "metadata": {"run_id": "test", "observation": "native-v1",
                         "build": {"binary_sha256": "a" * 64},
                         "scenarios": [{"id": "example"}], "seeds": [0, 1], "repeat": 1},
            "trials": [{"scenario_id": "example", "seed": 0, "repeat": 0, "status": "pass",
                        "metrics": {"steps": 1, "confirmations": 0, "ttft_s": None, "total_s": 2, "peak_rss_mib": 10},
                        "answer": "answer\n```", "inputs": None, "final_state": {}}],
        }

    def test_denominator_keeps_missing_and_error_trials(self):
        data = self.sample()
        row = report.aggregate(data)[0]
        self.assertEqual((row["pass"], row["planned"], row["missing"]), (1, 2, 1))
        data["trials"].append(dict(data["trials"][0], seed=1, status="error"))
        row = report.aggregate(data)[0]
        self.assertEqual((row["pass"], row["planned"], row["error"]), (1, 2, 1))
        self.assertIn("1/2 (50%)", report.markdown(data))

    def test_paired_comparison_and_incompatibility(self):
        before = self.sample()
        after = copy.deepcopy(before)
        after["trials"][0]["status"] = "fail"
        after["trials"][0]["metrics"]["total_s"] = 3
        after["metadata"]["model"] = {"weights_sha256": "different"}
        comparison = report.compare(after, before)
        self.assertEqual(comparison["paired_trials"], 1)
        self.assertEqual(len(comparison["regressions"]), 1)
        self.assertEqual(comparison["pairs"][0]["metric_delta"]["total_s"], 1)
        self.assertIsNone(comparison["pairs"][0]["metric_delta"]["ttft_s"])
        self.assertTrue(comparison["warnings"])
        before["metadata"].update(harness_sha256="raw-crlf", harness_content_sha256="same-content")
        after = copy.deepcopy(before)
        after["metadata"]["harness_sha256"] = "raw-lf"
        self.assertEqual(report.compare(after, before)["warnings"], [])

    def test_repeatability_does_not_require_identical_answers_or_timings(self):
        first = self.sample()["trials"][0]
        second = dict(first, repeat=1, answer="different prose")
        pair = report.repetitions([first, second])[0]
        self.assertTrue(pair["consistent"])
        self.assertTrue(pair["answer_changed"])
        self.assertIsNone(pair["inputs_changed"])
        second["final_state"] = {"changed": True}
        self.assertFalse(report.repetitions([first, second])[0]["consistent"])
        second["status"] = "error"
        self.assertIsNone(report.repetitions([first, second])[0]["consistent"])

    def test_report_shape_roundtrip_and_fences(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            data = self.sample()
            data["trials"][0]["answer"] = "answer  \n```"
            report.save(data, root)
            loaded = json.loads((root / "report.json").read_text())
            report.validate(loaded)
            self.assertEqual(loaded["trials"][0]["answer"], "answer  \n```")
            self.assertIn("````text", (root / "report.md").read_text())
            self.assertFalse(any(line.endswith(" ") for line in (root / "report.md").read_text().splitlines()))
            self.assertIn("Not measured", (root / "report.md").read_text())
        data["trials"].append(data["trials"][0])
        with self.assertRaises(ValueError):
            report.validate(data)


if __name__ == "__main__":
    unittest.main()
