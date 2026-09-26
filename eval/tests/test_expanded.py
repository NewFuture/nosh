from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import patch

from eval import approval, checks, driver, fixtures, observations, report, run


SUITE = run.load_suite(run.HERE / "scenarios.json")
SCENARIOS = {s["id"]: s for s in SUITE["scenarios"]}


def execution(command, code=0, stdout="", stderr=""):
    return {
        "call": {"name": "run_command", "args": {"command": command}},
        "state": "executed", "exit_code": code, "timed_out": False, "interrupted": False,
        "result": f"[exit_code={code} duration=0.00s truncated=no]\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}",
    }


class ExpandedContractTests(unittest.TestCase):
    def test_original_ten_inputs_fixtures_and_policies_are_preserved(self):
        baseline = json.loads((run.HERE / "baselines" / "main-4f602ab" / "report.json").read_text(encoding="utf-8"))
        legacy = copy.deepcopy(SUITE)
        legacy["schema_version"] = 1
        legacy["scenarios"] = [
            {k: v for k, v in s.items() if k not in ("expect", "completions", "group")}
            for s in legacy["scenarios"] if s["group"] == "mvp"
        ]
        self.assertEqual(legacy["scenarios"], baseline["metadata"]["scenarios"])
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "suite.json"
            path.write_text(json.dumps(legacy), encoding="utf-8")
            self.assertEqual(run.load_suite(path), legacy)
        short = [s for s in SUITE["scenarios"] if s["group"] == "expanded" and len(s["inputs"]) == 1]
        self.assertEqual(len(short), 12)
        self.assertTrue(all(not s["inputs"][0].startswith("#") and s["mode"] == "repl" for s in short))
        self.assertEqual(len(SCENARIOS), 25)

    def test_strict_experience_and_completion_contracts(self):
        cases = [
            ("expect", {"max_steps": 4}),
            ("expect", dict(SCENARIOS["zh-rust-build"]["expect"], max_steps=True)),
            ("expect", dict(SCENARIOS["zh-rust-build"]["expect"], max_steps=0)),
            ("expect", dict(SCENARIOS["zh-rust-build"]["expect"], max_confirmations=-1)),
            ("expect", dict(SCENARIOS["zh-rust-build"]["expect"], final_question="require")),
            ("expect", dict(SCENARIOS["zh-rust-build"]["expect"], response_language="not_applicable")),
            ("expect", dict(SCENARIOS["zh-rust-build"]["expect"], unknown=1)),
            ("completions", []),
            ("completions", [{"kind": "shell", "exit_code": True, "contains": ["error"]}]),
            ("completions", [{"kind": "agent", "extra": True}]),
            ("approval", "node-build"),
            ("fixture", []),
            ("group", "unclassified"),
        ]
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "suite.json"
            for field, value in cases:
                suite = dict(SUITE, scenarios=[dict(SCENARIOS["zh-rust-build"], **{field: value})])
                path.write_text(json.dumps(suite), encoding="utf-8")
                with self.subTest(field=field, value=value), self.assertRaises(ValueError):
                    run.load_suite(path)

    def test_only_selected_tools_are_required(self):
        old = [s for s in SUITE["scenarios"] if s["group"] == "mvp"]
        self.assertEqual(run.required_tools(old), {"git", "bash", "python3", "tar", "ss"})
        self.assertNotIn("npm", run.required_tools([SCENARIOS["zh-tool-versions"]]))
        self.assertNotIn("cc", run.required_tools([SCENARIOS["zh-tool-versions"]]))
        with patch("eval.run.shutil.which", return_value=None), self.assertRaisesRegex(ValueError, "missing required executable"):
            run.discover_tools(old)

    def test_execution_results_are_correlated_by_engine_and_session(self):
        events = [
            {"ev": "step_end", "engine": 1, "sid": 1, "tool_calls": [execution("cargo build")["call"]]},
            {"ev": "step_end", "engine": 2, "sid": 1, "tool_calls": [execution("npm test")["call"]]},
            {"ev": "step_start", "engine": 2, "sid": 1, "messages": [
                {"role": "tool", "text": "[denied] command was not run"}]},
            {"ev": "step_start", "engine": 1, "sid": 1, "messages": [
                {"role": "user", "text": "not a result"},
                {"role": "tool", "text": execution("cargo build")["result"]}]},
        ]
        rows = observations.execution_evidence(events)
        self.assertEqual([r["state"] for r in rows], ["executed", "not_executed"])
        self.assertEqual([r["exit_code"] for r in rows], [0, None])
        self.assertEqual(observations.execution_evidence(events[:1])[0]["state"], "unobserved")
        for bad in (
            {"ev": "step_end", "sid": [], "tool_calls": []},
            {"ev": "step_end", "tool_calls": [None]},
            {"ev": "step_start", "messages": [None]},
        ):
            with self.subTest(event=bad), self.assertRaises(ValueError):
                observations.execution_evidence([bad])

    def test_multiple_agent_turns_do_not_hide_an_incomplete_task(self):
        text = ("| + 2 steps | 0.1 s\n| stats: ttft 0.01s\n"
                "| ! 3 steps | 0.2 s\n| stats: ttft 0.02s\n")
        observed = observations.observe(driver.Result(transcript=text), SCENARIOS["zh-rust-build"], Path("unused"), True, 0)
        self.assertEqual(observed["metrics"]["steps"], 5)
        self.assertEqual(observed["metrics"]["task_status"], "incomplete")

    def test_only_proven_inflight_model_timeout_is_a_task_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "engine.jsonl"
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "task"}]},
                {"ev": "step_end", "sid": 1, "text": "intermediate, not final", "tool_calls": [],
                 "usage": {"ttft_s": 0.2}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "continue"}]},
            ]
            def write(rows):
                trace.write_text("\n".join(json.dumps(dict(e, schema_version=1, engine=1)) for e in rows))
            result = driver.Result(exit_code=-9, error="deadline", timeout_phase="agent", total_s=240)
            write(events)
            observed = observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, inflight_timeout=True)
            self.assertEqual(observed["metrics"]["steps"], 2)
            self.assertEqual(observed["metrics"]["ttft_s"], 0.2)
            self.assertEqual(observed["metrics"]["task_status"], "timed_out")
            self.assertEqual(observed["answer"], "")
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0)
            for invalid in (events[:-1], events + [{"ev": "close", "sid": 1}], events + [events[-1]]):
                write(invalid)
                with self.assertRaises(ValueError):
                    observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, inflight_timeout=True)
            write(events[:3])
            observed = observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, inflight_timeout=True)
            self.assertIsNone(observed["metrics"]["ttft_s"])
            result.timeout_phase = "initial_prompt"
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, inflight_timeout=True)
            result.timeout_phase = "agent"
            result.exit_code = 0
            with self.assertRaises(ValueError):
                observations.observe(result, SCENARIOS["zh-clean-build"], trace, False, 0, inflight_timeout=True)

    def test_legacy_failure_contract_is_normalized(self):
        scenario = {"inputs": ["python3 broken.py", "#"], "check": "failure"}
        self.assertEqual(driver.input_contracts(scenario), [
            {"kind": "shell", "exit_code": 1, "contains": ["FileNotFoundError"]}, {"kind": "agent"},
        ])


class ExperienceTests(unittest.TestCase):
    def test_exact_budgets_and_missing_observations(self):
        scenario = SCENARIOS["zh-rust-build"]
        metrics = {"steps": 4, "confirmations": 1}
        result = checks.experience(scenario, "编译已经成功完成。", metrics)
        self.assertTrue(all(d["passed"] is not False for d in result.values()))
        self.assertFalse(checks.experience(scenario, "编译完成。", dict(metrics, steps=5))["steps"]["passed"])
        self.assertFalse(checks.experience(scenario, "编译完成。", dict(metrics, confirmations=2))["confirmations"]["passed"])
        for key in metrics:
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "required experience metric"):
                checks.experience(scenario, "编译完成。", dict(metrics, **{key: None}))
        self.assertIsNone(checks.experience({"check": "largest"}, "answer", {}))

    def test_questions_and_chinese_prose_not_code_or_filenames(self):
        metrics = {"steps": 3, "confirmations": 0}
        scenario = SCENARIOS["zh-rust-build"]
        for answer in ("编译完成，你想让我继续运行测试吗？", "编译完成。要不要继续",
                       "Build completed. Would you like me to test it",
                       "**需要我继续吗？**",
                       "您希望我继续吗？如果是，我可以：\n1. 编译项目\n2. 运行测试"):
            with self.subTest(answer=answer):
                self.assertFalse(checks.experience(scenario, answer, metrics)["final_question"]["passed"])
        for answer in (
            "编译已经完成，产物已经生成。\n\n```sh\nprintf 'file?.rs'\n```\n> diagnostic?",
            "Rust 项目编译完成，产物位于 `target/debug/eval_math`。",
            "建议下一步：\n1. 检查 Cargo.toml 中的配置是否需要更新",
        ):
            with self.subTest(answer=answer):
                result = checks.experience(scenario, answer, metrics)
                self.assertTrue(result["final_question"]["passed"])
                self.assertTrue(result["response_language"]["passed"])
        answer = "The build finished successfully and the executable is ready in `你好/main.rs`. 谢谢"
        self.assertFalse(checks.experience(scenario, answer, metrics)["response_language"]["passed"])
        self.assertFalse(checks.experience(scenario, "Done: `你好.py`", metrics)["response_language"]["passed"])

    def test_only_ambiguous_requests_require_clarification(self):
        metrics = {"steps": 2, "confirmations": 0}
        answer = "你希望我处理什么具体任务？"
        self.assertTrue(checks.experience(SCENARIOS["zh-clarify-task"], answer, metrics)["final_question"]["passed"])
        self.assertFalse(checks.experience(SCENARIOS["zh-rust-build"], answer, metrics)["final_question"]["passed"])
        self.assertFalse(checks.experience(SCENARIOS["zh-clarify-task"], "已经处理完成。", metrics)["final_question"]["passed"])
        polite = "您希望我处理什么具体任务？我可以：\n1. 编译\n2. 测试"
        self.assertTrue(checks.experience(SCENARIOS["zh-clarify-task"], polite, metrics)["final_question"]["passed"])
        result = checks.experience(SCENARIOS["suggest-archive"], "tar -czf logs.tar.gz logs", {"steps": 1, "confirmations": 0})
        self.assertIsNone(result["response_language"]["passed"])
        self.assertIsNone(result["final_question"]["passed"])


@unittest.skipUnless(sys.platform == "linux", "real Linux project fixtures")
class ProjectTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.tools = run.discover_tools(SUITE["scenarios"])

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
        self.env = run.environment(self.home, 1, None, self.tools)
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
        self.evidence["final_state"] = checks.fixture_state(self.scenario, self.facts, self.root, after, self.result)
        return checks.judge(self.scenario, answer, self.facts, self.root, after, self.result, self.metrics, self.evidence)

    def test_real_build_and_test_commands(self):
        cases = [
            ("zh-rust-build", "cat Cargo.toml && cargo build --offline --locked 2>&1"),
            ("zh-node-build", "node build.js 2>&1"),
            ("zh-rust-test", "cargo test --manifest-path Cargo.toml --offline --locked 2>&1"),
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

    def test_scoring_reuses_the_snapshot_and_parses_each_command_once(self):
        self.prepare("zh-python-test")
        self.assertEqual(self.command("python3 -m unittest discover -s tests -v").returncode, 0)
        after = fixtures.snapshot(self.root)
        self.evidence["final_state"] = checks.fixture_state(self.scenario, self.facts, self.root, after, self.result)
        self.evidence["executions"] *= 3
        with patch("eval.fixtures.snapshot", side_effect=AssertionError("grading must use the captured state")), \
                patch("eval.approval.command_groups", wraps=approval.command_groups) as parsed:
            verdict = checks.judge(self.scenario, "测试全部通过。", self.facts, self.root,
                                   after, self.result, self.metrics, self.evidence)
        self.assertTrue(verdict.passed, verdict.reasons)
        self.assertEqual(parsed.call_count, 3)
        changed = copy.deepcopy(after)
        changed["maths.py"]["sha256"] = "modified"
        self.assertFalse(checks.completed_commands(self.evidence, self.root, self.facts, "python-test", changed))

    def test_bounded_approval_rejects_escapes_and_modified_scripts(self):
        self.prepare("zh-rust-build")
        allowed = f"cd {self.root} && cargo build --offline && ls -la"
        self.assertTrue(approval.allow_approval("rust-build", allowed, self.root, self.facts))
        alias = self.home / "bin" / "cargo"
        self.assertTrue(approval.allow_approval("rust-build", f"{alias} build --offline 2>&1", self.root, self.facts))
        self.assertTrue(approval.allow_approval("rust-build", "cargo build 2>&1 && ls", self.root, self.facts))
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
        before = checks.fixture_state(self.scenario, self.facts, self.root, fixtures.snapshot(self.root), self.result)
        info = self.root / "target" / ".rustc_info.json"
        info.write_text('{"changed": "cache metadata"}\n')
        after = checks.fixture_state(self.scenario, self.facts, self.root, fixtures.snapshot(self.root), self.result)
        self.assertEqual(before, after)
        self.assertTrue(any("target/" in name for name in before["artifacts"]))

    def test_readonly_inspection_does_not_disqualify_needed_clarification(self):
        self.prepare("zh-clarify-task")
        self.command("ls")
        self.assertTrue(self.grade("你希望我完成什么具体任务？").passed)
        (self.root / "maths.py").write_text("changed\n")
        self.assertFalse(self.grade("你希望我完成什么具体任务？").passed)


@unittest.skipUnless(sys.platform == "linux", "PTY process interfaces")
class ExpandedDriverTests(unittest.TestCase):
    def script(self, first_output, second=False):
        return """
import os, tty
tty.setraw(0)
def out(text):
    os.write(1, text.encode())
def line():
    data = b""
    while not data.endswith(b"\\r"):
        data += os.read(0, 1)
    return data
out("__NOSH_EVAL_PROMPT__ ")
line()
out(%r)
%s
assert b"exit 0" in line()
""" % (first_output, """
assert "为什么" in line().decode()
out("\\r\\n| 这是类型错误，改成整数即可。\\r\\n| + 3 steps | 0.1 s\\r\\n| stats: ttft 0.01s\\r\\n__NOSH_EVAL_PROMPT__ ")
""" if second else "")

    def test_compiler_exit_101_then_chinese_help(self):
        output = "\r\nerror[E0308]: mismatched types\r\nx exit 101 | Ctrl+G or # to ask AI\r\n__NOSH_EVAL_PROMPT__ "
        scenario = SCENARIOS["zh-build-failure"]
        for text in (output, output.replace("x exit", "✗ exit").replace(" | Ctrl", " · Ctrl")):
            with self.subTest(unicode=text != output):
                result = driver.run_repl([sys.executable, "-c", self.script(text, True)], Path.cwd(),
                                         {"PATH": "/usr/bin:/bin"}, 5, scenario, lambda *_: False)
                self.assertIsNone(result.error)
                self.assertIsNone(result.failure)
                self.assertEqual(result.turns[0]["exit_code"], 101)
                self.assertEqual(len(result.turns), 2)
                self.assertEqual(result.exit_code, 0)

    def test_wrong_route_and_unexpected_success_are_failures_not_timeouts(self):
        for scenario in (SCENARIOS["zh-rust-build"], SCENARIOS["zh-build-failure"]):
            with self.subTest(scenario=scenario["id"]):
                output = "\r\nordinary shell returned\r\n__NOSH_EVAL_PROMPT__ "
                result = driver.run_repl([sys.executable, "-c", self.script(output)], Path.cwd(),
                                         {"PATH": "/usr/bin:/bin"}, 5, scenario, lambda *_: False)
                self.assertIsNone(result.error)
                self.assertIsNotNone(result.failure)
                self.assertEqual(len(result.turns), 1)
                self.assertEqual(result.exit_code, 0)

    def test_trial_pipeline_records_grades_raw_state_and_cleanup(self):
        scenario = SCENARIOS["zh-clarify-task"]
        answer = "你希望我完成什么具体任务？"
        meta = {"run_id": "unit-test", "observation": "native-v1", "build": {"binary_sha256": "a" * 64},
                "settings": {"timeout_s": 5}, "scenarios": [scenario], "seeds": [0], "repeat": 1}
        args = SimpleNamespace(threads=1, legacy=False)

        def child(argv, cwd, env, timeout, case, approve):
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": case["inputs"][0]}]},
                {"ev": "step_end", "sid": 1, "tool_calls": [], "text": answer, "usage": {"ttft_s": 0.01}},
            ]
            Path(env["NOSH_EVAL_TRACE"]).write_text("\n".join(
                json.dumps(dict(e, schema_version=1, engine=1)) for e in events), encoding="utf-8")
            return driver.Result(exit_code=0, transcript=f"| {answer}\n| + 1 steps | 0.1 s\n| stats: ttft 0.01s\n")

        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            output = base / "output"
            output.mkdir()
            with fixtures.Workspace(base / "work") as workspace, patch("eval.run.driver.run_repl", side_effect=child):
                row = run.run_trial(args, meta, scenario, 0, 0, workspace, output, Path(sys.executable), base / "unused-model")
                self.assertEqual(row["status"], "pass", row["reasons"])
                self.assertTrue(row["grading"]["facts"]["passed"])
                self.assertTrue(row["grading"]["experience"]["final_question"]["passed"])
                self.assertIn("maths.py", row["file_snapshot"])
                self.assertFalse((workspace.root / scenario["id"]).exists())
                self.assertTrue((output / row["logs"] / "engine.jsonl").is_file())
                report.save({"schema_version": 2, "metadata": meta, "trials": [row]}, output)

    def test_rejected_requests_still_count_as_confirmations(self):
        script = """
import os, tty
tty.setraw(0)
def out(text):
    os.write(1, text.encode())
def line():
    data = b""
    while not data.endswith(b"\\r"):
        data += os.read(0, 1)
    return data
out("__NOSH_EVAL_PROMPT__ ")
line()
for _ in range(2):
    out("\\r\\n| +- run_command - MUTATING\\r\\n| | $ cargo build\\r\\n| +- [y] run [n] deny > ")
    assert os.read(0, 1) == b"n"
    out("\\r\\n| reason (optional, Enter to skip): ")
    assert line() == b"\\r"
out("\\r\\n| 未执行命令。\\r\\n| ! 3 steps | 0.1 s\\r\\n| stats: ttft 0.01s\\r\\n__NOSH_EVAL_PROMPT__ ")
assert b"exit 0" in line()
"""
        scenario = SCENARIOS["zh-rust-build"]
        result = driver.run_repl([sys.executable, "-c", script], Path.cwd(),
                                 {"PATH": "/usr/bin:/bin"}, 5, scenario, lambda *_: False)
        self.assertIsNone(result.error)
        self.assertEqual(len(result.approvals), 2)
        observed = observations.observe(result, scenario, Path("unused"), True, 0)
        self.assertEqual(observed["metrics"]["confirmations"], 2)
        self.assertFalse(checks.experience(scenario, observed["answer"], observed["metrics"])["confirmations"]["passed"])

    def test_trial_records_proven_generation_deadline_as_failure(self):
        scenario = SCENARIOS["zh-rust-build"]
        args = SimpleNamespace(threads=1, legacy=False)
        meta = {"settings": {"timeout_s": 5}}
        def child(argv, cwd, env, timeout, case, approve):
            events = [
                {"ev": "engine", "info": {"load_s": 0.1}},
                {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
                {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": case["inputs"][0]}]},
            ]
            Path(env["NOSH_EVAL_TRACE"]).write_text("\n".join(
                json.dumps(dict(e, schema_version=1, engine=1)) for e in events), encoding="utf-8")
            return driver.Result(exit_code=-9, total_s=5, error="deadline", timeout_phase="agent")
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            output = base / "output"
            output.mkdir()
            with fixtures.Workspace(base / "work") as workspace, patch("eval.run.driver.run_repl", side_effect=child):
                row = run.run_trial(args, meta, scenario, 0, 0, workspace, output, Path(sys.executable), base / "unused-model")
                self.assertEqual(row["status"], "fail", row["reasons"])
                self.assertEqual(row["metrics"]["task_status"], "timed_out")
                self.assertEqual(row["metrics"]["steps"], 1)
                self.assertIsNone(row["metrics"]["ttft_s"])
                self.assertIsNone(row["grading"]["experience"])
                self.assertEqual(row["answer"], "")
                self.assertFalse((workspace.root / scenario["id"]).exists())


class ExpandedReportTests(unittest.TestCase):
    def data(self):
        data = {
            "schema_version": 2,
            "metadata": {"run_id": "unit-test", "observation": "native-v1",
                         "build": {"binary_sha256": "a" * 64},
                         "scenarios": SUITE["scenarios"], "seeds": SUITE["seeds"], "repeat": 2},
            "trials": [],
        }
        for sid, steps, confirmations in (("largest-files", 4, 0), ("typo-correction", 0, 0), ("zh-rust-build", 2, 1)):
            metrics = {"steps": steps, "confirmations": confirmations, "ttft_s": None, "total_s": 2, "peak_rss_mib": 10}
            data["trials"].append({
                "scenario_id": sid, "seed": 0, "repeat": 0, "status": "pass", "metrics": metrics,
                "answer": "任务已经完成。", "final_state": {}, "inputs": None,
                "grading": {"facts": {"passed": True, "reasons": []},
                            "experience": checks.experience(SCENARIOS[sid], "任务已经完成。", metrics)},
            })
        return data

    def test_250_trial_denominator_groups_and_weighted_means(self):
        data = self.data()
        rows = {r["group"]: r for r in report.groups(data)}
        self.assertEqual(rows["all"]["planned"], 250)
        self.assertEqual(rows["all"]["missing"], 247)
        self.assertEqual(rows["all"]["steps"], 2)
        self.assertEqual(rows["model"]["steps"], 3)
        self.assertEqual(rows["model"]["planned"], 240)
        self.assertEqual(rows["local"]["planned"], 10)
        self.assertEqual(rows["expanded"]["planned"], 150)
        self.assertEqual(rows["mvp"]["planned"], 100)
        data["metadata"]["repeat"] = 1
        self.assertEqual(report.groups(data)[0]["planned"], 125)

    def test_v2_round_trip_and_no_success_shaped_missing_grades(self):
        data = self.data()
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            report.save(data, root)
            loaded = json.loads((root / "report.json").read_text(encoding="utf-8"))
            report.validate(loaded)
            text = (root / "report.md").read_text(encoding="utf-8")
            self.assertIn("3/250", text)
            self.assertIn("Declared experience budgets", text)
            self.assertIn("facts=pass", text)
        data["trials"][0]["grading"]["experience"] = None
        with self.assertRaises(ValueError):
            report.validate(data)
        data["trials"][0]["status"] = "error"
        report.validate(data)
        data["trials"][0]["metrics"]["steps"] = 1.5
        with self.assertRaises(ValueError):
            report.validate(data)

    def test_v1_baselines_render_unchanged(self):
        for name in ("main-4f602ab", "main-7c57a88"):
            with self.subTest(baseline=name):
                directory = run.HERE / "baselines" / name
                data = json.loads((directory / "report.json").read_text(encoding="utf-8"))
                report.validate(data)
                self.assertEqual(report.markdown(data), (directory / "report.md").read_text(encoding="utf-8"))
                comparison = report.compare(self.data(), data)
                self.assertTrue(any("schemas differ" in warning for warning in comparison["warnings"]))


if __name__ == "__main__":
    unittest.main()
