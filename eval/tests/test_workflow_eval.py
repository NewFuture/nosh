import copy
from pathlib import Path
import subprocess
import tarfile
import tempfile
import tomllib
import unittest
from unittest import mock

from eval import campaign, fixtures, report, runtime


class SourceSelectionTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.base = Path(self.temporary.name)
        self.origin = self.base / "origin"
        self.origin.mkdir()
        fixtures.git(self.origin, "init", "--quiet", "--initial-branch=main", "--template=")
        fixtures.write(self.origin, "source.txt", "main\n")
        fixtures.git(self.origin, "add", ".")
        fixtures.git(self.origin, "commit", "--quiet", "-m", "main source")
        self.main = fixtures.git(self.origin, "rev-parse", "HEAD").strip()
        fixtures.git(self.origin, "switch", "--quiet", "-c", "feature/capture")
        fixtures.write(self.origin, "source.txt", "feature\n")
        fixtures.git(self.origin, "add", ".")
        fixtures.git(self.origin, "commit", "--quiet", "-m", "feature source")
        self.feature = fixtures.git(self.origin, "rev-parse", "HEAD").strip()
        self.checkout = self.base / "checkout"
        subprocess.run(["git", "clone", "--quiet", "--no-checkout", str(self.origin), str(self.checkout)],
                       check=True)

    def test_default_main_and_explicit_branch_are_pinned(self):
        self.assertEqual(runtime.resolve_source_revision(cwd=self.checkout), self.main)
        self.assertEqual(runtime.resolve_source_revision("feature/capture", cwd=self.checkout), self.feature)
        self.assertEqual(runtime.resolve_source_revision("feature/capture", self.main, self.checkout), self.main)
        with self.assertRaisesRegex(ValueError, "reachable"):
            runtime.resolve_source_revision("main", self.feature, self.checkout)

    def test_invalid_refs_and_revision_expressions_are_not_executed(self):
        for ref in ("", "--upload-pack=anything", "main\nother", "main:other"):
            with self.subTest(ref=ref), self.assertRaises(ValueError):
                runtime.resolve_source_revision(ref, cwd=self.checkout)
        for revision in ("HEAD", "main~1", "-bad", "A" * 40, "a" * 39):
            with self.subTest(revision=revision), self.assertRaises(ValueError):
                runtime.resolve_source_revision("main", revision, self.checkout)


class BuildToolchainTests(unittest.TestCase):
    def test_compiler_pin_matches_workspace_and_ci(self):
        toolchain = tomllib.loads(runtime.ROOT.joinpath("rust-toolchain.toml").read_text())
        version = toolchain["toolchain"]["channel"]
        workspace = tomllib.loads(runtime.ROOT.joinpath("Cargo.toml").read_text())
        self.assertEqual(workspace["workspace"]["package"]["rust-version"], version)
        self.assertEqual(set(toolchain["toolchain"]["components"]), {"clippy", "rustfmt"})
        for name in ("ci.yml", "eval.yml", "arm64-memory.yml"):
            workflow = runtime.ROOT.joinpath(".github", "workflows", name).read_text()
            self.assertIn(f"toolchain: '{version}'", workflow, name)
        self.assertFalse(workspace["workspace"]["dependencies"]["vte"]["default-features"])

    def test_artifact_client_is_loaded_as_esm_and_provenance_uses_build_cwd(self):
        workflow = runtime.ROOT.joinpath(".github", "workflows", "eval.yml").read_text()
        self.assertIn("@actions/artifact@6.2.1", workflow)
        self.assertIn("await import(pathToFileURL", workflow)
        self.assertIn("node_modules/@actions/artifact/lib/artifact.js", workflow)
        self.assertIn('["rustc", "-Vv"], cwd=os.environ["EVAL_SOURCE"]', workflow)
        self.assertIn('["cargo", "--version"], cwd=os.environ["EVAL_SOURCE"]', workflow)
        self.assertNotIn("const {DefaultArtifactClient} = require(", workflow)
        self.assertIn("npm install --global npm@12.1.0", workflow)

    def test_materialization_precedes_cache_metadata_and_covers_provenance(self):
        for name in ("ci.yml", "arm64-memory.yml"):
            workflow = runtime.ROOT.joinpath(".github", "workflows", name).read_text()
            for job in workflow.split("runs-on:")[1:]:
                self.assertLess(job.index("-- prepare --offline"), job.index("Swatinem/rust-cache"))
        workflow = runtime.ROOT.joinpath(".github", "workflows", "eval.yml").read_text()
        self.assertLess(workflow.index("git archive"), workflow.index("prepare_source_dependencies"))
        self.assertLess(workflow.index("prepare_source_dependencies"), workflow.index("Swatinem/rust-cache"))
        self.assertIn("source-dependencies.json", workflow)
        self.assertIn("prepared dependency source changed during the build", workflow)
        self.assertIn('"source_dependencies": dependencies', workflow)
        self.assertIn("verify_source_archive(Path", workflow)

    def test_selected_archive_requires_current_managed_sources(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manifest = root / "Cargo.toml"
            manifest.write_text('[workspace.dependencies]\nreedline = "0.51.0"\n')
            with self.assertRaisesRegex(ValueError, "incomplete managed"):
                runtime.prepare_source_dependencies(root)
            manifest.write_text('[workspace.dependencies]\nreedline = {path=".nosh/reedline"}\n')
            with self.assertRaisesRegex(ValueError, "incomplete"):
                runtime.prepare_source_dependencies(root)
            pin = root / "patches" / "reedline" / "source.toml"
            pin.parent.mkdir(parents=True)
            pin.write_text('repository = "official"\nrevision = "' + "a" * 40 + '"\n')
            tool = root / "tools" / "source" / "Cargo.toml"
            tool.parent.mkdir(parents=True)
            tool.write_text("[package]\n")
            with mock.patch.object(runtime.subprocess, "run", side_effect=subprocess.CalledProcessError(1, "prepare")) as run:
                with self.assertRaises(subprocess.CalledProcessError):
                    runtime.prepare_source_dependencies(root)
                self.assertEqual(run.call_args.args[0][3], str(tool))
                self.assertEqual(run.call_args.kwargs["cwd"], root)

    def test_archive_verification_detects_cache_or_build_rewriting_locked_inputs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "source"
            root.mkdir()
            lock = root / "Cargo.lock"
            lock.write_text("pinned dependency graph\n")
            archive = Path(directory) / "source.tar"
            with tarfile.open(archive, "w") as saved:
                saved.add(lock, arcname="Cargo.lock")
            runtime.verify_source_archive(root, archive)
            (root / ".nosh").mkdir()
            (root / ".nosh" / "generated").write_text("separately verified dependency\n")
            runtime.verify_source_archive(root, archive)
            lock.write_text("silently re-resolved dependency graph\n")
            with self.assertRaisesRegex(ValueError, "Cargo.lock"):
                runtime.verify_source_archive(root, archive)


class CampaignCompletenessTests(unittest.TestCase):
    def data(self):
        declared = {
            "schema_version": 2, "dataset_revision": 4, "seeds": [0, 1],
            "scenarios": [{"id": "example", "check": "largest"}],
        }
        data = {
            "schema_version": 2,
            "metadata": {"scenarios": declared["scenarios"], "seeds": declared["seeds"],
                         "repeat": 2, "dataset_revision": 4, "observation": "native-v1"},
            "trials": [{
                "scenario_id": "example", "seed": seed, "repeat": repeat, "status": "fail",
                "metrics": {name: None for name in report.METRICS}, "answer": "", "final_state": {},
            } for seed in declared["seeds"] for repeat in range(2)],
        }
        return declared, data

    def test_complete_model_failures_are_kept_but_missing_trials_are_not_complete(self):
        declared, data = self.data()
        self.assertTrue(campaign.is_complete(data, declared))
        missing = copy.deepcopy(data)
        missing["trials"].pop()
        self.assertFalse(campaign.is_complete(missing, declared))
        error = copy.deepcopy(data)
        error["trials"][0]["status"] = "error"
        self.assertFalse(campaign.is_complete(error, declared))
        changed = copy.deepcopy(data)
        changed["metadata"]["dataset_revision"] = 3
        self.assertFalse(campaign.is_complete(changed, declared))
        duplicate = copy.deepcopy(data)
        duplicate["trials"].append(duplicate["trials"][0])
        with self.assertRaisesRegex(ValueError, "duplicate trial"):
            campaign.is_complete(duplicate, declared)

    def test_workflow_uses_the_tested_guards_instead_of_stale_counts(self):
        workflow = runtime.ROOT.joinpath(".github", "workflows", "eval.yml").read_text()
        self.assertIn("resolve_source_revision", workflow)
        self.assertIn("campaign.is_complete(data", workflow)
        self.assertIn("EVAL_SOURCE_REF", workflow)
        self.assertNotIn('len(data["trials"]) == 250', workflow)
        self.assertIn("EVAL_TRIAL_TIMEOUT_S: '60'", workflow)
