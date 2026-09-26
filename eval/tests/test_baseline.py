import copy
import json
import unittest

from eval import driver, fixtures, report, run


class ExpandedBaselineTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.root = run.HERE / "baselines" / "main-78b7e50-expanded"
        cls.data = json.loads((cls.root / "report.json").read_text(encoding="utf-8"))
        cls.raw = json.loads((cls.root / "raw-report.json").read_text(encoding="utf-8"))
        cls.provenance = json.loads((cls.root / "provenance.json").read_text(encoding="utf-8"))

    def test_all_identities_metrics_and_sources_are_preserved(self):
        data = self.data
        report.validate(data)
        report.validate(self.raw)
        expected = {(s["id"], seed, repeat) for s in data["metadata"]["scenarios"]
                    for seed in range(5) for repeat in range(2)}
        self.assertEqual(len(expected), 250)
        self.assertEqual({report.trial_key(t) for t in data["trials"]}, expected)
        groups = {g["group"]: g for g in report.groups(data)}
        self.assertEqual((groups["all"]["pass"], groups["all"]["fail"], groups["all"]["error"], groups["all"]["missing"]),
                         (120, 130, 0, 0))
        self.assertEqual(groups["all"]["steps"], 4.88)
        self.assertEqual(groups["all"]["confirmations"], 1.028)
        self.assertEqual(groups["all"]["steps_samples"], 250)
        self.assertEqual(groups["model"]["pass"], 110)
        self.assertEqual(groups["model"]["planned"], 240)
        self.assertEqual(groups["local"]["pass"], 10)
        self.assertEqual(data["summary"], report.aggregate(data))
        self.assertEqual(data["groups"], report.groups(data))
        self.assertEqual(data["reproducibility"], report.repetitions(data["trials"]))
        build = json.loads((self.root / "build-info.json").read_text(encoding="utf-8"))
        self.assertEqual(data["metadata"]["build"]["binary_sha256"], build["binary_sha256"])
        self.assertEqual(build["source_revision"], "78b7e509ad0d6d71ce50397cfa9e9f2187b0db75")
        for name, expected_hash in self.provenance["derived_report_hashes"].items():
            self.assertEqual(fixtures.file_hash(self.root / name), expected_hash)

    def test_original_trial_payload_is_losslessly_recoverable(self):
        restored = []
        for row in self.data["trials"]:
            if "original_trial" in row:
                restored.append(row["original_trial"])
            else:
                item = copy.deepcopy(row)
                if "original_judgment" in item:
                    item.update(item.pop("original_judgment"))
                restored.append(item)
        self.assertEqual(restored, self.raw["trials"])
        self.assertEqual(fixtures.digest(restored), self.provenance["raw_trial_payload_sha256"])
        self.assertEqual(self.provenance["raw_trial_payload_sha256"],
                         self.provenance["restored_trial_payload_sha256"])
        workflow = self.provenance["raw_workflow_provenance"]
        self.assertFalse(workflow["complete"])
        self.assertEqual(fixtures.file_hash(self.root / "raw-report.json"),
                         workflow["attributed_report_hashes"]["report.json"])
        diagnostic = json.loads((self.root / "diagnostic-report.json").read_text(encoding="utf-8"))
        report.validate(diagnostic)
        self.assertEqual(len(diagnostic["trials"]), 250)
        self.assertEqual(fixtures.file_hash(self.root / "diagnostic-report.json"),
                         self.provenance["prior_attempts"][1]["report_sha256"])

    def test_timeout_recovery_uses_recorded_trace_not_a_replacement_trial(self):
        row = next(t for t in self.data["trials"] if "original_trial" in t)
        original = row["original_trial"]
        scenario = next(s for s in self.data["metadata"]["scenarios"] if s["id"] == row["scenario_id"])
        result = driver.Result(
            exit_code=original["exit_code"], total_s=original["metrics"]["total_s"],
            peak_rss_mib=original["metrics"]["peak_rss_mib"],
            approvals=original["approvals"], turns=original["turns"],
            error=original["reasons"][0], timeout_phase="agent",
            transcript=(self.root / "timeout-transcript.txt").read_text(encoding="utf-8"),
        )
        recovered = run.observe(result, scenario, self.root / "timeout-engine.jsonl",
                                False, row["seed"], inflight_timeout=True)
        self.assertEqual(recovered["metrics"], row["metrics"])
        self.assertEqual(recovered["inputs"], row["inputs"])
        self.assertEqual(recovered["tool_calls"], row["tool_calls"])
        self.assertEqual(recovered["executions"], row["executions"])
        self.assertEqual(row["status"], "fail")
        self.assertEqual(row["answer"], "")
        self.assertEqual(row["metrics"]["steps"], 7)
        self.assertEqual(row["metrics"]["task_status"], "timed_out")
        self.assertEqual(row["final_state"], original["final_state"])
        for name, expected_hash in self.provenance["timeout_evidence_sha256"].items():
            self.assertEqual(fixtures.file_hash(self.root / name), expected_hash)

    def test_closing_correction_changes_no_success_count(self):
        changed = next(t for t in self.data["trials"] if "original_judgment" in t)
        self.assertEqual(report.trial_key(changed), ("zh-rust-test", 0, 0))
        self.assertEqual(changed["status"], changed["original_judgment"]["status"])
        self.assertEqual(changed["status"], "fail")
        self.assertFalse(changed["original_judgment"]["grading"]["experience"]["final_question"]["passed"])
        self.assertTrue(changed["grading"]["experience"]["final_question"]["passed"])
