import contextlib
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from eval import kv_assets as assets
from eval import kv_cache as kv


def sample(prefill=100, decode=100, count=128):
    return {
        "tokens": list(range(count)),
        "final": {"timings": {"prompt_per_second": prefill, "predicted_per_second": decode,
                             "prompt_n": 8062, "predicted_n": count, "cache_n": 0}},
    }


class KVCacheTests(unittest.TestCase):
    def test_manifest_preserves_ten_weights_and_six_models(self):
        models = assets.manifest()["models"]
        self.assertEqual(len(models), 10)
        self.assertEqual(len({m["model_id"] for m in models}), 6)
        self.assertEqual(sum(m["quant"] == "Q4_0_PURE" for m in models), 4)
        self.assertTrue(all(m["bytes"] > 0 for m in models))

    def test_followup_scope_is_explicit_and_validated(self):
        self.assertEqual(len(assets.select_models()), 10)
        self.assertEqual([m["id"] for m in assets.select_models("gemma270-pure,shellcue-q4km")],
                         ["shellcue-q4km", "gemma270-pure"])
        for cases in ("", "unknown", "gemma270-pure,gemma270-pure", "all,gemma270-pure"):
            with self.assertRaises(ValueError):
                assets.select_models(cases)

    def test_five_percent_is_strict_and_both_metrics_are_required(self):
        for prefill, decode, expected in ((95, 100, False), (100, 95, False),
                                          (95.00001, 96, True), (94, 120, False)):
            row = kv.compare([sample()] * 3, [sample(prefill, decode)] * 3)
            self.assertEqual(row["passes_both"], expected)

    def test_uses_medians_and_rejects_invalid_samples(self):
        row = kv.compare([sample()] * 3, [sample(90), sample(91), sample(200)])
        self.assertEqual(row["prefill"]["q8_tok_s"], 91)
        for value in (float("nan"), float("inf"), 0, -1):
            with self.assertRaises(ValueError):
                kv.compare([sample()], [sample(value)])

    def test_cpu_selection_excludes_smt_siblings_and_reserves_controller(self):
        topology = {cpu: {"core": [0, cpu // 2], "siblings": [cpu // 2 * 2, cpu // 2 * 2 + 1]}
                    for cpu in range(6)}
        plan = kv.choose_cpus(topology, {cpu: 0 for cpu in topology})
        self.assertEqual(len({tuple(topology[n]["core"]) for n in plan["cpus"]}), 2)
        self.assertFalse(set(plan["controller_cpus"]) & set(plan["cpus"] + plan["siblings"]))
        with self.assertRaises(ValueError):
            kv.choose_cpus({0: topology[0], 1: topology[1]}, {0: 0, 1: 0})
        with self.assertRaises(ValueError):
            kv.choose_cpus({0: {"core": [0, 0], "siblings": [0]},
                            1: {"core": [0, 1], "siblings": [1]}}, {0: 0, 1: 0})

    def test_affinity_subsets_are_valid_but_escape_is_not(self):
        kv.validate_masks({1: [4], 2: [6], 3: [4, 6]}, [4, 6])
        for masks in ({}, {1: []}, {1: [4, 5]}):
            with self.assertRaises(ValueError):
                kv.validate_masks(masks, [4, 6])
        self.assertEqual(kv.cpu_list("0-2,4"), {0, 1, 2, 4})
        with self.assertRaises(ValueError):
            kv.cpu_list("2-1")

    def test_core_contention_is_not_diluted_by_idle_machine(self):
        before = {"cpu": (4000, 2000), "cpu4": (1000, 500),
                  "cpu6": (1000, 500), "cpu5": (1000, 900)}
        after = {"cpu": (4400, 2200), "cpu4": (1100, 500),
                 "cpu6": (1100, 500), "cpu5": (1100, 1000)}
        values = kv.contention(before, after, [4, 6], [5], 160)
        self.assertEqual(values["global_external"], 0.1)
        self.assertEqual(values["target_external"], 0.2)
        self.assertEqual(values["sibling_busy"], 0)
        with self.assertRaises(ValueError):
            kv.busy((100, 50), (100, 50))

    def test_commands_differ_only_in_kv_precision(self):
        model = {"path": "weights.gguf", "model_id": "model", "quant": "Q4_K_M"}
        plan = {"cpus": [4, 6]}
        f16 = kv.command(Path("llama-server"), model, plan, "f16", 9000)
        q8 = kv.command(Path("llama-server"), model, plan, "q8_0", 9000)
        self.assertEqual([(a, b) for a, b in zip(f16, q8) if a != b],
                         [("f16", "q8_0"), ("f16", "q8_0")])
        for flag, value in (("-t", "2"), ("-tb", "2"), ("-c", "8192"), ("-b", "512"),
                            ("-ub", "128"), ("-ngl", "0"), ("--cpu-mask", "0x50"),
                            ("--cpu-mask-batch", "0x50"), ("--cpu-strict", "1")):
            self.assertEqual(f16[f16.index(flag) + 1], value)
        self.assertNotIn("--no-repack", f16)

    def test_actual_token_counts_and_cache_are_validated(self):
        kv.validate_sample(sample(), 8062)
        for field in ("predicted_n", "prompt_n"):
            bad = sample()
            bad["final"]["timings"][field] += 1
            with self.assertRaises(ValueError):
                kv.validate_sample(bad, 8062)
        bad = sample()
        bad["final"]["timings"]["cache_n"] = 1
        with self.assertRaises(ValueError):
            kv.validate_sample(bad, 8062)
        bad = sample()
        bad["final"]["truncated"] = True
        with self.assertRaises(ValueError):
            kv.validate_sample(bad, 8062)

    def rows(self):
        result = []
        for index, order in enumerate(kv.ORDERS, 1):
            for cache in order:
                tick = len(result)
                result.append({
                    "status": "ok", "round": index, "kv": cache,
                    "started_utc": f"2026-09-26T10:0{tick}:00Z",
                    "finished_utc": f"2026-09-26T10:0{tick}:59Z",
                    "resources": {"peak_rss_bytes": 400000000},
                    "samples": [sample(100 if cache == "f16" else 96)] * 3,
                })
        return result

    def test_partial_or_failed_results_cannot_be_complete(self):
        model = assets.manifest()["models"][0]
        rows = self.rows()
        result = kv.summary(model, rows)
        self.assertTrue(result["complete"])
        self.assertEqual(result["throughput_samples"], 12)
        self.assertTrue(result["comparison"]["passes_both"])
        self.assertTrue(result["passes_each_round"])
        self.assertFalse(kv.summary(model, rows[:3])["complete"])
        rows[0]["status"] = "failed"
        self.assertFalse(kv.summary(model, rows)["complete"])

    def test_one_good_round_does_not_hide_second_round_loss(self):
        rows = self.rows()
        rows[2]["samples"] = [sample(94)] * 3
        result = kv.summary(assets.manifest()["models"][0], rows)
        self.assertFalse(result["passes_each_round"])
        self.assertTrue(result["rounds"][0]["passes_both"])
        self.assertFalse(result["rounds"][1]["passes_both"])
        rows[2]["started_utc"] = rows[0]["started_utc"]
        with self.assertRaises(ValueError):
            kv.summary(assets.manifest()["models"][0], rows)

    def test_downloads_require_checksum_and_safe_paths(self):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "model"
            path.write_bytes(b"test")
            expected = {"bytes": 4, "sha256": hashlib.sha256(b"test").hexdigest()}
            assets.verify(path, expected)
            with self.assertRaises(ValueError):
                assets.verify(path, {"bytes": 4})
            with self.assertRaises(ValueError):
                assets.verify(path, {**expected, "bytes": 5})
            for bad in ("../model", "/model", "folder\\model"):
                with self.assertRaises(ValueError):
                    assets.source_path(Path(temporary), bad)

    def test_separate_quantizer_preserves_strict_output_verification(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            source = root / "source.gguf"
            quantizer = root / "quantizer"
            source.write_bytes(b"source")
            quantizer.write_bytes(b"executable")
            model = {
                "id": "test-pure", "model_id": "test", "revision": "a" * 40,
                "kind": "quantize_gguf", "quant": "Q4_0_PURE", "source": {},
                "bytes": 4, "sha256": hashlib.sha256(b"data").hexdigest(),
            }
            commands = []

            def execute(command, log):
                commands.append(command)
                Path(command[-3]).write_bytes(b"data")

            with (patch.object(assets, "download", return_value={"path": str(source)}),
                  patch.object(assets, "execute", side_effect=execute)):
                receipt = assets.prepare(model, root, quantizer)
                self.assertTrue(receipt["verified"])
                self.assertEqual(commands[0][0], quantizer)
                self.assertEqual(commands[0][1], "--pure")
                self.assertEqual(receipt["sha256"], model["sha256"])
                with self.assertRaises(ValueError):
                    assets.prepare({**model, "sha256": "0" * 64}, root, quantizer)
            recorded = json.loads((root / "results" / "model-output.json").read_text())
            self.assertEqual(recorded["actual_sha256"], model["sha256"])
            self.assertEqual(recorded["expected_sha256"], "0" * 64)

    def test_stream_persists_the_actual_request_and_final_event(self):
        final = sample(count=1)["final"]
        final["stop"] = True
        events = [{"tokens": [0]}, final]
        data = b"".join(b"data: " + json.dumps(e).encode() + b"\n" for e in events)
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "request.json"
            request = kv.payload("prompt", 1)
            with patch.object(kv.HTTP, "open", return_value=io.BytesIO(data)):
                result = kv.stream("http://127.0.0.1", request, path)
            saved = json.loads(path.read_text())
            self.assertEqual(saved["request"], request)
            self.assertEqual(saved["events"][-1]["data"], result["final"])
            self.assertEqual(result["tokens"], [0])
            with patch.object(kv.HTTP, "open", return_value=io.BytesIO(b"")):
                with self.assertRaises(ValueError):
                    kv.stream("http://127.0.0.1", request, path)

    def test_report_keeps_missing_cases_in_the_denominator(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            model = assets.manifest()["models"][0]
            assets.save(root / model["id"] / "summary.json", kv.summary(model, self.rows()))
            with (patch.dict("os.environ", {}, clear=True),
                  patch.object(kv, "audit_case"),
                  contextlib.redirect_stdout(io.StringIO())):
                self.assertFalse(kv.report(root))
            report = json.loads((root / "report.json").read_text())
            self.assertEqual(len(report["missing"]), 9)
            self.assertIn("MISSING", (root / "report.md").read_text())

    def test_unauditable_result_is_not_reported_as_complete(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            model = assets.manifest()["models"][0]
            assets.save(root / model["id"] / "summary.json", kv.summary(model, self.rows()))
            with patch.dict("os.environ", {}, clear=True), contextlib.redirect_stdout(io.StringIO()):
                self.assertFalse(kv.report(root))
            row = json.loads((root / "report.json").read_text())["cases"][0]
            self.assertFalse(row["complete"])
            self.assertIn("audit_error", row)

    def test_selected_report_does_not_claim_all_ten_cases(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            model = assets.manifest()["models"][0]
            assets.save(root / model["id"] / "summary.json", kv.summary(model, self.rows()))
            with (patch.dict("os.environ", {}, clear=True),
                  patch.object(kv, "audit_case"),
                  contextlib.redirect_stdout(io.StringIO())):
                self.assertTrue(kv.report(root, model["id"]))
            report = json.loads((root / "report.json").read_text())
            self.assertEqual(report["expected_cases"], [model["id"]])


if __name__ == "__main__":
    unittest.main()
