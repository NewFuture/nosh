from __future__ import annotations
import copy
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from eval import approval, checks, driver, fixtures, runtime
from eval.checks import project as project_checks
from .support import SCENARIOS, SUITE, execution


@unittest.skipUnless(sys.platform == "linux", "real Linux project fixtures")
class ProjectTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tools = runtime.discover_tools(SUITE["scenarios"])

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.base = Path(self.temporary.name)
        self.workspace = fixtures.Workspace(self.base / "work", self.tools).__enter__()

    def tearDown(self):
        self.workspace.__exit__()
        self.temporary.cleanup()

    def prepare(self, sid):
        self.scenario = copy.deepcopy(SCENARIOS[sid])
        self.root, self.home, self.facts = self.workspace.prepare(self.scenario)
        self.env = runtime.environment(self.home, 1, None, self.tools)
        self.facts["tools"] = self.tools
        if self.scenario["check"] == "versions":
            self.facts["versions"] = {n: self.tools[n]["version"] for n in ("cargo", "node", "python3")}
        if self.scenario["check"] == "recent-history":
            self.facts["git_before"] = fixtures.git_state(self.root)
            self.facts["commit_ids"] = fixtures.git(self.root, "log", "--format=%H").splitlines()
        self.result = driver.Result(exit_code=0)
        self.evidence = {"executions": []}
        self.metrics = {"task_status": "completed", "steps": min(3, self.scenario["expect"]["max_steps"]),
                        "confirmations": self.scenario["expect"]["max_confirmations"]}

    def command(self, command):
        result = driver.run_cli(["bash", "--noprofile", "--norc", "-c", command], self.root, self.env, 60)
        self.assertIsNone(result.error, result.error)
        proc = subprocess.CompletedProcess(command, result.exit_code, result.stdout, result.stderr)
        self.evidence["executions"].append(execution(command, proc.returncode, proc.stdout, proc.stderr))
        return proc

    def grade(self, answer="任务已经完成。"):
        after = fixtures.snapshot(self.root)
        self.evidence["final_state"] = fixtures.fixture_state(self.scenario, self.facts, self.root, after, self.result)
        return checks.judge(self.scenario, answer, self.facts, self.root, after, self.result, self.metrics, self.evidence)

    def test_real_build_and_test_commands(self):
        cases = [
            ("zh-rust-build", "cat Cargo.toml && cargo build --offline --locked -j 1 2>&1"),
            ("zh-node-build", "node build.js 2>&1"),
            ("zh-rust-test", "cargo test --manifest-path Cargo.toml --offline --locked --all-features 2>&1"),
            ("zh-node-test", "npm test 2>&1"),
            ("zh-python-test", "python3 -m unittest discover -s tests -v 2>&1"),
        ]
        for sid, command in cases:
            with self.subTest(scenario=sid):
                self.prepare(sid)
                self.assertTrue(approval.allow_approval(self.scenario["approval"], command, self.root, self.facts))
                proc = self.command(command)
                self.assertEqual(proc.returncode, 0, proc.stdout + proc.stderr)
                verdict = self.grade()
                self.assertTrue(verdict.passed, verdict.reasons)
                self.evidence["executions"] = []
                self.assertFalse(self.grade().passed, "state/output alone must not imply agent execution")

    def test_real_cleanup_preserves_source_and_rejects_extra_files(self):
        self.prepare("zh-clean-build")
        self.assertTrue((self.root / "target" / "debug" / "eval_math").is_file())
        self.assertTrue(approval.allow_approval("rust-clean", "cargo clean", self.root, self.facts))
        self.assertEqual(self.command("cargo clean").returncode, 0)
        self.assertTrue(self.grade().passed, self.grade().reasons)
        (self.root / "src" / "main.rs").unlink()
        self.assertFalse(self.grade().passed)

    def test_build_success_cannot_hide_wrong_artifacts_or_source_changes(self):
        self.prepare("zh-node-build")
        self.assertEqual(self.command("npm run build").returncode, 0)
        (self.root / "dist" / "math.js").write_text("wrong artifact\n")
        self.assertFalse(self.grade().passed)
        self.prepare("zh-rust-build")
        self.assertEqual(self.command("cargo build --offline").returncode, 0)
        (self.root / "target" / "unexpected.txt").write_text("not a compiler output")
        self.assertFalse(self.grade().passed)

    def test_test_counts_and_nonexecution_are_not_success(self):
        self.prepare("zh-python-test")
        for command, text in (
            ("python3 -m unittest discover -s tests", "Ran 0 tests in 0.001s\n\nOK\n"),
            ("python3 -m unittest discover -s tests", "Ran 2 tests in 0.001s\n\nFAILED\n"),
            ("python3 -m unittest discover -s tests; echo fake", "Ran 2 tests in 0.001s\n\nOK\n"),
        ):
            with self.subTest(command=command, text=text):
                self.evidence["executions"] = [execution(command, stdout=text)]
                self.assertFalse(self.grade().passed)

    def test_semicolon_cannot_mask_a_failed_project_command(self):
        self.prepare("zh-test-failure")
        command = "python3 -m unittest discover -s tests -v; ls"
        proc = self.command(command)
        self.assertEqual(proc.returncode, 0, "the shell exposes only the final command's status")
        self.assertIn("FAILED", proc.stderr)
        self.assertFalse(approval.allow_approval("python-test", command, self.root, self.facts))
        after = fixtures.snapshot(self.root)
        self.assertEqual(project_checks.completed_commands(self.evidence, self.root, self.facts, "python-test", after), [])
        for accepted in (
            "python3 -m unittest discover -s tests -v && ls",
            "python3 -m unittest discover -s tests -v;",
        ):
            self.assertTrue(approval.allow_approval("python-test", accepted, self.root, self.facts), accepted)
        for rejected in (
            "python3 -m unittest discover -s tests -v 2>&1; ls",
            "python3 -m unittest discover -s tests -v; cat README.md",
        ):
            self.assertFalse(approval.allow_approval("python-test", rejected, self.root, self.facts), rejected)

    def test_scoring_reuses_the_snapshot_and_parses_each_command_once(self):
        self.prepare("zh-python-test")
        self.assertEqual(self.command("python3 -m unittest discover -s tests -v").returncode, 0)
        after = fixtures.snapshot(self.root)
        self.evidence["final_state"] = fixtures.fixture_state(self.scenario, self.facts, self.root, after, self.result)
        self.evidence["executions"] *= 3
        with patch("eval.fixtures.snapshot", side_effect=AssertionError("grading must use the captured state")), \
                patch("eval.approval.command_groups", wraps=approval.command_groups) as parsed:
            verdict = checks.judge(self.scenario, "测试全部通过。", self.facts, self.root,
                                   after, self.result, self.metrics, self.evidence)
        self.assertTrue(verdict.passed, verdict.reasons)
        self.assertEqual(parsed.call_count, 3)
        changed = copy.deepcopy(after)
        changed["maths.py"]["sha256"] = "modified"
        self.assertFalse(project_checks.completed_commands(self.evidence, self.root, self.facts, "python-test", changed))

    def test_bounded_approval_rejects_escapes_and_modified_scripts(self):
        self.prepare("zh-rust-build")
        allowed = f"cd {self.root} && cargo build --offline && ls -la"
        self.assertTrue(approval.allow_approval("rust-build", allowed, self.root, self.facts))
        alias = self.home / "bin" / "cargo"
        self.assertTrue(approval.allow_approval("rust-build", f"{alias} build --offline 2>&1", self.root, self.facts))
        self.assertTrue(approval.allow_approval("rust-build", "cargo build 2>&1 && ls", self.root, self.facts))
        for flags in ("-j 1", "--jobs 1", "-j1", "--jobs=1", "--all-features", "--no-default-features"):
            self.assertTrue(approval.allow_approval("rust-build", f"cargo build {flags}", self.root, self.facts), flags)
        for flags in ("-j 2", "--jobs=2", "-j", "--jobs 0", "--jobs 1 2", "--features other"):
            self.assertFalse(approval.allow_approval("rust-build", f"cargo build {flags}", self.root, self.facts), flags)
        self.assertFalse(approval.allow_approval("rust-test", "cargo test -- -j 1", self.root, self.facts))
        for command in (
            "cargo build --manifest-path ../Cargo.toml", "cargo build; touch outside",
            "cargo build || curl https://example.invalid", "cargo install evil",
            "cat ../home/nosh/config.toml && cargo build",
            "./cargo build", "cargo build &&", "env -i cargo build",
            "cargo build; git push", "rm -rf .",
            "cargo build > output.txt", "cargo build 2> error.txt", "cargo build 2>&10",
            "cargo build 2>&1 || true", "cargo build '2>&1'", "cargo build 2>&1; touch outside",
            "cargo build --manifest-path ../Cargo.toml 2>&1",
        ):
            with self.subTest(command=command):
                self.assertFalse(approval.allow_approval("rust-build", command, self.root, self.facts))
        outside = self.base / "outside"
        outside.mkdir()
        (self.root / "target").symlink_to(outside, target_is_directory=True)
        self.assertFalse(approval.allow_approval("rust-clean", "rm -rf target", self.root, self.facts))
        self.prepare("zh-node-build")
        self.assertTrue(approval.allow_approval("node-build", "npm run build", self.root, self.facts))
        self.assertTrue(approval.allow_approval("node-build", "node build.js 2>&1", self.root, self.facts))
        self.assertFalse(approval.allow_approval("node-build", "node ../outside.js 2>&1", self.root, self.facts))
        (self.root / "build.js").write_text("process.exit(0)\n")
        self.assertFalse(approval.allow_approval("node-build", "npm run build", self.root, self.facts))

    def test_git_commit_tree_parent_and_clean_index(self):
        self.prepare("zh-git-commit")
        command = 'git add . && git commit -m "Add subtraction and document tests"'
        self.assertTrue(approval.allow_approval("git-commit", command, self.root, self.facts))
        self.assertEqual(self.command(command).returncode, 0)
        verdict = self.grade("改动已经提交，工作区干净。")
        self.assertTrue(verdict.passed, verdict.reasons)
        self.command('git commit --allow-empty -m "extra commit"')
        self.assertFalse(self.grade().passed)

    def test_dirty_diff_requires_both_facts_and_unchanged_index(self):
        self.prepare("zh-git-diff")
        answer = "已暂存：\n- maths.py：新增 subtract 减法函数。\n\n未暂存：\n- README.md：补充 unittest 测试说明。"
        self.assertTrue(self.grade(answer).passed, self.grade(answer).reasons)
        inline = "maths.py 已暂存，新增 subtract 减法函数；README.md 未暂存，补充 unittest 测试说明。"
        self.assertTrue(self.grade(inline).passed, self.grade(inline).reasons)
        reversed_staging = "未暂存：\n- maths.py：新增 subtract 减法函数。\n\n已暂存：\n- README.md：补充 unittest 测试说明。"
        self.assertFalse(self.grade(reversed_staging).passed)
        self.assertFalse(self.grade("只有 README.md 修改了测试说明。").passed)
        self.command("git add .")
        self.assertFalse(self.grade(answer).passed)

    def test_recent_history_accepts_a_truthful_recent_subset(self):
        self.prepare("zh-git-log")
        answer = "1. docs：记录离线使用方式。\n2. tests：覆盖命令超时。\n3. cli：新增命令建议模式。"
        self.assertTrue(self.grade(answer).passed, self.grade(answer).reasons)
        translated = "1. 文档：记录离线使用方式。\n2. 测试：覆盖命令超时。\n3. 命令行：新增命令建议模式。"
        self.assertTrue(self.grade(translated).passed, self.grade(translated).reasons)
        self.assertFalse(self.grade(answer.replace("docs", "shell")).passed)
        self.assertFalse(self.grade(answer + "\n不存在的提交 deadbeef。").passed)
        self.assertFalse(self.grade(answer + "\n4. auth：添加用户验证。").passed)
        latest_two = "仓库最近2个提交：\n1. docs：记录离线使用方式。\n2. tests：覆盖命令超时。"
        self.assertTrue(self.grade(latest_two).passed, self.grade(latest_two).reasons)
        skipped = latest_two.replace("tests：覆盖命令超时", "shell：支持管道")
        self.assertFalse(self.grade(skipped).passed)

    def test_version_queries_use_real_tool_versions_without_confirmations(self):
        self.prepare("zh-tool-versions")
        self.assertEqual(self.command("cargo --version; node --version; python3 --version").returncode, 0)
        answer = "工具版本如下：\n" + "\n".join(f"{name}：{version}" for name, version in self.facts["versions"].items())
        self.assertTrue(self.grade(answer).passed, self.grade(answer).reasons)
        self.metrics["confirmations"] = 1
        verdict = self.grade(answer)
        self.assertFalse(verdict.passed)
        self.assertTrue(verdict.details["facts"]["passed"])
        self.metrics["confirmations"] = 0
        self.evidence["executions"] = []
        self.assertFalse(self.grade(answer).passed)
        self.assertEqual(self.command("cargo version; node -v; python3 -V").returncode, 0)
        self.assertTrue(self.grade(answer).passed, self.grade(answer).reasons)

    def test_real_failure_diagnosis_and_source_preservation(self):
        cases = [
            ("zh-build-failure", "src/main.rs 将字符串赋给 i32 整数，类型不匹配；将字符串改成数字或解析转换。"),
            ("zh-test-failure", "maths.py 的 add 误用了减法 a - b，应改成加法 a + b。"),
        ]
        for sid, answer in cases:
            with self.subTest(scenario=sid):
                self.prepare(sid)
                proc = self.command(self.scenario["inputs"][0])
                self.assertEqual(proc.returncode, self.scenario["completions"][0]["exit_code"], proc.stdout + proc.stderr)
                self.result.turns = [{"exit_code": proc.returncode, "output": proc.stdout + proc.stderr}]
                verdict = self.grade(answer)
                self.assertTrue(verdict.passed, verdict.reasons)
                self.assertFalse(self.grade("遇到了错误。").passed)
                self.result.turns = []
                self.assertFalse(self.grade(answer).passed)

    def test_real_port_conflict_keeps_listener_alive(self):
        self.prepare("zh-port-failure")
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
        with fixtures.listener(self.root, port) as listener:
            proc = self.command(f"python3 -m http.server {port} --bind 127.0.0.1")
            self.assertEqual(proc.returncode, 1)
            self.result.turns = [{"exit_code": 1, "output": proc.stdout + proc.stderr}]
        self.facts["listener"] = listener
        answer = f"端口 {port} 已被占用，导致绑定冲突；请改用其他空闲端口。"
        self.assertTrue(self.grade(answer).passed, self.grade(answer).reasons)
        listener["alive_at_end"] = False
        self.assertFalse(self.grade(answer).passed)

    def test_stable_state_does_not_include_compiler_cache_bytes(self):
        self.prepare("zh-rust-build")
        self.assertEqual(self.command("cargo build --offline").returncode, 0)
        before = fixtures.fixture_state(self.scenario, self.facts, self.root, fixtures.snapshot(self.root), self.result)
        info = self.root / "target" / ".rustc_info.json"
        info.write_text('{"changed": "cache metadata"}\n')
        after = fixtures.fixture_state(self.scenario, self.facts, self.root, fixtures.snapshot(self.root), self.result)
        self.assertEqual(before, after)
        self.assertTrue(any("target/" in name for name in before["artifacts"]))

    def test_readonly_inspection_does_not_disqualify_needed_clarification(self):
        self.prepare("zh-clarify-task")
        self.command("ls")
        call = {"name":"ask_user", "args":{"question":"你希望我完成什么具体任务？"}}
        reply = self.scenario["completions"][0]["answers"][0]
        self.evidence["executions"].append({"call":call, "state":"returned", "result":reply})
        self.evidence["questions"] = [{"call":call, "state":"answered", "answer":reply}]
        self.assertTrue(self.grade("好的，已停止处理。").passed)
        (self.root / "maths.py").write_text("changed\n")
        self.assertFalse(self.grade("好的，已停止处理。").passed)
