"""Explicit devices survive trial isolation and must match native observations."""

import json
import os
from pathlib import Path
import tempfile
import tomllib
import unittest
from unittest.mock import patch

from eval import campaign, driver, observations, runtime


class DeviceTests(unittest.TestCase):
    def test_selection_is_explicit_and_cpu_is_default(self):
        self.assertEqual(campaign.arguments([]).device, "cpu")
        self.assertEqual(campaign.arguments(["--device", "cuda"]).device, "cuda:0")
        self.assertEqual(runtime.inference_device("auto"), "cpu")
        self.assertEqual(runtime.inference_device("cuda:01"), "cuda:1")
        for value in ("", "metal", "cuda:-1", "cuda:+1", "cuda:", "cuda:2147483648"):
            with self.assertRaises(ValueError):
                runtime.inference_device(value)

    def test_isolated_config_and_gpu_visibility(self):
        with tempfile.TemporaryDirectory() as temporary, patch.dict(os.environ, {
            "CUDA_VISIBLE_DEVICES": "1", "CUDA_DEVICE_ORDER": "PCI_BUS_ID",
            "LD_LIBRARY_PATH": "/cuda/lib64", "NOSH_DEVICE": "cuda",
        }):
            for device in ("cpu", "cuda", "cuda:1"):
                home = Path(temporary) / device.replace(":", "-")
                home.mkdir()
                env = runtime.environment(home, 4, None, device=device)
                cfg = tomllib.loads((Path(env["NOSH_HOME"]) / "config.toml").read_text())
                self.assertEqual(cfg["model"]["device"], runtime.inference_device(device))
                if device == "cpu":
                    self.assertNotIn("CUDA_VISIBLE_DEVICES", env)
                    self.assertNotIn("LD_LIBRARY_PATH", env)
                else:
                    self.assertEqual(env["CUDA_VISIBLE_DEVICES"], "1")
                    self.assertEqual(env["LD_LIBRARY_PATH"], "/cuda/lib64")
                self.assertNotIn("NOSH_DEVICE", env)

    def test_gpu_requests_require_observed_gpu_and_old_cpu_traces_still_work(self):
        events = [
            {"ev": "engine", "info": {"load_s": 1.0}},
            {"ev": "open", "sid": 1, "sampling": {"seed": 0}},
            {"ev": "step_start", "sid": 1, "messages": [{"role": "user", "text": "task"}]},
            {"ev": "step_end", "sid": 1, "text": "answer", "errors": [], "tool_calls": [],
             "stop": "end_of_turn", "usage": {"ttft_s": 0.1}},
        ]
        result = driver.Result(transcript="| + 1 steps | 1.0 s\n")
        scenario = {"mode": "repl", "check": "largest"}
        with tempfile.TemporaryDirectory() as temporary:
            trace = Path(temporary) / "trace.jsonl"
            def save():
                trace.write_text("\n".join(json.dumps(dict(e, schema_version=1, engine=1)) for e in events))
            save()
            observations.observe(result, scenario, trace, seed=0)
            for actual in (None, "cpu", "cuda:1"):
                if actual is not None:
                    events[0]["info"]["device"] = actual
                save()
                with self.assertRaisesRegex(ValueError, "device mismatch"):
                    observations.observe(result, scenario, trace, seed=0, expected_device="cuda")
            events[0]["info"]["device"] = "cuda:0"
            save()
            observed = observations.observe(result, scenario, trace, seed=0, expected_device="cuda")
            self.assertEqual(observed["engines"][0]["device"], "cuda:0")
            with self.assertRaisesRegex(ValueError, "device mismatch"):
                observations.observe(result, scenario, trace, seed=0)
