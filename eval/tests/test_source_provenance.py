import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

from eval import runtime


class SourceProvenanceTests(unittest.TestCase):
    def test_both_managed_dependencies_are_validated(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "Cargo.toml").write_text('[workspace.dependencies]\nreedline={path=".nosh/reedline"}\n')
            tool = root / "tools" / "source" / "Cargo.toml"
            tool.parent.mkdir(parents=True)
            tool.write_text("[package]\n")
            states = {}
            for name, repository in (("reedline", "editor"), ("brush-core", "shell")):
                pin = root / "patches" / name / "source.toml"
                pin.parent.mkdir(parents=True)
                pin.write_text(f'repository="{repository}"\nrevision="{"a" * 40}"\n')
                states[name] = {
                    "schema_version": 1,
                    "upstream_repository": repository,
                    "upstream_revision": "a" * 40,
                    "upstream_tree": "b" * 40,
                    "prepared_tree": "c" * 40,
                    "patch_sha256": "d" * 64,
                    "source_pin_sha256": "e" * 64,
                    "prepared_archive_sha256": "f" * 64,
                }
            with mock.patch.object(runtime.subprocess, "check_output", return_value=json.dumps(states)):
                self.assertEqual(runtime.source_dependency_provenance(root)["managed_sources"], states)
            with mock.patch.object(runtime.subprocess, "check_output",
                                   return_value=json.dumps({"reedline": states["reedline"]})):
                with self.assertRaisesRegex(ValueError, "incomplete"):
                    runtime.source_dependency_provenance(root)
            states["brush-core"]["patch_sha256"] = 42
            with mock.patch.object(runtime.subprocess, "check_output", return_value=json.dumps(states)):
                with self.assertRaisesRegex(ValueError, "invalid"):
                    runtime.source_dependency_provenance(root)
