"""Pinned public model preparation for the optional KV-cache experiment."""

import hashlib
import json
from pathlib import Path, PurePosixPath
import re
import subprocess
import sys
import urllib.parse
import urllib.request


MANIFEST = Path(__file__).with_name("kv_cache_models.json")
SUPPORT = {
    "config.json", "generation_config.json", "tokenizer.json", "tokenizer_config.json",
    "special_tokens_map.json", "added_tokens.json", "tokenizer.model", "spiece.model",
    "vocab.json", "merges.txt", "chat_template.jinja", "README.md", "LICENSE", "NOTICE",
    "STUDENT_LICENSE", "TEACHER_LICENSE", "model.safetensors.index.json",
}


def manifest():
    data = json.loads(MANIFEST.read_text(encoding="utf-8"))
    if not re.fullmatch("[0-9a-f]{40}", data["runtime_revision"]):
        raise ValueError("Runtime revision must be immutable")
    ids = set()
    for model in data["models"]:
        if model["id"] in ids or not re.fullmatch("[a-z0-9-]+", model["id"]):
            raise ValueError("Duplicate or invalid case ID")
        ids.add(model["id"])
        if not re.fullmatch("[0-9a-f]{40}", model["revision"]):
            raise ValueError("Model revision must be immutable")
        for field in ("sha256", "request_sha256"):
            if not re.fullmatch("[0-9a-f]{64}", model[field]):
                raise ValueError(f"Invalid {field}")
        if model["kind"] not in ("gguf", "convert_hf", "quantize_gguf"):
            raise ValueError("Unsupported model preparation")
    return data


def select_models(cases="all"):
    models = manifest()["models"]
    if cases == "all":
        return models
    requested = cases.split(",")
    known = {model["id"] for model in models}
    if len(set(requested)) != len(requested) or not set(requested) <= known:
        raise ValueError("Cases must be unique manifest IDs separated by commas, or 'all'")
    return [model for model in models if model["id"] in requested]


def save(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_text(json.dumps(data, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")
    temporary.replace(path)


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verify(path, expected):
    actual = digest(path)
    if "bytes" in expected and path.stat().st_size != expected["bytes"]:
        raise ValueError(f"File size differs: {path.name}")
    if expected.get("sha256"):
        if actual != expected["sha256"]:
            raise ValueError(f"SHA-256 mismatch: {path.name}: {actual} != {expected['sha256']}")
    elif expected.get("git_oid"):
        checksum = hashlib.sha1(f"blob {path.stat().st_size}\0".encode())
        with path.open("rb") as stream:
            while block := stream.read(1024 * 1024):
                checksum.update(block)
        if checksum.hexdigest() != expected["git_oid"]:
            raise ValueError(f"Git blob checksum mismatch: {path.name}")
    else:
        raise ValueError(f"No source checksum: {path.name}")
    return actual


def source_path(folder, filename):
    relative = PurePosixPath(filename)
    if relative.is_absolute() or ".." in relative.parts or "\\" in filename:
        raise ValueError(f"Unsafe source filename: {filename}")
    return folder.joinpath(*relative.parts)


def execute(command, log, timeout=900):
    with log.open("a", encoding="utf-8") as output:
        output.write(json.dumps([str(arg) for arg in command]) + "\n")
        output.flush()
        subprocess.run([str(arg) for arg in command], check=True, timeout=timeout,
                       stdout=output, stderr=subprocess.STDOUT, stdin=subprocess.DEVNULL)


def download(model, item, folder, log):
    destination = source_path(folder, item["file"])
    destination.parent.mkdir(parents=True, exist_ok=True)
    if not destination.exists():
        partial = destination.with_name(destination.name + ".partial")
        url = (f"https://huggingface.co/{model['repo']}/resolve/{model['revision']}/"
               + urllib.parse.quote(item["file"], safe="/") + "?download=true")
        execute([
            "curl", "--fail", "--location", "--silent", "--show-error",
            "--proto", "=https", "--proto-redir", "=https", "--retry", "3",
            "--retry-all-errors", "--connect-timeout", "30", "--max-time", "600",
            "--continue-at", "-", "--output", partial, url,
        ], log)
        verify(partial, item)
        partial.replace(destination)
    checksum = verify(destination, item)
    return {"file": item["file"], "path": str(destination),
            "bytes": destination.stat().st_size, "sha256": checksum}


def hf_files(model):
    url = (f"https://huggingface.co/api/models/{model['repo']}/tree/"
           f"{model['revision']}?recursive=true&limit=100")
    with urllib.request.urlopen(url, timeout=90) as response:
        if response.headers.get("Link"):
            raise ValueError("Pinned small-model tree unexpectedly requires pagination")
        rows = json.load(response)
    selected = [
        {"file": row["path"], "bytes": row["size"], "git_oid": row["oid"],
         "sha256": row.get("lfs", {}).get("oid")}
        for row in rows if row["type"] == "file"
        and (row["path"] in SUPPORT or row["path"].endswith(".safetensors"))
    ]
    weights = {row["file"]: row["sha256"] for row in selected
               if row["file"].endswith(".safetensors")}
    if weights != model["hf_weights"] or not any(x["file"] == "config.json" for x in selected):
        raise ValueError("Pinned HF weight inventory differs")
    return selected


def prepare(model, root, quantizer=None):
    result_dir = root / "results"
    save(result_dir / "case.json", model)
    source = root / "sources" / model["model_id"] / model["revision"]
    items = hf_files(model) if model["kind"] == "convert_hf" else [model["source"]]
    receipts = [download(model, item, source, result_dir / "prepare.log") for item in items]
    save(result_dir / "source-receipts.json", receipts)
    if model["kind"] == "gguf":
        output = Path(receipts[0]["path"])
    else:
        folder = root / "models" / model["model_id"]
        folder.mkdir(parents=True, exist_ok=True)
        if model["kind"] == "convert_hf":
            high_precision = folder / f"{model['model_id']}-BF16.gguf"
            temporary = high_precision.with_name(high_precision.name + ".incomplete")
            execute([
                sys.executable, root / "llama.cpp" / "convert_hf_to_gguf.py", source,
                "--outtype", "bf16", "--outfile", temporary,
            ], result_dir / "prepare.log")
            verify(temporary, {"sha256": model["precision_source_sha256"]})
            temporary.replace(high_precision)
        else:
            high_precision = Path(receipts[0]["path"])
        output = folder / f"{model['model_id']}-{model['quant']}.gguf"
        if quantizer is None:
            quantizer = root / "build" / "bin" / "llama-quantize"
        command = [quantizer]
        if model["quant"] == "Q4_0_PURE":
            command.append("--pure")
        command += [high_precision, output,
                    "Q4_0" if model["quant"] == "Q4_0_PURE" else model["quant"], "2"]
        execute(command, result_dir / "prepare.log")
    assets_output = {
        "expected_sha256": model["sha256"], "actual_sha256": digest(output),
        "expected_bytes": model["bytes"], "actual_bytes": output.stat().st_size,
    }
    if quantizer is not None:
        assets_output["quantizer_sha256"] = digest(quantizer)
    save(result_dir / "model-output.json", assets_output)
    verify(output, model)
    receipt = {**model, "path": str(output), "verified": True}
    save(result_dir / "asset.json", receipt)
    return receipt
