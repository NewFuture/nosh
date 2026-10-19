"""Model-free reference-runtime provenance regressions."""

import importlib.util
from pathlib import Path
import tempfile
from types import SimpleNamespace
import unittest
from unittest.mock import Mock, patch
import zipfile


SOURCE = Path(__file__).resolve().parents[2] / "tools" / "qwen35-reference.py"
SPEC = importlib.util.spec_from_file_location("nosh_qwen_reference", SOURCE)
REFERENCE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(REFERENCE)


class ReferenceLibrariesTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        root = Path(self.temporary.name)
        self.directory = root / "bin"
        self.directory.mkdir()
        self.archive = root / "reference.zip"
        self.payload = {
            "llama.dll": b"pinned llama",
            "ggml.dll": b"pinned ggml",
            "ggml-base.dll": b"pinned base",
            "ggml-cpu-test.dll": b"pinned CPU backend",
            "libomp.dll": b"pinned dependency",
        }
        with zipfile.ZipFile(self.archive, "w") as distribution:
            for name, data in self.payload.items():
                distribution.writestr(name, data)
                (self.directory / name).write_bytes(data)
        self.archive_sha256 = REFERENCE.digest(self.archive)
        pin = patch.object(REFERENCE, "REFERENCE_ARCHIVE_SHA256", self.archive_sha256)
        pin.start()
        self.addCleanup(pin.stop)

    def verify(self):
        return REFERENCE.verify_libraries(self.directory, self.archive)

    def test_records_the_verified_archive_and_complete_dll_set(self):
        provenance = self.verify()
        self.assertEqual(provenance["release_archive_sha256"], self.archive_sha256)
        self.assertEqual(provenance["reference_verification"], REFERENCE.REFERENCE_VERIFICATION)
        self.assertEqual(provenance["computation_libraries_sha256"], {
            name: REFERENCE.digest(self.directory / name) for name in self.payload
        })

    def test_rejects_an_unpinned_archive(self):
        self.archive.write_bytes(self.archive.read_bytes() + b"changed")
        with self.assertRaisesRegex(ValueError, "not the pinned"):
            self.verify()

    def test_rejects_modified_backends_and_dependencies_with_an_intact_llama(self):
        for name in ("ggml.dll", "ggml-base.dll", "ggml-cpu-test.dll", "libomp.dll"):
            with self.subTest(library=name):
                path = self.directory / name
                path.write_bytes(b"compatible but modified")
                with self.assertRaisesRegex(ValueError, "computation library mismatch"):
                    self.verify()
                path.write_bytes(self.payload[name])

    def test_rejects_missing_or_extra_dlls(self):
        backend = self.directory / "ggml-cpu-test.dll"
        backend.unlink()
        with self.assertRaisesRegex(ValueError, "DLL set mismatch; missing"):
            self.verify()
        backend.write_bytes(self.payload[backend.name])
        (self.directory / "unexpected.DLL").write_bytes(b"unverified backend")
        with self.assertRaisesRegex(ValueError, "unexpected.*unexpected.dll"):
            self.verify()

    def test_verification_failure_precedes_any_library_loading(self):
        args = SimpleNamespace(bin=self.directory, archive=self.archive, model=Path("model.gguf"))
        system = SimpleNamespace(name="nt", add_dll_directory=Mock())
        with (
            patch.object(REFERENCE, "os", system),
            patch.object(REFERENCE, "digest", return_value=
                         "37ae482d336108d23516fa35e8e0c4126688d81018b87178a18d752a1357814f"),
            patch.object(REFERENCE, "verify_libraries", side_effect=ValueError("unverified SDK")),
            patch.object(REFERENCE.c, "CDLL") as load,
        ):
            with self.assertRaisesRegex(ValueError, "unverified SDK"):
                REFERENCE.run(args)
            system.add_dll_directory.assert_not_called()
            load.assert_not_called()
