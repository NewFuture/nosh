import hashlib
import importlib.util
import json
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from eval import run

BASELINE = run.HERE / "baselines" / "main-78b7e50-expanded"
SPEC = importlib.util.spec_from_file_location("baseline_archive", BASELINE / "reproduce.py")
ARCHIVE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ARCHIVE)


class ExpandedBaselineTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.data = json.loads((BASELINE / "summary.json").read_text(encoding="utf-8"))
        cls.provenance = json.loads((BASELINE / "provenance.json").read_text(encoding="utf-8"))
        cls.index = json.loads((BASELINE / "archive.json").read_text(encoding="utf-8"))

    def test_compact_counts_and_weighted_metrics(self):
        data = self.data
        self.assertEqual(data["kind"], "evaluation-baseline-summary")
        scenarios = {s["id"]: s for s in run.load_suite(run.HERE / "scenarios.json")["scenarios"]}
        self.assertEqual({row["scenario_id"] for row in data["scenarios"]}, set(scenarios))
        self.assertEqual(data["seeds"], [0, 1, 2, 3, 4])
        self.assertEqual(data["repeat"], 2)
        for group in data["groups"]:
            rows = [row for row in data["scenarios"] if (
                group["group"] == "all"
                or group["group"] == scenarios[row["scenario_id"]]["group"]
                or group["group"] == ("local" if scenarios[row["scenario_id"]]["check"] == "typos" else "model")
            )]
            for key in ("planned", "pass", "fail", "error", "missing", "steps_samples", "confirmations_samples"):
                self.assertEqual(group[key], sum(row[key] for row in rows), (group["group"], key))
            for metric in ("steps", "confirmations"):
                weighted = sum(row[metric] * row[metric + "_samples"] for row in rows)
                self.assertAlmostEqual(group[metric], weighted / group[metric + "_samples"])
        overall = data["groups"][0]
        self.assertEqual([overall[k] for k in ("planned", "pass", "fail", "error", "missing")], [250, 120, 130, 0, 0])
        self.assertEqual((overall["steps"], overall["confirmations"]), (4.88, 1.028))
        self.assertEqual(data["raw_counts"], {"pass": 120, "fail": 129, "error": 1})
        self.assertEqual(data["diagnostic"]["pass"], 107)
        self.assertEqual(data["reproducibility"]["verdict_and_state_consistent"], 107)
        self.assertEqual(data["reproducibility"]["state_consistent"], 119)

    def test_sources_and_external_archive_remain_explicit(self):
        build = json.loads((BASELINE / "build-info.json").read_text(encoding="utf-8"))
        self.assertEqual(self.data["source_revision"], build["source_revision"])
        self.assertEqual(self.data["source_revision"], self.provenance["source_revision"])
        self.assertEqual(self.data["harness_revision"], self.provenance["runtime_harness_revision"])
        self.assertEqual(self.data["normalization_revision"], self.provenance["normalization_revision"])
        self.assertEqual(self.provenance["raw_trial_payload_sha256"], self.provenance["restored_trial_payload_sha256"])
        self.assertEqual(self.index["storage"], "github-release-asset")
        self.assertTrue(self.index["issue_comment_url"].startswith("https://github.com/NewFuture/nosh/issues/4#"))
        self.assertTrue(self.index["url"].startswith("https://github.com/NewFuture/nosh/releases/download/"))
        self.assertEqual(self.index["file_count"], 22)
        for key in ("sha256", "manifest_sha256"):
            self.assertRegex(self.index[key], r"^[0-9a-f]{64}$")
        self.assertGreater(self.index["bytes"], 0)
        self.assertLess(self.index["bytes"], self.index["uncompressed_bytes"])
        for name in ("report.json", "raw-report.json", "diagnostic-report.json"):
            self.assertFalse((BASELINE / name).exists(), "full reports belong in the external archive")

    def test_readable_summary_matches_machine_readable_metrics(self):
        rows = {}
        for line in (BASELINE / "report.md").read_text(encoding="utf-8").splitlines():
            if line.startswith("|"):
                cells = [cell.strip() for cell in line.strip("|").split("|")]
                rows[cells[0]] = cells[1:]
        for scenario in self.data["scenarios"]:
            cells = rows[scenario["scenario_id"]]
            self.assertEqual([int(part) for part in cells[0].split("/")], [scenario["pass"], scenario["planned"]])
            self.assertAlmostEqual(float(cells[1]), scenario["steps"])
            self.assertAlmostEqual(float(cells[2]), scenario["confirmations"])


class ArchiveVerificationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)

    def tearDown(self):
        self.temporary.cleanup()

    def package(self, files, expected=None, symlinks=()):
        manifest = json.dumps({"schema_version": 1, "files": {
            name: {"bytes": len(content), "sha256": hashlib.sha256(content).hexdigest()}
            for name, content in (expected if expected is not None else files).items()
        }}).encode()
        path = self.root / "test.zip"
        with zipfile.ZipFile(path, "w") as archive:
            for name, content in dict(files, **{"manifest.json": manifest}).items():
                info = zipfile.ZipInfo(name)
                info.external_attr = (stat.S_IFLNK | 0o777) << 16 if name in symlinks else (stat.S_IFREG | 0o644) << 16
                archive.writestr(info, content)
        index = {
            "bytes": path.stat().st_size, "sha256": ARCHIVE.digest(path),
            "manifest_sha256": hashlib.sha256(manifest).hexdigest(),
            "file_count": len(files) + 1,
            "uncompressed_bytes": sum(map(len, files.values())) + len(manifest),
        }
        return path, index

    def test_explicit_local_archive_is_verified_without_network(self):
        path, index = self.package({"summary.json": b"{}", "verify.py": b"print('verified')"})
        with patch.object(ARCHIVE.subprocess, "run") as execute:
            ARCHIVE.verify(path, index, {})
        execute.assert_called_once()
        args = execute.call_args.args[0]
        self.assertEqual(args[1:3], ["-I", "-B"])
        self.assertEqual(Path(args[3]).name, "verify.py")
        with patch.object(ARCHIVE.subprocess, "run") as execute, self.assertRaisesRegex(ValueError, "summary"):
            ARCHIVE.verify(path, index, {"changed": True})
        execute.assert_not_called()

    def test_wrong_zip_or_file_hash_is_rejected_before_execution(self):
        path, index = self.package({"summary.json": b"{}", "verify.py": b"old"}, expected={"summary.json": b"{}", "verify.py": b"new"})
        with patch.object(ARCHIVE.subprocess, "run") as execute:
            with self.assertRaisesRegex(ValueError, "archived file changed"):
                ARCHIVE.verify(path, index, {})
            with self.assertRaisesRegex(ValueError, "archive size or SHA-256"):
                ARCHIVE.verify(path, dict(index, sha256="0" * 64), {})
            with self.assertRaisesRegex(ValueError, "manifest SHA-256"):
                ARCHIVE.verify(path, dict(index, manifest_sha256="0" * 64), {})
        execute.assert_not_called()

    def test_escaping_paths_symlinks_and_extra_files_are_rejected(self):
        for name in ("../outside", "/outside", "C:/outside", "dir\\outside", "dir/../outside"):
            with self.subTest(name=name):
                path, index = self.package({name: b"bad"})
                with self.assertRaisesRegex(ValueError, "unsafe archived path"):
                    ARCHIVE.verify(path, index, {})
        path, index = self.package({"link": b"outside"}, symlinks={"link"})
        with self.assertRaisesRegex(ValueError, "unsafe archived path"):
            ARCHIVE.verify(path, index, {})
        path, index = self.package({"extra": b"bad"}, expected={})
        with self.assertRaisesRegex(ValueError, "archive file set"):
            ARCHIVE.verify(path, index, {})
