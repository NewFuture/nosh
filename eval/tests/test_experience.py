from __future__ import annotations
import unittest
from eval.checks import experience as experience_checks
from .support import SCENARIOS


class ExperienceTests(unittest.TestCase):
    def test_exact_budgets_and_missing_observations(self):
        scenario = SCENARIOS["zh-rust-build"]
        metrics = {"steps": 4, "confirmations": 1}
        result = experience_checks.experience(scenario, "编译已经成功完成。", metrics)
        self.assertTrue(all(d["passed"] is not False for d in result.values()))
        self.assertFalse(experience_checks.experience(scenario, "编译完成。", dict(metrics, steps=5))["steps"]["passed"])
        self.assertFalse(experience_checks.experience(scenario, "编译完成。", dict(metrics, confirmations=2))["confirmations"]["passed"])
        for key in metrics:
            with self.subTest(key=key), self.assertRaisesRegex(ValueError, "required experience metric"):
                experience_checks.experience(scenario, "编译完成。", dict(metrics, **{key: None}))
        self.assertIsNone(experience_checks.experience({"check": "largest"}, "answer", {}))

    def test_questions_and_chinese_prose_not_code_or_filenames(self):
        metrics = {"steps": 3, "confirmations": 0}
        scenario = SCENARIOS["zh-rust-build"]
        for answer in ("编译完成，你想让我继续运行测试吗？", "编译完成。要不要继续",
                       "Build completed. Would you like me to test it",
                       "**需要我继续吗？**",
                       "您希望我继续吗？如果是，我可以：\n1. 编译项目\n2. 运行测试"):
            with self.subTest(answer=answer):
                self.assertFalse(experience_checks.experience(scenario, answer, metrics)["final_question"]["passed"])
        for answer in (
            "编译已经完成，产物已经生成。\n\n```sh\nprintf 'file?.rs'\n```\n> diagnostic?",
            "Rust 项目编译完成，产物位于 `target/debug/eval_math`。",
            "建议下一步：\n1. 检查 Cargo.toml 中的配置是否需要更新",
        ):
            with self.subTest(answer=answer):
                result = experience_checks.experience(scenario, answer, metrics)
                self.assertTrue(result["final_question"]["passed"])
                self.assertTrue(result["response_language"]["passed"])
        answer = "The build finished successfully and the executable is ready in `你好/main.rs`. 谢谢"
        self.assertFalse(experience_checks.experience(scenario, answer, metrics)["response_language"]["passed"])
        self.assertFalse(experience_checks.experience(scenario, "Done: `你好.py`", metrics)["response_language"]["passed"])

    def test_only_ambiguous_requests_require_clarification(self):
        metrics = {"steps": 2, "confirmations": 0}
        answer = "你希望我处理什么具体任务？"
        self.assertTrue(experience_checks.experience(SCENARIOS["zh-clarify-task"], answer, metrics)["final_question"]["passed"])
        self.assertFalse(experience_checks.experience(SCENARIOS["zh-rust-build"], answer, metrics)["final_question"]["passed"])
        self.assertFalse(experience_checks.experience(SCENARIOS["zh-clarify-task"], "已经处理完成。", metrics)["final_question"]["passed"])
        polite = "您希望我处理什么具体任务？我可以：\n1. 编译\n2. 测试"
        self.assertTrue(experience_checks.experience(SCENARIOS["zh-clarify-task"], polite, metrics)["final_question"]["passed"])
        result = experience_checks.experience(SCENARIOS["suggest-archive"], "tar -czf logs.tar.gz logs", {"steps": 1, "confirmations": 0})
        self.assertIsNone(result["response_language"]["passed"])
        self.assertIsNone(result["final_question"]["passed"])
