"""Integrity of retained read-only evidence, not compatibility with its old grader."""

import hashlib
import importlib.util
import json
from pathlib import Path
import stat
import tempfile
import unittest
from unittest.mock import patch
import zipfile

from eval import runtime

BASELINE = runtime.HERE / "baselines" / "main-78b7e50-expanded"
SPEC = importlib.util.spec_from_file_location("baseline_archive", BASELINE / "reproduce.py")
ARCHIVE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(ARCHIVE)


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
