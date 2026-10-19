"""Development-only teacher forcing against verified llama.cpp b11706.

The ABI below is from include/llama.h at
f74e2fa6a31cc33ab4ee4ae516641cfe62df3924. Never used by nosh.
The exporter uses the digest-verified Windows x64 CPU release; the output
artifacts are portable to CPU/CUDA acceptance runs on other platforms.
"""

import argparse
import ctypes as c
import hashlib
import json
import os
from pathlib import Path
import time


class ModelParams(c.Structure):
    _fields_ = [
        ("devices", c.c_void_p), ("tensor_buft_overrides", c.c_void_p),
        ("n_gpu_layers", c.c_int32), ("split_mode", c.c_int32),
        ("load_mode", c.c_int32), ("lazy_mode", c.c_int32), ("main_gpu", c.c_int32),
        ("tensor_split", c.c_void_p), ("progress_callback", c.c_void_p),
        ("progress_callback_user_data", c.c_void_p), ("kv_overrides", c.c_void_p),
        *[(name, c.c_bool) for name in (
            "vocab_only", "check_tensors", "use_extra_bufts", "no_host", "no_alloc", "load_mtp"
        )],
    ]


class ContextParams(c.Structure):
    _fields_ = [
        *[(name, c.c_uint32) for name in (
            "n_ctx", "n_batch", "n_ubatch", "n_seq_max", "n_rs_seq",
            "n_outputs_max", "n_outputs_max_per_seq"
        )],
        *[(name, c.c_int32) for name in (
            "n_threads", "n_threads_batch", "ctx_type", "rope_scaling_type",
            "pooling_type", "attention_type", "flash_attn_type"
        )],
        *[(name, c.c_float) for name in (
            "rope_freq_base", "rope_freq_scale", "yarn_ext_factor",
            "yarn_attn_factor", "yarn_beta_fast", "yarn_beta_slow"
        )],
        ("yarn_orig_ctx", c.c_uint32), ("defrag_thold", c.c_float),
        ("cb_eval", c.c_void_p), ("cb_eval_user_data", c.c_void_p),
        ("type_k", c.c_int32), ("type_v", c.c_int32), ("moe_cache_size", c.c_size_t),
        ("abort_callback", c.c_void_p), ("abort_callback_data", c.c_void_p),
        *[(name, c.c_bool) for name in (
            "embeddings", "offload_kqv", "no_perf", "op_offload", "swa_full", "kv_unified"
        )],
        ("samplers", c.c_void_p), ("n_samplers", c.c_size_t), ("ctx_other", c.c_void_p),
    ]


class Batch(c.Structure):
    _fields_ = [("n_tokens", c.c_int32), *[(name, c.c_void_p) for name in (
        "token", "embd", "pos", "n_seq_id", "seq_id", "logits"
    )]]


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def run(args):
    if os.name != "nt":
        raise RuntimeError("this exporter requires the verified Windows x64 b11706 CPU release")
    assert c.sizeof(ModelParams) == 80
    assert c.sizeof(ContextParams) == 168
    assert c.sizeof(Batch) == 56
    if digest(args.model) != "37ae482d336108d23516fa35e8e0c4126688d81018b87178a18d752a1357814f":
        raise ValueError("not the pinned Qwen Q8 weights")
    if digest(args.bin / "llama.dll") != "8f64b0cb6ccbe75780261eaae2e12485b8b3939809bd57000ba4e99e428e3217":
        raise ValueError("not the verified b11706 Windows x64 CPU library")
    dll_dir = os.add_dll_directory(str(args.bin))
    ggml = c.CDLL(str(args.bin / "ggml.dll"))
    ggml.ggml_backend_load_all_from_path.argtypes = [c.c_char_p]
    ggml.ggml_backend_load_all_from_path(str(args.bin).encode())
    api = c.CDLL(str(args.bin / "llama.dll"))

    def bind(name, result, params):
        function = getattr(api, name)
        function.restype = result
        function.argtypes = params
        return function

    model_defaults = bind("llama_model_default_params", ModelParams, [])
    context_defaults = bind("llama_context_default_params", ContextParams, [])
    load = bind("llama_model_load_from_file", c.c_void_p, [c.c_char_p, ModelParams])
    init = bind("llama_init_from_model", c.c_void_p, [c.c_void_p, ContextParams])
    get_vocab = bind("llama_model_get_vocab", c.c_void_p, [c.c_void_p])
    vocab_size = bind("llama_vocab_n_tokens", c.c_int32, [c.c_void_p])
    get_batch = bind("llama_batch_get_one", Batch, [c.POINTER(c.c_int32), c.c_int32])
    decode = bind("llama_decode", c.c_int32, [c.c_void_p, Batch])
    logits = bind("llama_get_logits_ith", c.c_void_p, [c.c_void_p, c.c_int32])
    free_context = bind("llama_free", None, [c.c_void_p])
    free_model = bind("llama_model_free", None, [c.c_void_p])
    bind("llama_backend_init", None, [])()
    params = model_defaults()
    params.n_gpu_layers = 0
    params.load_mtp = False
    model = load(str(args.model).encode(), params)
    if not model:
        raise RuntimeError("reference model load failed")
    try:
        vocab = vocab_size(get_vocab(model))
        if vocab != 248320:
            raise ValueError(f"wrong vocabulary: {vocab}")
        for path in args.inputs:
            data = json.loads(path.read_text())
            if data["reference_revision"] != "f74e2fa6a31cc33ab4ee4ae516641cfe62df3924":
                raise ValueError("wrong reference revision")
            ctx_params = context_defaults()
            ctx_params.n_ctx = 8192
            ctx_params.n_batch = 512
            ctx_params.n_ubatch = 512
            ctx_params.n_seq_max = 1
            ctx_params.n_threads = args.threads
            ctx_params.n_threads_batch = args.threads
            ctx_params.type_k = 1
            ctx_params.type_v = 1
            ctx_params.flash_attn_type = 0
            ctx_params.offload_kqv = False
            ctx_params.op_offload = False
            ctx = init(model, ctx_params)
            if not ctx:
                raise RuntimeError("reference context init failed")
            output = path.with_suffix(".f32")
            try:
                started = time.monotonic()

                def feed(tokens):
                    ids = (c.c_int32 * len(tokens))(*tokens)
                    code = decode(ctx, get_batch(ids, len(tokens)))
                    if code != 0:
                        raise RuntimeError(f"reference decode failed: {code}")

                prompt = data["ids"][:data["prompt_tokens"]]
                tail = data["ids"][data["prompt_tokens"]:]
                for start in range(0, len(prompt), 512):
                    feed(prompt[start:start + 512])
                with output.open("xb") as stream:
                    def save_logits():
                        ptr = logits(ctx, -1)
                        if not ptr:
                            raise RuntimeError("reference returned no logits")
                        stream.write(c.string_at(ptr, vocab * 4))
                    save_logits()
                    for token in tail:
                        feed([token])
                        save_logits()
                manifest = {
                    "backend": "llama.cpp-reference-only",
                    "revision": data["reference_revision"], "release": "b11706",
                    "release_archive_sha256": "f426ae6d20afeb6970f51cc182dedc3b21cf23ec278192da1af262de43be6115",
                    "library_sha256": digest(args.bin / "llama.dll"),
                    "exporter_sha256": digest(Path(__file__)),
                    "model_sha256": digest(args.model),
                    "input_sha256": digest(path), "logits_sha256": digest(output),
                    "kv_dtype": "f16", "ssm_dtype": "f32", "flash_attention": False,
                    "threads": args.threads, "prefill_chunk": 512,
                    "vocab": vocab, "rows": len(tail) + 1,
                    "wall_seconds": time.monotonic() - started,
                }
                output.with_suffix(".manifest.json").write_text(json.dumps(manifest, indent=2) + "\n")
                print(json.dumps(manifest), flush=True)
            finally:
                free_context(ctx)
    finally:
        free_model(model)
        bind("llama_backend_free", None, [])()
        dll_dir.close()


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--bin", type=Path, required=True)
    parser.add_argument("--model", type=Path, required=True)
    parser.add_argument("--threads", type=int, default=8)
    parser.add_argument("inputs", type=Path, nargs="+")
    run(parser.parse_args())
