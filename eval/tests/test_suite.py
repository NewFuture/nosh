from __future__ import annotations
import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch
from eval import approval, contracts, driver, fixtures, observations, runtime, suite, suite as suite_api
from .support import SCENARIOS, SUITE


class ContractTests(unittest.TestCase):
    def test_registry_is_the_shared_source_of_check_and_policy_relationships(self):
        self.assertIs(suite.CHECK_FIXTURES, contracts.CHECK_FIXTURES)
        self.assertIs(approval.APPROVAL_CHECKS, contracts.APPROVAL_CHECKS)
        self.assertEqual(set(contracts.CHECK_FIXTURES.values()), fixtures.FIXTURES)
        for name, spec in contracts.CHECK_SPECS.items():
            self.assertIs(contracts.check_spec(name), spec)
            self.assertIn(spec.family, ("agent", "project", "capture", "assist"))
            if spec.approval != "deny":
                self.assertIn(name, approval.APPROVAL_CHECKS[spec.approval])
        self.assertEqual(approval.PROJECT_POLICY_FIXTURES["git-commit"], {"staged-git"})
        with self.assertRaisesRegex(ValueError, "unknown check"):
            contracts.check_spec("unknown")


    def test_suite_loading_has_no_runtime_or_scorer_dependency(self):
        result = subprocess.run([
            sys.executable, "-B", "-c",
            "import sys; from eval.suite import load_suite; load_suite('smoke'); "
            "assert not {'eval.run', 'eval.runtime', 'eval.driver', 'eval.fixtures', 'eval.checks'} & sys.modules.keys()",
        ], cwd=runtime.ROOT, capture_output=True, text=True, timeout=10)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_suite_names_paths_and_in_memory_validation_share_the_same_contract(self):
        for name in suite.BUILTIN_SUITES:
            with self.subTest(name=name):
                expanded = suite.load_suite(name)
                self.assertEqual(expanded, suite.load_suite(runtime.HERE / "suites" / f"{name}.json"))
                before = copy.deepcopy(expanded)
                self.assertEqual(suite.validate_suite(expanded), before)
                self.assertEqual(expanded, before)
        self.assertEqual(suite.suite_path(Path("smoke")), Path("smoke"))
        catalog = json.loads((runtime.HERE / "suites" / "smoke.json").read_text())
        with self.assertRaisesRegex(ValueError, "catalog_root"):
            suite.validate_suite(catalog)
        self.assertEqual(suite.validate_suite(catalog, catalog_root=runtime.HERE / "suites"),
                         suite.load_suite("smoke"))
        for value in (None, [], {}, 1, True):
            invalid = copy.deepcopy(SUITE)
            invalid["scenarios"][0]["check"] = value
            with self.subTest(value=value), self.assertRaisesRegex(ValueError, "unknown fixture or check"):
                suite.validate_suite(invalid)

    def test_workspace_cannot_inherit_project_guidance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            work = root / "isolated"
            runtime.validate_workspace_ancestry(work)
            (root / "NOSH.md").write_text("Legacy name is not an instruction source")
            runtime.validate_workspace_ancestry(work)
            for name in (
                ".git", "AGENTS.md", "README.md", "Readme.md", "readme.md", "README.rst", "README.txt", "README",
            ):
                with self.subTest(name=name):
                    path = root / name
                    path.write_text("Project-specific context")
                    with self.assertRaisesRegex(ValueError, "AGENTS.md/README ancestry"):
                        runtime.validate_workspace_ancestry(work)
                    path.unlink()
            (root / "AGENTS.md").symlink_to(root / "missing")
            with self.assertRaisesRegex(ValueError, "AGENTS.md/README ancestry"):
                runtime.validate_workspace_ancestry(work)

    def test_guidance_ancestry_does_not_treat_errors_as_absence(self):
        with patch.object(Path, "lstat", side_effect=PermissionError("blocked")):
            with self.assertRaisesRegex(PermissionError, "blocked"):
                runtime.validate_workspace_ancestry(Path("/isolated/workspace"))

    def test_source_fingerprint_ignores_platform_line_endings(self):
        with tempfile.TemporaryDirectory() as temporary:
            a, b = Path(temporary) / "a.py", Path(temporary) / "b.py"
            a.write_bytes(b"print(1)\r\n")
            b.write_bytes(b"print(1)\n")
            self.assertEqual(fixtures.source_hash(a), fixtures.source_hash(b))
            self.assertNotEqual(fixtures.file_hash(a), fixtures.file_hash(b))

    def test_default_suite_and_seeds(self):
        suite = suite_api.load_suite(runtime.HERE / "suites" / "regression.json")
        self.assertEqual(len(suite["scenarios"]), 27)
        self.assertEqual(sum(s["group"] == "mvp" for s in suite["scenarios"]), 10)
        self.assertEqual(sum(s["group"] == "expanded" for s in suite["scenarios"]), 17)
        self.assertEqual(suite["seeds"], [0, 1, 2, 3, 4])
        self.assertEqual(suite_api.seeds([0, 2**64 - 1]), [0, 2**64 - 1])
        for bad in ([], [True], [-1], [2**64], [0, 0], ["0"], None):
            with self.subTest(bad=bad), self.assertRaises(ValueError):
                suite_api.seeds(bad)

    def test_invalid_suite_rejected(self):
        original = suite_api.load_suite(runtime.HERE / "suites" / "regression.json")
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "suite.json"
            for field, value in (("id", "../outside"), ("mode", "unknown"), ("approval", "yolo"),
                                 ("fixture", "host"), ("fixture", "project"), ("inputs", ["# x\nrm x"]),
                                 ("unknown_option", True)):
                suite = copy.deepcopy(original)
                suite["scenarios"][0][field] = value
                path.write_text(json.dumps(suite))
                with self.subTest(field=field), self.assertRaises(ValueError):
                    suite_api.load_suite(path)

    def test_input_types_are_checked_before_route_specific_parsing(self):
        for sid in ("captured-diagnosis", "zh-build-failure", "largest-files"):
            for value in (None, {}, [], 1, True):
                data = copy.deepcopy(SUITE)
                scenario = next(s for s in data["scenarios"] if s["id"] == sid)
                scenario["inputs"][-1] = value
                with self.subTest(scenario=sid, value=value), self.assertRaisesRegex(ValueError, "single lines"):
                    suite_api.validate_suite(data)
        with self.assertRaisesRegex(ValueError, "finite"):
            suite_api.validate_suite(dict(SUITE, timeout_s=10**400))

    def test_stdin_producers_are_not_silently_ignored_in_repl_mode(self):
        data = copy.deepcopy(SUITE)
        data["scenarios"][0]["stdin_command"] = ["git", "log", "--stat", "-8"]
        with self.assertRaisesRegex(ValueError, "stdin attachments require agent mode"):
            suite_api.validate_suite(data)


    def test_isolated_config_uses_the_existing_string_contract(self):
        import tomllib

        with tempfile.TemporaryDirectory() as temporary:
            home = Path(temporary)
            env = runtime.environment(home, 4, None)
            cfg = tomllib.loads((Path(env["NOSH_HOME"]) / "config.toml").read_text())
            self.assertEqual(cfg["model"]["thinking"], "off")
            result = driver.Result(stdout="tar -czf logs.tar.gz logs", exit_code=0,
                                   stderr="nosh: /tmp/config.toml: model.thinking: expected a string\n")
            with self.assertRaises(ValueError):
                observations.observe(result, {"mode": "suggest", "check": "archive"}, home / "trace", seed=0)
class ExpandedContractTests(unittest.TestCase):
    def test_current_tasks_declare_diagnosis_routing_and_short_requests(self):
        diagnosis = SCENARIOS["explain-failure"]
        self.assertEqual(diagnosis["inputs"][0], "python3 broken.py")
        self.assertTrue(diagnosis["inputs"][1].startswith("ai fix "))
        self.assertEqual(diagnosis["completions"][0]["kind"], "shell")
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
                    suite_api.load_suite(path)

    def test_only_selected_tools_are_required(self):
        old = [s for s in SUITE["scenarios"] if s["group"] == "mvp"]
        self.assertEqual(runtime.required_tools(old), {"git", "bash", "python3", "tar", "ss"})
        self.assertNotIn("npm", runtime.required_tools([SCENARIOS["zh-tool-versions"]]))
        self.assertNotIn("cc", runtime.required_tools([SCENARIOS["zh-tool-versions"]]))
        with patch("eval.runtime.shutil.which", return_value=None), self.assertRaisesRegex(ValueError, "missing required executable"):
            runtime.discover_tools(old)

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
                        suite_api.load_suite(path)


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
        return suite_api.load_suite(self.path)

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
        for path in sorted((runtime.HERE / "scenarios").glob("*.json")):
            for scenario in json.loads(path.read_text(encoding="utf-8")):
                self.assertNotIn(scenario["id"], catalog)
                catalog[scenario["id"]] = scenario
        self.assertEqual(len(catalog), 39)
        for name, count in (("regression", 27), ("command-assist", 5), ("smoke", 5), ("workflows", 8)):
            loaded = suite_api.load_suite(runtime.HERE / "suites" / f"{name}.json")
            self.assertEqual(len(loaded["scenarios"]), count)
            for scenario in loaded["scenarios"]:
                self.assertEqual(scenario, catalog[scenario["id"]])
        smoke = suite_api.load_suite(runtime.HERE / "suites" / "smoke.json")
        self.assertEqual(smoke["seeds"], [0])
