"""CPU-only, matched f16/q8 KV measurements; generated commands are never executed."""

import argparse
import hashlib
import json
import math
import os
from pathlib import Path
import platform
import signal
import socket
import statistics
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request

from . import kv_assets as assets


THRESHOLD = 0.15
ORDERS = (("f16", "q8_0"), ("q8_0", "f16"))
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))
TRANSCRIPT = (
    "user@host:/work$ printf '%s\\n' 'build complete'\n"
    "build complete\nuser@host:/work$ find . -maxdepth 1 -type f\n"
    "./src/main.py\n./build log.txt\n"
) * 600
APPEND = "\nuser@host:/work$ printf '%s\\n' 'next task'; pwd; find . -maxdepth 1 -type f\n"


def utc():
    return time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())


def median(values):
    if not values or not all(math.isfinite(x) and x > 0 for x in values):
        raise ValueError(f"Invalid timing samples: {values}")
    return statistics.median(values)


def compare(f16, q8):
    result = {}
    for metric, field in (("prefill", "prompt_per_second"), ("decode", "predicted_per_second")):
        left = median([sample["final"]["timings"][field] for sample in f16])
        right = median([sample["final"]["timings"][field] for sample in q8])
        result[metric] = {
            "f16_tok_s": left, "q8_tok_s": right,
            "change_percent": (right / left - 1) * 100, "passes": right / left > 0.95,
        }
    result["passes_both"] = all(result[key]["passes"] for key in ("prefill", "decode"))
    return result


def cpu_list(text):
    result = set()
    for part in text.strip().split(","):
        bounds = [int(value) for value in part.split("-")]
        if len(bounds) not in (1, 2) or min(bounds) < 0 or bounds[-1] < bounds[0]:
            raise ValueError(f"Invalid CPU list: {text}")
        result.update(range(bounds[0], bounds[-1] + 1))
    return result


def busy(before, after, own=0):
    total, idle = after[0] - before[0], after[1] - before[1]
    if total <= 0 or idle < 0 or idle > total or own < 0:
        raise ValueError("Missing or non-monotonic CPU counters")
    return max(0, total - idle - own) / total


def cpu_counters(cpus):
    result = {}
    wanted = {"cpu", *(f"cpu{cpu}" for cpu in cpus)}
    for line in Path("/proc/stat").read_text().splitlines():
        fields = line.split()
        if fields[0] in wanted:
            values = [int(value) for value in fields[1:9]]
            if len(values) != 8:
                raise ValueError("Incomplete CPU counters")
            result[fields[0]] = (sum(values), values[3] + values[4])
    if set(result) != wanted:
        raise ValueError("Missing CPU counters")
    return result


def core_sum(counters, cpus):
    return tuple(sum(counters[f"cpu{cpu}"][index] for cpu in cpus) for index in (0, 1))


def contention(before, after, cpus, siblings, own):
    return {
        "global_external": busy(before["cpu"], after["cpu"], own),
        "target_external": busy(core_sum(before, cpus), core_sum(after, cpus), own),
        "sibling_busy": max(
            (busy(before[f"cpu{cpu}"], after[f"cpu{cpu}"]) for cpu in siblings), default=0),
    }


def choose_cpus(topology, occupancy):
    selected = []
    cores = set()
    for cpu in sorted(topology, key=lambda n: (
            max(occupancy[v] for v in topology[n]["siblings"]),
            0 in topology[n]["siblings"], n)):
        core = tuple(topology[cpu]["core"])
        if core not in cores:
            selected.append(cpu)
            cores.add(core)
        if len(selected) == 2:
            break
    reserved = set().union(*(set(topology[n]["siblings"]) for n in selected))
    controllers = sorted(set(topology) - reserved)
    if len(selected) != 2 or not controllers:
        raise ValueError("Need two distinct cores plus a separate CPU for the controller")
    return {"cpus": sorted(selected), "siblings": sorted(reserved - set(selected)),
            "controller_cpus": controllers}


def cpu_plan():
    allowed = set(os.sched_getaffinity(0))
    topology = {}
    for cpu in sorted(allowed):
        folder = Path(f"/sys/devices/system/cpu/cpu{cpu}/topology")
        siblings = cpu_list((folder / "thread_siblings_list").read_text())
        if not siblings <= allowed:
            raise ValueError("Cannot observe all SMT siblings inside the CPU allowance")
        topology[cpu] = {
            "core": [int((folder / "physical_package_id").read_text()),
                     int((folder / "core_id").read_text())],
            "siblings": sorted(siblings),
        }
    before = cpu_counters(allowed)
    time.sleep(3)
    after = cpu_counters(allowed)
    occupancy = {cpu: busy(before[f"cpu{cpu}"], after[f"cpu{cpu}"]) for cpu in allowed}
    plan = {**choose_cpus(topology, occupancy), "topology": topology, "occupancy": occupancy}
    os.sched_setaffinity(0, plan["controller_cpus"])
    return plan


def validate_masks(masks, cpus):
    if not masks or any(not mask or not set(mask) <= set(cpus) for mask in masks.values()):
        raise ValueError(f"Model threads escaped the selected CPUs: {masks}")


def thread_masks(pid):
    masks = {}
    for task in Path(f"/proc/{pid}/task").iterdir():
        try:
            masks[int(task.name)] = sorted(os.sched_getaffinity(int(task.name)))
        except ProcessLookupError:
            # A short-lived worker may exit between enumeration and the affinity query.
            continue
    return masks


def wait_idle(plan):
    cpus = plan["cpus"] + plan["siblings"]
    deadline = time.monotonic() + 300
    while True:
        before = cpu_counters(cpus)
        time.sleep(2)
        after = cpu_counters(cpus)
        fractions = {key: busy(before[key], after[key]) for key in before}
        memory = next(int(line.split()[1]) * 1024
                      for line in Path("/proc/meminfo").read_text().splitlines()
                      if line.startswith("MemAvailable:"))
        if max(fractions.values()) <= THRESHOLD and memory >= 2 * 1024**3:
            return {"cpu_busy": fractions, "memory_available_bytes": memory}
        if time.monotonic() >= deadline:
            raise RuntimeError(f"Host remains busy: {fractions}, available RAM {memory}")
        print("WAITING FOR IDLE", fractions, memory, flush=True)
        time.sleep(5)


class Monitor:
    def __init__(self, process, binary, plan):
        self.process, self.binary, self.plan = process, binary.resolve(), plan
        self.samples, self.windows, self.errors = [], [], []
        self.masks, self.affinity_checks = set(), 0
        self.previous = None
        self.event = threading.Event()
        self.thread = threading.Thread(target=self.watch, daemon=True)
        self.thread.start()

    def sample(self):
        if self.process.poll() is not None:
            return
        try:
            pid = self.process.pid
            memory = {}
            for line in Path(f"/proc/{pid}/status").read_text().splitlines():
                key, _, value = line.partition(":")
                if key in ("VmRSS", "VmHWM", "VmSwap"):
                    memory[key] = int(value.split()[0]) * 1024
            if len(memory) != 3:
                raise ValueError("Missing process memory counters")
            now = time.monotonic()
            self.samples.append({"monotonic": now, **memory})
            if Path(f"/proc/{pid}/exe").resolve() != self.binary:
                return
            masks = thread_masks(pid)
            self.masks.update(tuple(mask) for mask in masks.values())
            self.affinity_checks += len(masks)
            validate_masks(masks, self.plan["cpus"])
            counters = cpu_counters(self.plan["cpus"] + self.plan["siblings"])
            fields = Path(f"/proc/{pid}/stat").read_text().rsplit(") ", 1)[1].split()
            own = int(fields[11]) + int(fields[12])
            if self.previous is None:
                self.previous = (now, counters, own)
            elif now - self.previous[0] >= 1:
                self.windows.append({
                    "monotonic": now,
                    **contention(self.previous[1], counters, self.plan["cpus"],
                                 self.plan["siblings"], own - self.previous[2]),
                })
                self.previous = (now, counters, own)
        except (OSError, ValueError) as error:
            if self.process.poll() is None:
                self.errors.append(str(error))

    def watch(self):
        while not self.event.wait(0.2):
            self.sample()

    def finish(self):
        self.event.set()
        self.thread.join(timeout=5)
        if self.thread.is_alive():
            self.errors.append("Monitor did not stop")
        self.sample()
        if not self.samples or not self.windows or not self.affinity_checks:
            self.errors.append("Missing memory, CPU or affinity observations")
        return {
            "memory": self.samples, "cpu_windows": self.windows, "errors": self.errors,
            "affinity_checks": self.affinity_checks,
            "observed_masks": [list(mask) for mask in sorted(self.masks)],
            "peak_rss_bytes": max((s["VmHWM"] for s in self.samples), default=0),
            "peak_swap_bytes": max((s["VmSwap"] for s in self.samples), default=0),
            "max_contention": {
                key: max((w[key] for w in self.windows), default=0)
                for key in ("global_external", "target_external", "sibling_busy")
            },
        }


def payload(prompt, count=128, cache=False):
    return {"prompt": prompt, "n_predict": count, "cache_prompt": cache,
            "temperature": 0, "seed": 42, "ignore_eos": True, "return_tokens": True}


def post(base, endpoint, data):
    request = urllib.request.Request(base + endpoint, json.dumps(data).encode(),
                                     {"Content-Type": "application/json"})
    with HTTP.open(request, timeout=900) as response:
        return json.load(response)


def stream(base, data, path):
    start = time.monotonic()
    request = urllib.request.Request(
        base + "/completion", json.dumps({**data, "stream": True}).encode(),
        {"Content-Type": "application/json"})
    events, tokens, final, ttft = [], [], None, None
    try:
        with HTTP.open(request, timeout=900) as response:
            for line in response:
                elapsed = time.monotonic() - start
                if elapsed > 900:
                    raise TimeoutError("Streaming request exceeded 900 seconds")
                if not line.startswith(b"data: "):
                    continue
                raw = line[6:].strip()
                if raw == b"[DONE]":
                    break
                event = json.loads(raw)
                if "error" in event:
                    raise RuntimeError(f"Server error: {event['error']}")
                events.append({"elapsed_seconds": elapsed, "data": event})
                tokens.extend(event.get("tokens", []))
                if ttft is None and (event.get("tokens") or event.get("content")):
                    ttft = elapsed
                if event.get("stop"):
                    final = event
    finally:
        assets.save(path, {"request": data, "events": events})
    if final is None or ttft is None:
        raise ValueError("Incomplete token stream")
    return {"tokens": tokens, "final": final, "ttft_seconds": ttft,
            "wall_seconds": time.monotonic() - start}


def validate_sample(sample, prompt_count, generated=128, cache=False):
    timings = sample["final"]["timings"]
    actual = timings["prompt_n"] + (timings.get("cache_n", 0) if cache else 0)
    if actual != prompt_count or timings["predicted_n"] != generated:
        raise ValueError("Actual prompt or generated token count differs")
    if len(sample["tokens"]) != generated or sample["final"].get("truncated"):
        raise ValueError("Incomplete or truncated generation")
    if not cache and timings.get("cache_n", 0):
        raise ValueError("Uncached request reused a prefix")
    if generated > 1:
        for key in ("prompt_per_second", "predicted_per_second"):
            median([timings[key]])


def prepare_prompts(base, model):
    result = {}
    for target in (1024, 8064):
        tokens = post(base, "/tokenize", {"content": TRANSCRIPT, "add_special": False})["tokens"]
        text = post(base, "/detokenize", {"tokens": tokens[:target - 2]})["content"]
        actual = post(base, "/tokenize", {"content": text, "add_special": True})["tokens"]
        if not target - 4 <= len(actual) <= target:
            raise ValueError("Prepared prompt length differs")
        result[str(target)] = {"text": text, "tokens": actual, "count": len(actual)}
    checksum = hashlib.sha256(
        json.dumps(payload(result["8064"]["text"]), sort_keys=True).encode()).hexdigest()
    if checksum != model["request_sha256"]:
        raise ValueError("ARM input request differs from the x86 reference")
    result["request_sha256"] = checksum
    result["append"] = post(base, "/tokenize", {"content": APPEND, "add_special": False})["tokens"]
    return result


def command(binary, model, plan, kv, port):
    if kv not in ("f16", "q8_0"):
        raise ValueError("Unsupported cache precision")
    mask = hex(sum(1 << cpu for cpu in plan["cpus"]))
    return [
        str(binary), "-m", model["path"], "-ngl", "0", "-t", "2", "-tb", "2",
        "-c", "8192", "-np", "1", "-b", "512", "-ub", "128",
        "-ctk", kv, "-ctv", kv, "-fa", "on", "--cache-ram", "0", "--ctx-checkpoints", "0",
        "--no-context-shift", "--host", "127.0.0.1", "--port", str(port),
        "--no-webui", "--jinja", "--reasoning", "off", "--reasoning-budget", "0",
        "--alias", model["model_id"] + "-" + model["quant"],
        "--cpu-mask", mask, "--cpu-mask-batch", mask, "--cpu-strict", "1", "--cpu-strict-batch", "1",
    ]


def stop(process):
    if process.poll() is None:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=15)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()


def profile(model, binary, plan, root, round_index, kv, attempt=1):
    directory = root / f"r{round_index}-{kv}-attempt{attempt}"
    directory.mkdir(parents=True, exist_ok=False)
    with Path(model["path"]).open("rb") as weights:
        while weights.read(16 * 1024**2):
            pass
    before = wait_idle(plan)
    with socket.socket() as available:
        available.bind(("127.0.0.1", 0))
        port = available.getsockname()[1]
    base = f"http://127.0.0.1:{port}"
    args = command(binary, model, plan, kv, port)
    result = {
        "status": "running", "round": round_index, "kv": kv, "attempt": attempt,
        "started_utc": utc(), "command": args, "samples": [], "environment_before": before,
    }
    assets.save(directory / "result.json", result)
    process, monitor = None, None
    print("START", model["id"], round_index, kv, attempt, flush=True)
    try:
        wrapper = (
            "import os,resource,sys;"
            f"os.sched_setaffinity(0,{set(plan['cpus'])!r});"
            "resource.setrlimit(resource.RLIMIT_CORE,(0,0));"
            "resource.setrlimit(resource.RLIMIT_AS,(12884901888,12884901888));"
            "os.execv(sys.argv[1],sys.argv[1:])"
        )
        with (directory / "server.log").open("w") as log:
            process = subprocess.Popen(
                [sys.executable, "-c", wrapper, *args], process_group=0, stdout=log,
                stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL, env={**os.environ, "LC_ALL": "C"})
        monitor = Monitor(process, binary, plan)
        result["server_pid"] = process.pid
        start = time.monotonic()
        while True:
            if process.poll() is not None:
                raise RuntimeError(f"llama-server exited {process.returncode}; see server.log")
            if time.monotonic() - start > 180:
                raise TimeoutError("llama-server startup exceeded 180 seconds")
            try:
                with HTTP.open(base + "/health", timeout=2) as response:
                    if json.load(response)["status"] == "ok":
                        break
            except (urllib.error.URLError, TimeoutError):
                time.sleep(0.05)
        with HTTP.open(base + "/props", timeout=20) as response:
            properties = json.load(response)
        assets.save(directory / "properties.json", properties)
        if (properties["total_slots"] != 1
                or properties["default_generation_settings"]["n_ctx"] != 8192
                or Path(properties["model_path"]).resolve() != Path(model["path"]).resolve()):
            raise ValueError("Effective server configuration differs")
        masks = thread_masks(process.pid)
        validate_masks(masks, plan["cpus"])
        assets.save(directory / "affinity-on-ready.json", masks)
        prompt_path = root / "prompts.json"
        if not prompt_path.exists():
            assets.save(prompt_path, prepare_prompts(base, model))
        prepared = json.loads(prompt_path.read_text())
        if prepared["request_sha256"] != model["request_sha256"]:
            raise ValueError("Cached prompt provenance differs")
        cold = stream(base, payload(prepared["1024"]["text"], 1), directory / "cold.json")
        validate_sample(cold, prepared["1024"]["count"], 1)
        result["cold"] = cold
        tokens = prepared["1024"]["tokens"] + cold["tokens"]
        result["warm"] = []
        for index in range(5):
            tokens.extend(prepared["append"])
            sample = stream(base, payload(tokens, 1, True), directory / f"warm-{index}.json")
            validate_sample(sample, len(tokens), 1, True)
            timings = sample["final"]["timings"]
            sample["verified_prefix_reuse"] = (
                timings.get("cache_n", 0) >= len(tokens) - len(prepared["append"]) - 8
                and 1 <= timings["prompt_n"] <= len(prepared["append"]) + 8)
            result["warm"].append(sample)
            tokens.extend(sample["tokens"])
        result["warm_reuse_valid"] = all(s["verified_prefix_reuse"] for s in result["warm"])
        if not result["warm_reuse_valid"]:
            result["warm_warning"] = "Prefix reuse is unverified; do not compare warm TTFT"
            print(result["warm_warning"], flush=True)
        for index in range(3):
            sample = stream(base, payload(prepared["8064"]["text"]),
                            directory / f"near-context-{index}.json")
            validate_sample(sample, prepared["8064"]["count"])
            result["samples"].append(sample)
            assets.save(directory / "result.json", result)
        result["status"] = "ok"
    except (OSError, ValueError, RuntimeError, KeyError, subprocess.SubprocessError) as error:
        result.update(status="failed", error=str(error))
        print("PROFILE FAILED", model["id"], kv, str(error), flush=True)
    except (KeyboardInterrupt, SystemExit):
        result.update(status="cancelled", error="Measurement interrupted")
        raise
    finally:
        try:
            if monitor is not None:
                resource = monitor.finish()
                assets.save(directory / "resources.json", resource)
                result["resources"] = {k: v for k, v in resource.items()
                                       if k not in ("memory", "cpu_windows")}
                if result["status"] == "ok":
                    if resource["errors"] or resource["peak_rss_bytes"] <= 0 or resource["peak_swap_bytes"]:
                        result.update(status="failed", error="Invalid memory/CPU observations or swap")
                    elif max(resource["max_contention"].values()) > THRESHOLD:
                        result.update(status="contended", error="External CPU exceeded 15%")
        finally:
            if process is not None:
                stop(process)
            result["finished_utc"] = utc()
            assets.save(directory / "result.json", result)
            print("RESULT", model["id"], round_index, kv, result["status"], flush=True)
    return result


def summary(model, rows):
    valid = {(r["round"], r["kv"]): r for r in rows if r["status"] == "ok"}
    expected = {(index, kv) for index, order in enumerate(ORDERS, 1) for kv in order}
    complete = len(rows) == 4 and set(valid) == expected
    result = {
        "generated_utc": utc(), "case": model["id"], "model_id": model["model_id"],
        "quant": model["quant"], "sha256": model["sha256"],
        "request_sha256": model["request_sha256"], "complete": complete,
        "expected_profiles": 4, "valid_profiles": len(valid),
        "throughput_samples": sum(len(r["samples"]) for r in valid.values()),
        "missing": sorted(expected - set(valid)), "profiles": rows,
        "quality_scored": False, "executed_commands": False, "gpu_measured": False,
    }
    if complete:
        if any(len(r["samples"]) != 3 for r in valid.values()):
            raise ValueError("Completed profile must contain exactly three samples")
        sequence = [valid[(index, kv)] for index, order in enumerate(ORDERS, 1) for kv in order]
        if any(a["finished_utc"] > b["started_utc"] for a, b in zip(sequence, sequence[1:])):
            raise ValueError("Paired profiles overlap or ran in the wrong order")
        combined = {kv: [s for i in (1, 2) for s in valid[(i, kv)]["samples"]]
                    for kv in ("f16", "q8_0")}
        result["comparison"] = compare(combined["f16"], combined["q8_0"])
        result["rounds"] = [compare(valid[(i, "f16")]["samples"], valid[(i, "q8_0")]["samples"])
                            for i in (1, 2)]
        result["passes_each_round"] = all(r["passes_both"] for r in result["rounds"])
        result["peak_rss_bytes"] = {
            kv: max(valid[(i, kv)]["resources"]["peak_rss_bytes"] for i in (1, 2))
            for kv in ("f16", "q8_0")
        }
    return result


def run(model, root, require_arm=False):
    if require_arm and platform.machine().lower() not in ("aarch64", "arm64"):
        raise ValueError("This CI measurement requires native ARM64, not emulation or x86")
    runtime = assets.manifest()["runtime_revision"]
    actual = subprocess.check_output(
        ["git", "-C", str(root / "llama.cpp"), "rev-parse", "HEAD"], text=True).strip()
    dirty = subprocess.check_output(
        ["git", "-C", str(root / "llama.cpp"), "status", "--porcelain"], text=True).strip()
    if actual != runtime or dirty:
        raise ValueError("Runtime checkout differs from the pinned clean source")
    result_dir = root / "results"
    asset = json.loads((result_dir / "asset.json").read_text())
    if any(asset[key] != value for key, value in model.items()) or not asset["verified"]:
        raise ValueError("Prepared asset provenance differs")
    assets.verify(Path(asset["path"]), model)
    if (result_dir / "summary.json").exists():
        raise ValueError("Use a fresh output root; prior measurements must not be overwritten")
    plan = cpu_plan()
    binary = root / "build" / "bin" / "llama-server"
    environment = {
        "recorded_utc": utc(), "machine": platform.machine(), "kernel": platform.uname()._asdict(),
        "cpu": json.loads(subprocess.check_output(["lscpu", "--json"], text=True)),
        "runtime_revision": runtime, "binary_sha256": assets.digest(binary), "plan": plan,
        "python": sys.version, "controller_affinity": sorted(os.sched_getaffinity(0)),
        "source_sha256": {p.name: assets.digest(p) for p in
                          (Path(__file__), Path(assets.__file__), assets.MANIFEST)},
        "ci": {key: os.environ.get(key) for key in
               ("GITHUB_RUN_ID", "GITHUB_RUN_ATTEMPT", "GITHUB_SHA", "RUNNER_NAME",
                "RUNNER_ARCH", "ImageOS", "ImageVersion")},
    }
    assets.save(result_dir / "environment.json", environment)
    rows = []
    assets.save(result_dir / "summary.json", summary(model, rows))
    try:
        for index, order in enumerate(ORDERS, 1):
            for kv in order:
                row = profile(asset, binary, plan, result_dir, index, kv)
                if row["status"] == "contended":
                    row = profile(asset, binary, plan, result_dir, index, kv, attempt=2)
                rows.append(row)
                assets.save(result_dir / "summary.json", summary(model, rows))
                if row["status"] != "ok":
                    return False
    finally:
        assets.save(result_dir / "summary.json", summary(model, rows))
    return True


def audit_case(directory, row, require_arm=True):
    model = next(m for m in assets.manifest()["models"] if m["id"] == row["case"])
    for key in ("sha256", "request_sha256", "model_id", "quant"):
        if row[key] != model[key]:
            raise ValueError(f"Case identity differs: {key}")
    environment = json.loads((directory / "environment.json").read_text())
    if (environment["runtime_revision"] != assets.manifest()["runtime_revision"]
            or require_arm and environment["machine"].lower() not in ("aarch64", "arm64")):
        raise ValueError("Not the pinned native ARM64 runtime")
    plan = environment["plan"]
    cores = {tuple(plan["topology"][str(cpu)]["core"]) for cpu in plan["cpus"]}
    if len(plan["cpus"]) != 2 or len(cores) != 2:
        raise ValueError("Recorded target CPUs are not two distinct cores")
    if not plan["controller_cpus"] or set(plan["controller_cpus"]) & set(plan["cpus"] + plan["siblings"]):
        raise ValueError("Controller shares a model core or SMT sibling")
    for profile_row in row["profiles"]:
        if profile_row["status"] != "ok":
            continue
        index, cache, attempt = profile_row["round"], profile_row["kv"], profile_row["attempt"]
        if index not in (1, 2) or cache not in ORDERS[0] or attempt not in (1, 2):
            raise ValueError("Unexpected profile identity")
        folder = directory / f"r{index}-{cache}-attempt{attempt}"
        if json.loads((folder / "result.json").read_text()) != profile_row:
            raise ValueError("Summary differs from the original profile")
        resource = json.loads((folder / "resources.json").read_text())
        if resource["errors"] or not resource["affinity_checks"] or not resource["cpu_windows"]:
            raise ValueError("Missing resource/affinity evidence")
        validate_masks(dict(enumerate(resource["observed_masks"])), plan["cpus"])
        ready = json.loads((folder / "affinity-on-ready.json").read_text())
        validate_masks(ready, plan["cpus"])
        if (resource["peak_rss_bytes"] != max(s["VmHWM"] for s in resource["memory"])
                or resource["peak_swap_bytes"] or any(s["VmSwap"] for s in resource["memory"])):
            raise ValueError("Invalid peak RSS or swap")
        for key, value in resource["max_contention"].items():
            if value != max(w[key] for w in resource["cpu_windows"]) or value > THRESHOLD:
                raise ValueError("Invalid CPU contention evidence")
        if profile_row["resources"] != {
                k: v for k, v in resource.items() if k not in ("memory", "cpu_windows")}:
            raise ValueError("Summary resource counters differ")
        args = profile_row["command"]
        settings = {
            "-ngl": "0", "-t": "2", "-tb": "2", "-c": "8192", "-np": "1",
            "-b": "512", "-ub": "128", "-ctk": cache, "-ctv": cache, "-fa": "on",
            "--cache-ram": "0", "--ctx-checkpoints": "0", "--cpu-strict": "1",
            "--cpu-strict-batch": "1",
        }
        if "--no-repack" in args or any(args[args.index(k) + 1] != v for k, v in settings.items()):
            raise ValueError("Effective model parameters differ")
        for flag in ("--cpu-mask", "--cpu-mask-batch"):
            if int(args[args.index(flag) + 1], 16) != sum(1 << n for n in plan["cpus"]):
                raise ValueError("Effective CPU mask differs")
        prepared = json.loads((directory / "prompts.json").read_text())
        properties = json.loads((folder / "properties.json").read_text())
        if (properties["total_slots"] != 1
                or properties["default_generation_settings"]["n_ctx"] != 8192
                or properties["model_path"] != args[args.index("-m") + 1]):
            raise ValueError("Actual server properties differ")
        if len(profile_row["samples"]) != 3:
            raise ValueError("Missing throughput samples")
        for sample_index, sample in enumerate(profile_row["samples"]):
            raw = json.loads((folder / f"near-context-{sample_index}.json").read_text())
            checksum = hashlib.sha256(json.dumps(raw["request"], sort_keys=True).encode()).hexdigest()
            if checksum != model["request_sha256"] or raw["events"][-1]["data"] != sample["final"]:
                raise ValueError("Raw request/response differs from the matched reference")
            tokens = [t for event in raw["events"] for t in event["data"].get("tokens", [])]
            if tokens != sample["tokens"]:
                raise ValueError("Raw token stream differs")
            validate_sample(sample, prepared["8064"]["count"])
    recalculated = summary(model, row["profiles"])
    if any(row.get(key) != value for key, value in recalculated.items() if key != "generated_utc"):
        raise ValueError("Reported comparison differs from its samples")


def report(root, cases="all"):
    expected = {model["id"] for model in assets.select_models(cases)}
    rows = []
    seen = set()
    for path in sorted(root.rglob("summary.json")):
        row = json.loads(path.read_text())
        if row["case"] not in expected or row["case"] in seen:
            raise ValueError("Unexpected or duplicated benchmark case")
        seen.add(row["case"])
        try:
            audit_case(path.parent, row)
        except (OSError, ValueError, KeyError) as error:
            row = {**row, "complete": False, "audit_error": str(error)}
            print("AUDIT FAILED", row["case"], str(error), flush=True)
        rows.append(row)
    result = {"complete": seen == expected and all(r["complete"] for r in rows),
              "expected_cases": sorted(expected),
              "missing": sorted(expected - seen), "cases": rows}
    assets.save(root / "report.json", result)
    lines = [
        "## ARM64 f16/q8 KV comparison",
        "Two distinct CPU cores; 8K; repack; batch 512/128; six samples per cache type.",
        "Loss below 5% requires BOTH unrounded throughput ratios to be strictly >0.95.",
        "",
        "| Case | Profiles | f16/q8 RSS MB | Prefill change | Decode change | Both rounds pass |",
        "| --- | ---: | ---: | ---: | ---: | --- |",
    ]
    for row in rows:
        if row["complete"]:
            pair = row["comparison"]
            memory = row["peak_rss_bytes"]
            lines.append(
                f"| {row['case']} | 4/4 | {memory['f16']/1e6:.1f}/{memory['q8_0']/1e6:.1f} | "
                f"{pair['prefill']['change_percent']:+.2f}% | {pair['decode']['change_percent']:+.2f}% | "
                f"{pair['passes_both'] and row['passes_each_round']} |")
        else:
            lines.append(f"| {row['case']} | {row['valid_profiles']}/4 | INCOMPLETE | N/A | N/A | N/A |")
    for case in sorted(expected - seen):
        lines.append(f"| {case} | 0/4 | MISSING | N/A | N/A | N/A |")
    for row in rows:
        if row.get("audit_error"):
            lines.append(f"\nAudit error for {row['case']}: {row['audit_error']}")
    lines += ["", "Only measured runner hardware is covered; this is not a claim about most ARM CPUs.",
              "No model quality, GPU performance or generated-command execution is evaluated."]
    text = "\n".join(lines) + "\n"
    (root / "report.md").write_text(text, encoding="utf-8")
    if os.environ.get("GITHUB_STEP_SUMMARY"):
        with Path(os.environ["GITHUB_STEP_SUMMARY"]).open("a", encoding="utf-8") as destination:
            destination.write(text)
    print(text)
    return result["complete"]


def interrupted(signum, frame):
    raise KeyboardInterrupt(f"Signal {signum}")


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("action", choices=("matrix", "prepare", "run", "report"))
    parser.add_argument("--root", type=Path)
    parser.add_argument("--case", choices=[m["id"] for m in assets.manifest()["models"]])
    parser.add_argument("--cases", default="all")
    parser.add_argument("--quantizer", type=Path)
    parser.add_argument("--require-arm", action="store_true")
    args = parser.parse_args()
    if args.action == "matrix":
        print(json.dumps([{"case": model["id"], "hf": model["kind"] == "convert_hf",
                           "quantize": model["kind"] != "gguf"}
                          for model in assets.select_models(args.cases)]))
        return 0
    if args.root is None:
        parser.error("--root is required for prepare, run and report")
    root = args.root.resolve()
    if args.action == "report":
        return 0 if report(root, args.cases) else 1
    if not args.case:
        parser.error("--case is required for prepare and run")
    model = next(m for m in assets.manifest()["models"] if m["id"] == args.case)
    for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
        signal.signal(signum, interrupted)
    if args.action == "prepare":
        assets.prepare(model, root, args.quantizer.resolve() if args.quantizer else None)
        return 0
    return 0 if run(model, root, args.require_arm) else 1


if __name__ == "__main__":
    raise SystemExit(main())
