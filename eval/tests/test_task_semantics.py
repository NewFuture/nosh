from __future__ import annotations

import copy
import json
import os
from pathlib import Path
import shlex
import socket
import sys
import tempfile
import unittest

from eval import approval, checks, driver, fixtures, run, suite


REPOSITORY = Path(__file__).resolve().parents[2]
SCENARIO_PATH = REPOSITORY / "eval" / "scenarios.json"


def execution(command: str, result: driver.Result | None = None) -> dict:
    result = result or driver.Result(exit_code=0)
    stdout = result.stdout or ""
    stderr = result.stderr or ""
    return {
        "call": {"name": "run_command", "args": {"command": command}},
        "state": "executed",
        "exit_code": result.exit_code,
        "timed_out": False,
        "interrupted": False,
        "result": stdout + stderr,
    }


class ProjectWorkCase(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(prefix="nosh-task-semantics-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)

    def make_fixture(self, name: str, kind: str) -> tuple[Path, dict]:
        root = self.base / name
        return root, fixtures.create(root, kind)

    def environment(self, name: str) -> dict[str, str]:
        home = self.base / f"{name}-home"
        home.mkdir()
        return fixtures.project_environment(home, self.tools)

    def run_shell(self, root: Path, env: dict[str, str], command: str) -> driver.Result:
        result = driver.run_cli(
            ["bash", "--noprofile", "--norc", "-c", command], root, env, 120,
        )
        self.assertIsNone(result.error, result.error)
        return result


@unittest.skipUnless(sys.platform == "linux", "fixture semantics require Linux/WSL")
class FixtureSemanticsTests(ProjectWorkCase):
    @classmethod
    def setUpClass(cls):
        cls.tools = run.discover_tools(suite.load_suite(SCENARIO_PATH)["scenarios"])

    def test_commit_fixture_is_all_staged_while_diff_fixture_remains_mixed(self):
        dirty_root, dirty = self.make_fixture("dirty", "dirty-git")
        staged_root, staged = self.make_fixture("staged", "staged-git")

        self.assertEqual(dirty["changed_files"], ["README.md", "maths.py"])
        self.assertEqual(staged["changed_files"], dirty["changed_files"])
        self.assertEqual(set(dirty["git_before"]["status"].splitlines()), {
            " M README.md", "M  maths.py",
        })
        self.assertEqual(set(staged["git_before"]["status"].splitlines()), {
            "M  README.md", "M  maths.py",
        })
        self.assertEqual(fixtures.git(dirty_root, "diff", "--name-only").splitlines(), ["README.md"])
        self.assertEqual(fixtures.git(dirty_root, "diff", "--cached", "--name-only").splitlines(), ["maths.py"])
        self.assertEqual(fixtures.git(staged_root, "diff", "--name-only"), "")
        self.assertEqual(
            fixtures.git(staged_root, "diff", "--cached", "--name-only").splitlines(),
            ["README.md", "maths.py"],
        )
        command = 'git commit -m "Add subtraction and document tests"'
        self.assertTrue(approval.allow_approval("git-commit", command, staged_root, staged))
        fixtures.git(staged_root, "commit", "--quiet", "-m", "Add subtraction and document tests")
        scenario = next(
            item for item in suite.load_suite(SCENARIO_PATH)["scenarios"]
            if item["id"] == "zh-git-commit"
        )
        result = driver.Result(exit_code=0)
        after = fixtures.snapshot(staged_root)
        evidence = {
            "executions": [execution(command)],
            "final_state": checks.fixture_state(scenario, staged, staged_root, after, result),
        }
        verdict = checks.judge(
            scenario,
            "改动已经提交，工作区保持干净。",
            staged,
            staged_root,
            after,
            result,
            {"task_status": "completed", "steps": 2, "confirmations": 1},
            evidence,
        )
        self.assertTrue(verdict.passed, verdict.reasons)

    def test_project_fixture_exposes_per_file_line_counts_without_content_changes(self):
        root, facts = self.make_fixture("project", "project")
        expected = {name: len(text.splitlines()) for name, text in fixtures.PROJECT.items()}
        self.assertEqual(facts["file_lines"], expected)
        self.assertEqual(list(facts["file_lines"]), list(fixtures.PROJECT))
        self.assertEqual(
            {name: (root / name).read_text(encoding="utf-8") for name in fixtures.PROJECT},
            fixtures.PROJECT,
        )

    def test_real_same_family_build_and_test_chains_are_approved_and_observed(self):
        cases = [
            (
                "rust", "rust",
                "cargo build --offline --locked && cargo test --offline --locked",
                ("rust-build", "rust-test"),
            ),
            (
                "node", "node",
                "npm run build && npm test",
                ("node-build", "node-test"),
            ),
        ]
        for name, kind, command, policies in cases:
            with self.subTest(family=name):
                root, facts = self.make_fixture(name, kind)
                for policy in policies:
                    self.assertTrue(approval.allow_approval(policy, command, root, facts), policy)
                result = self.run_shell(root, self.environment(name), command)
                self.assertEqual(result.exit_code, 0, result.stdout + result.stderr)
                after = fixtures.snapshot(root)
                evidence = {"executions": [execution(command, result)]}
                for primary in policies:
                    self.assertEqual(
                        checks.completed_commands(evidence, root, facts, primary, after),
                        evidence["executions"],
                    )
                if kind == "node":
                    self.assertIn("dist/main.js", after)
                    self.assertIn("dist/math.js", after)

    def test_secondary_action_approval_does_not_supply_primary_task_evidence(self):
        rust_root, rust = self.make_fixture("wrong-rust", "rust")
        rust_command = "cargo test --offline --locked"
        self.assertTrue(approval.allow_approval("rust-build", rust_command, rust_root, rust))
        self.assertEqual(approval.project_actions("rust-build", rust_command, rust_root, rust), {"rust-test"})
        self.assertEqual(
            checks.completed_commands(
                {"executions": [execution(rust_command)]},
                rust_root,
                rust,
                "rust-build",
                rust["before"],
            ),
            [],
        )

        node_root, node = self.make_fixture("wrong-node", "node")
        node_command = "npm run build"
        self.assertTrue(approval.allow_approval("node-test", node_command, node_root, node))
        self.assertEqual(approval.project_actions("node-test", node_command, node_root, node), {"node-build"})
        self.assertEqual(
            checks.completed_commands(
                {"executions": [execution(node_command)]},
                node_root,
                node,
                "node-test",
                node["before"],
            ),
            [],
        )
        self.assertFalse(approval.allow_approval("rust-test", "cargo test", node_root, node))

    def test_build_test_approval_still_rejects_changed_or_untrusted_paths(self):
        command = "npm run build && npm test"

        root, facts = self.make_fixture("unknown", "node")
        (root / "unexpected.out").write_text("not a known artifact\n", encoding="utf-8")
        self.assertFalse(approval.allow_approval("node-test", command, root, facts))

        root, facts = self.make_fixture("unknown-output", "node")
        (root / "dist").mkdir()
        (root / "dist" / "other.js").write_text("unknown output\n", encoding="utf-8")
        self.assertFalse(approval.allow_approval("node-test", command, root, facts))

        root, facts = self.make_fixture("symlink", "node")
        outside = self.base / "outside.js"
        outside.write_text("outside\n", encoding="utf-8")
        (root / "dist").mkdir()
        (root / "dist" / "math.js").symlink_to(outside)
        self.assertFalse(approval.allow_approval("node-test", command, root, facts))

        for name in ("build.js", "package.json", "src/math.js", "test/math.test.js"):
            with self.subTest(modified=name):
                root, facts = self.make_fixture("modified-" + name.replace("/", "-").replace(".", "-"), "node")
                (root / name).write_text("modified\n", encoding="utf-8")
                self.assertFalse(approval.allow_approval("node-test", command, root, facts))

        root, facts = self.make_fixture("unsafe-command", "node")
        for unsafe in (
            "npm install",
            "node -e 'process.exit(0)'",
            "node ../outside.js",
            "npm test > result.txt",
            "npm test || true",
            "npm run build && curl https://example.invalid",
        ):
            with self.subTest(command=unsafe):
                self.assertFalse(approval.allow_approval("node-test", unsafe, root, facts))

    def test_exact_port_reproduction_is_approved_and_keeps_owned_listener_alive(self):
        root, facts = self.make_fixture("port", "port")
        with socket.socket() as probe:
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]

        with fixtures.listener(root, port) as listener:
            facts["listener"] = listener
            command = f"python3 -m http.server {port} --bind 127.0.0.1"
            self.assertTrue(approval.allow_approval("port-failure", command, root, facts))
            self.assertTrue(approval.allow_approval("port-failure", command + " 2>&1", root, facts))
            self.assertTrue(approval.allow_approval(
                "port-failure", f"cd {shlex.quote(str(root))} && {command}", root, facts,
            ))

            wrong_port = port - 1 if port == 65535 else port + 1
            for deviation in (
                f"python3 -m http.server {wrong_port} --bind 127.0.0.1",
                f"python3 -m http.server {port} --bind 0.0.0.0",
                "python3 -c 'print(1)'",
                "python3 server.py",
                command + " --directory .",
                command + " > server.log",
                command + " 2>&1 || true",
                command + f"; kill {listener['pid']}",
                f"cd {shlex.quote(str(self.base))} && {command}",
            ):
                with self.subTest(command=deviation):
                    self.assertFalse(approval.allow_approval("port-failure", deviation, root, facts))

            result = self.run_shell(root, self.environment("port"), command + " 2>&1")
            self.assertEqual(result.exit_code, 1, result.stdout + result.stderr)
            self.assertIn("Address already in use", result.stdout + result.stderr)
            with socket.create_connection(("127.0.0.1", port), timeout=2) as connection:
                connection.sendall(b"GET / HTTP/1.0\r\nHost: 127.0.0.1\r\n\r\n")
                self.assertIn(b"HTTP/", connection.recv(128))
            os.kill(listener["pid"], 0)
            self.assertEqual(fixtures.snapshot(root), facts["before"])
            forged = dict(facts, listener={
                "pid": os.getpid(), "port": port, "process": "python3",
            })
            self.assertFalse(approval.allow_approval("port-failure", command, root, forged))

        self.assertTrue(listener["alive_at_end"])
        self.assertFalse(approval.allow_approval("port-failure", command, root, facts))
        self.assertFalse(approval.allow_approval("port-failure", command, root, {"before": facts["before"]}))


class DatasetRevisionTests(ProjectWorkCase):
    def write_suite(self, data: dict) -> Path:
        path = self.base / "suite.json"
        path.write_text(json.dumps(data, ensure_ascii=False), encoding="utf-8")
        return path

    def test_current_suite_declares_the_second_dataset_without_changing_task_shape(self):
        data = suite.load_suite(SCENARIO_PATH)
        scenarios = {scenario["id"]: scenario for scenario in data["scenarios"]}
        self.assertEqual(data["schema_version"], 2)
        self.assertEqual(data["dataset_revision"], 2)
        self.assertEqual(data["seeds"], [0, 1, 2, 3, 4])
        self.assertEqual(len(scenarios), 25)
        self.assertEqual(scenarios["zh-git-commit"]["inputs"], ["提交改动"])
        self.assertEqual(scenarios["zh-git-commit"]["fixture"], "staged-git")
        self.assertEqual(scenarios["zh-git-diff"]["fixture"], "dirty-git")
        self.assertEqual(scenarios["zh-port-failure"]["approval"], "port-failure")
        self.assertEqual(scenarios["zh-port-failure"]["expect"]["max_steps"], 6)
        self.assertEqual(scenarios["zh-port-failure"]["expect"]["max_confirmations"], 1)
        for sid in ("listening-port", "zh-listening-port", "zh-tool-versions"):
            self.assertEqual(scenarios[sid]["expect"]["max_confirmations"], 0)

    def test_dataset_revision_is_optional_positive_and_never_normalized(self):
        current = json.loads(SCENARIO_PATH.read_text(encoding="utf-8"))
        historical = copy.deepcopy(current)
        historical.pop("dataset_revision")
        next(s for s in historical["scenarios"] if s["id"] == "zh-git-commit")["fixture"] = "dirty-git"
        loaded = suite.load_suite(self.write_suite(historical))
        self.assertEqual(loaded, historical)
        self.assertNotIn("dataset_revision", loaded)
        self.assertEqual(fixtures.digest(loaded), fixtures.digest(historical))

        explicit_first = dict(historical, dataset_revision=1)
        self.assertEqual(suite.load_suite(self.write_suite(explicit_first)), explicit_first)
        revised_dirty = dict(historical, dataset_revision=2)
        with self.assertRaisesRegex(ValueError, "required facts"):
            suite.load_suite(self.write_suite(revised_dirty))

        for revision in (0, -1, True, 2.0, "2", None, []):
            invalid = dict(current, dataset_revision=revision)
            with self.subTest(revision=revision), self.assertRaisesRegex(ValueError, "dataset_revision"):
                suite.load_suite(self.write_suite(invalid))

    def test_historical_schema_one_suite_stays_byte_semantically_unchanged(self):
        current = json.loads(SCENARIO_PATH.read_text(encoding="utf-8"))
        historical = {
            "schema_version": 1,
            "seeds": current["seeds"],
            "timeout_s": current["timeout_s"],
            "scenarios": [
                {
                    key: value for key, value in scenario.items()
                    if key not in ("group", "expect", "completions")
                }
                for scenario in current["scenarios"][:10]
            ],
        }
        loaded = suite.load_suite(self.write_suite(historical))
        self.assertEqual(loaded, historical)
        self.assertNotIn("dataset_revision", loaded)


if __name__ == "__main__":
    unittest.main()
