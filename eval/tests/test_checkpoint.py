import json
from pathlib import Path
import tempfile
import unittest

from eval import checkpoint, report


class CheckpointTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.source = self.root / "live"
        self.campaign = self.source / "campaign"
        self.logs = self.campaign / "logs" / "example-0-0"
        self.logs.mkdir(parents=True)
        (self.logs / "transcript.txt").write_text("immutable trial output\n")
        (self.source / "build-info.json").write_text("{}\n")
        (self.source / "environment.txt").write_text("test environment\n")
        self.data = {
            "schema_version": 2,
            "metadata": {"run_id": "checkpoint-test", "observation": "native-v1",
                         "build": {"binary_sha256": "a" * 64}, "scenarios": [{"id": "example"}],
                         "seeds": [0, 1], "repeat": 1},
            "trials": [{"scenario_id": "example", "seed": 0, "repeat": 0, "status": "fail",
                        "metrics": {"steps": 1, "confirmations": 0, "ttft_s": None, "total_s": 1, "peak_rss_mib": 1},
                        "answer": "recorded answer", "final_state": {}, "grading": None,
                        "logs": "logs/example-0-0"}],
        }
        report.save(self.data, self.campaign)

    def tearDown(self):
        self.temporary.cleanup()

    def test_copy_keeps_raw_report_and_closed_logs(self):
        original = (self.campaign / "report.json").read_bytes()
        (self.campaign / "report.md").write_text("an older live Markdown file\n")
        destination = self.root / "checkpoint"
        info = checkpoint.snapshot(self.source, destination)
        self.assertEqual(info["trials"], 1)
        self.assertEqual(info["counts"], {"pass": 0, "fail": 1, "error": 0})
        self.assertTrue(info["checkpoint_only"])
        self.assertEqual((destination / "campaign" / "report.json").read_bytes(), original)
        self.assertEqual((self.campaign / "report.json").read_bytes(), original)
        self.assertEqual((destination / "campaign" / "report.md").read_text(encoding="utf-8"), report.markdown(self.data))
        self.assertEqual((destination / "campaign" / "logs" / "example-0-0" / "transcript.txt").read_text(),
                         "immutable trial output\n")
        with self.assertRaises(FileExistsError):
            checkpoint.snapshot(self.source, destination)

    def test_overlaps_and_escaping_log_paths_are_rejected(self):
        with self.assertRaises(ValueError):
            checkpoint.snapshot(self.source, self.campaign / "snapshot")
        self.data["trials"][0]["logs"] = "../outside"
        (self.campaign / "report.json").write_text(json.dumps(self.data))
        with self.assertRaisesRegex(ValueError, "log location"):
            checkpoint.snapshot(self.source, self.root / "checkpoint")

    def test_invalid_atomic_report_is_not_a_successful_checkpoint(self):
        (self.campaign / "report.json").write_text("{partial")
        with self.assertRaises(ValueError):
            checkpoint.snapshot(self.source, self.root / "checkpoint")
        self.assertFalse((self.root / "checkpoint").exists())
