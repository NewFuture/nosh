from __future__ import annotations
import copy
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from eval import driver, fixtures, observations, run
from .support import SCENARIOS, SUITE


class ContractTests(unittest.TestCase):
    def test_workspace_cannot_inherit_project_guidance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / "isolated"
            run.validate_workspace_ancestry(work)
            (root / "NOSH.md").write_text("Legacy name is not an instruction source")
            run.validate_workspace_ancestry(work)
            for name in (
                ".git", "AGENTS.md", "README.md", "Readme.md", "readme.md", "README.rst", "README.txt", "README",
            ):
                with self.subTest(name=name):
                    path = root / name
                    path.write_text("Project-specific context")
                    with self.assertRaisesRegex(ValueError, "AGENTS.md/README ancestry"):
                        run.validate_workspace_ancestry(work)
                    path.unlink()
            (root / "AGENTS.md").symlink_to(root / "missing")
            with self.assertRaisesRegex(ValueError, "AGENTS.md/README ancestry"):
                run.validate_workspace_ancestry(work)

    def test_guidance_ancestry_does_not_treat_errors_as_absence(self):
        with patch.object(Path, "lstat", side_effect=PermissionError("blocked")):
            with self.assertRaisesRegex(PermissionError, "blocked"):
                run.validate_workspace_ancestry(Path("/isolated/workspace"))

    def test_source_fingerprint_ignores_platform_line_endings(self):
        with tempfile.TemporaryDirectory() as temporary:
            a, b = Path(temporary) / "a.py", Path(temporary) / "b.py"
            a.write_bytes(b"print(1)\r\n")
            b.write_bytes(b"print(1)\n")
            self.assertEqual(fixtures.source_hash(a), fixtures.source_hash(b))
            self.assertNotEqual(fixtures.file_hash(a), fixtures.file_hash(b))

    def test_default_suite_and_seeds(self):
        suite = run.load_suite(run.HERE / "suites" / "regression.json")
        self.assertEqual(len(suite["scenarios"]), 27)
        self.assertEqual(sum(s["group"] == "mvp" for s in suite["scenarios"]), 10)
        self.assertEqual(sum(s["group"] == "expanded" for s in suite["scenarios"]), 17)
        self.assertEqual(suite["seeds"], [0, 1, 2, 3, 4])
        self.assertEqual(run.seeds([0, 2**64 - 1]), [0, 2**64 - 1])
        for bad in ([], [True], [-1], [2**64], [0, 0], ["0"], None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                run.seeds(bad)

    def test_invalid_suite_rejected(self):
        original = run.load_suite(run.HERE / "suites" / "regression.json")
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
                observations.observe(result, {"mode": "suggest", "check": "archive"}, home / "trace", True, 0)
class ExpandedContractTests(unittest.TestCase):
    def test_original_tasks_preserve_facts_with_explicit_diagnosis_routing(self):
        baseline = json.loads((run.HERE / "baselines" / "main-4f602ab" / "report.json").read_text(encoding="utf-8"))
        legacy = copy.deepcopy(SUITE)
        legacy["schema_version"] = 1
        legacy["scenarios"] = [
            {k: v for k, v in s.items() if k not in ("expect", "completions", "group")}
            for s in legacy["scenarios"] if s["group"] == "mvp"
        ]
        previous = copy.deepcopy(baseline["metadata"]["scenarios"])
        diagnosis = next(s for s in previous if s["id"] == "explain-failure")
        self.assertEqual(diagnosis["inputs"], ["python3 broken.py", "#"])
        diagnosis["inputs"][-1] = "ai fix Explain why this failed and how to fix it without changing files."
        self.assertEqual(legacy["scenarios"], previous)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "suite.json"
            path.write_text(json.dumps(legacy), encoding="utf-8")
            self.assertEqual(run.load_suite(path), legacy)
        short = [s for s in SUITE["scenarios"] if s["group"] == "expanded" and len(s["inputs"]) == 1]
        self.assertEqual(len(short), 12)
        self.assertTrue(all(not s["inputs"][0].startswith("#") and s["mode"] == "repl" for s in short))
        self.assertEqual(len(SCENARIOS), 27)

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

    def test_failure_diagnosis_requires_the_initial_shell_failure(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "suite.json"
            for sid in ("zh-build-failure", "zh-test-failure", "zh-port-failure"):
                scenario = copy.deepcopy(SCENARIOS[sid])
                for inputs, completions in (
                    (scenario["inputs"], [{"kind": "agent"}, {"kind": "agent"}]),
                    ([scenario["inputs"][-1]], [{"kind": "agent"}]),
                ):
                    scenario.update(inputs=inputs, completions=completions)
                    path.write_text(json.dumps(dict(SUITE, scenarios=[scenario])), encoding="utf-8")
                    with self.subTest(scenario=sid, inputs=inputs), self.assertRaisesRegex(
                        ValueError, "initial failed shell command",
                    ):
                        run.load_suite(path)
    def test_legacy_failure_contract_is_normalized(self):
        scenario = {"inputs": ["python3 broken.py", "#"], "check": "failure"}
        self.assertEqual(driver.input_contracts(scenario), [
            {"kind": "shell", "exit_code": 1, "contains": ["FileNotFoundError"]}, {"kind": "agent"},
        ])


class CatalogSuiteTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.root = Path(self.directory.name)
        self.path = self.root / "suite.json"
        self.entries = copy.deepcopy(SUITE["scenarios"][:2])
        self.catalog = self.root / "cases.json"
        self.catalog.write_text(json.dumps(self.entries), encoding="utf-8")
        self.manifest = dict(SUITE, catalogs=["cases.json"],
                             scenarios=[case["id"] for case in reversed(self.entries)])

    def load(self, **changes):
        self.path.write_text(json.dumps(dict(self.manifest, **changes)), encoding="utf-8")
        return run.load_suite(self.path)

    def test_catalogs_expand_to_the_existing_contract_in_explicit_order(self):
        expected = dict(SUITE, scenarios=list(reversed(self.entries)))
        self.assertEqual(self.load(), expected)
        self.assertNotIn("catalogs", self.load())
        self.assertEqual(self.load(scenarios=[self.entries[0]["id"]])["scenarios"], self.entries[:1])

    def test_invalid_catalogs_and_selections_are_not_silently_skipped(self):
        for changes in (
            {"catalogs": []}, {"catalogs": [""]}, {"catalogs": [True]},
            {"schema_version": 1}, {"scenarios": []}, {"scenarios": [self.entries[0]]},
            {"scenarios": ["missing"]},
            {"scenarios": [self.entries[0]["id"]] * 2},
            {"catalogs": ["cases.json", "cases.json"]},
        ):
            with self.subTest(changes=changes), self.assertRaises(ValueError):
                self.load(**changes)
        with self.assertRaises(FileNotFoundError):
            self.load(catalogs=["missing.json"])
        for contents in ([], {}, [None], [{"id": "../invalid"}], self.entries * 2):
            self.catalog.write_text(json.dumps(contents), encoding="utf-8")
            with self.subTest(contents=contents), self.assertRaises(ValueError):
                self.load()

    def test_referenced_cases_still_require_valid_budgets_and_fixture_contracts(self):
        self.entries[0]["expect"]["max_steps"] = -1
        self.catalog.write_text(json.dumps(self.entries), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "nonnegative"):
            self.load()

    def test_bundled_suites_reuse_one_catalog_without_changing_cases(self):
        catalog = {}
        for path in sorted((run.HERE / "scenarios").glob("*.json")):
            for scenario in json.loads(path.read_text(encoding="utf-8")):
                self.assertNotIn(scenario["id"], catalog)
                catalog[scenario["id"]] = scenario
        self.assertEqual(len(catalog), 32)
        for name, count in (("regression", 27), ("command-assist", 5), ("smoke", 5)):
            loaded = run.load_suite(run.HERE / "suites" / f"{name}.json")
            self.assertEqual(len(loaded["scenarios"]), count)
            for scenario in loaded["scenarios"]:
                self.assertEqual(scenario, catalog[scenario["id"]])
        smoke = run.load_suite(run.HERE / "suites" / "smoke.json")
        self.assertEqual(smoke["seeds"], [0])
