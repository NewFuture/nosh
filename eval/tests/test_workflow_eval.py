import copy
from pathlib import Path
import subprocess
import tempfile
import unittest

from eval import fixtures, report, run


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
        self.assertEqual(run.resolve_source_revision(cwd=self.checkout), self.main)
        self.assertEqual(run.resolve_source_revision("feature/capture", cwd=self.checkout), self.feature)
        self.assertEqual(run.resolve_source_revision("feature/capture", self.main, self.checkout), self.main)
        with self.assertRaisesRegex(ValueError, "reachable"):
            run.resolve_source_revision("main", self.feature, self.checkout)

    def test_invalid_refs_and_revision_expressions_are_not_executed(self):
        for ref in ("", "--upload-pack=anything", "main\nother", "main:other"):
            with self.subTest(ref=ref), self.assertRaises(ValueError):
                run.resolve_source_revision(ref, cwd=self.checkout)
        for revision in ("HEAD", "main~1", "-bad", "A" * 40, "a" * 39):
            with self.subTest(revision=revision), self.assertRaises(ValueError):
                run.resolve_source_revision("main", revision, self.checkout)


class CampaignCompletenessTests(unittest.TestCase):
    def data(self):
        declared = {
            "schema_version": 2, "dataset_revision": 4, "seeds": [0, 1],
            "scenarios": [{"id": "example", "check": "largest"}],
        }
        data = {
            "schema_version": 2,
            "metadata": {"scenarios": declared["scenarios"], "seeds": declared["seeds"],
                         "repeat": 2, "dataset_revision": 4},
            "trials": [{
                "scenario_id": "example", "seed": seed, "repeat": repeat, "status": "fail",
                "metrics": {name: None for name in report.METRICS}, "answer": "", "final_state": {},
            } for seed in declared["seeds"] for repeat in range(2)],
        }
        return declared, data

    def test_complete_model_failures_are_kept_but_missing_trials_are_not_complete(self):
        declared, data = self.data()
        self.assertTrue(run.campaign_complete(data, declared))
        missing = copy.deepcopy(data)
        missing["trials"].pop()
        self.assertFalse(run.campaign_complete(missing, declared))
        error = copy.deepcopy(data)
        error["trials"][0]["status"] = "error"
        self.assertFalse(run.campaign_complete(error, declared))
        changed = copy.deepcopy(data)
        changed["metadata"]["dataset_revision"] = 3
        self.assertFalse(run.campaign_complete(changed, declared))
        duplicate = copy.deepcopy(data)
        duplicate["trials"].append(duplicate["trials"][0])
        with self.assertRaisesRegex(ValueError, "duplicate trial"):
            run.campaign_complete(duplicate, declared)

    def test_workflow_uses_the_tested_guards_instead_of_stale_counts(self):
        workflow = run.ROOT.joinpath(".github", "workflows", "eval.yml").read_text()
        self.assertIn("resolve_source_revision", workflow)
        self.assertIn("run.campaign_complete(data", workflow)
        self.assertIn("EVAL_SOURCE_REF", workflow)
        self.assertNotIn('len(data["trials"]) == 250', workflow)
        self.assertIn("EVAL_TRIAL_TIMEOUT_S: '60'", workflow)
