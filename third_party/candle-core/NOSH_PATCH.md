# Vendored candle-core (nosh patch)

This directory is `candle-core` from [huggingface/candle](https://github.com/huggingface/candle)
at rev `9b1be4a321ef265f13d2c30be4f2037109c51d14` (crate version 0.11.0, the rev nosh pins),
plus a small patch (the prepack below and one upstream bug fix). The root `Cargo.toml` uses it through
`[patch."https://github.com/huggingface/candle"]`, so `candle-nn` from the same rev links
against it too. License: MIT OR Apache-2.0 (`LICENSE-MIT`, `LICENSE-APACHE`).

Vendored: `src/` unchanged apart from the patch, and the manifest `cargo package` generates
for that rev without the example, test and bench targets (their files are not included).
`third_party` is excluded from the workspace, so nosh's fmt, clippy and tests skip it.

## Why

On x86, candle repacks quantized weights into tiles on first use and keeps the tiles in a
cache next to the raw blocks; the raw blocks stay the source of truth for the life of the
tensor. For MiniCPM5-2B Q4_K_M the 252 Q4K layer matrices take 915 MiB raw and 1,221 MiB as
tiles, and every Q4K matmul (all batch sizes) only reads the tiles, so 915 MiB of raw data
sat unused in memory (design §2.3).

## What the patch changes

All changes are marked `nosh patch`; `nosh.patch` is the diff against the rev above
(`git apply third_party/candle-core/nosh.patch` in a candle checkout re-applies it).

- `QTensor::prepack_x86_and_release_storage(&mut self) -> Result<bool>` (the only new public
  API): builds the x86 tiles now and replaces the raw blocks with an empty buffer. It only
  acts when `repack_x86::select` accepts the tensor for every batch size m: CPU storage on
  x86_64 with AVX2 or VNNI, a 2D shape with n % 16 == 0 and k % 256 == 0, and a dtype whose
  tiles also serve m == 1 (Q4K; Q8_0 unless the CPU only has AVX2). Q6K is never released
  (its m == 1 gemv reads the raw blocks). Otherwise it returns `Ok(false)` and changes
  nothing (other CPUs and architectures keep the raw data).
- On CPUs with AMX, Q4K matmuls with m >= 32 use a second tile layout; it is built at the
  same time, so no matmul ever needs the raw blocks.
- `PackedCache` gets a `released` flag and an eager `x86_prepack`.
- After release, every path that reads raw blocks returns an error saying so instead of
  touching them: `dequantize`, `dequantize_f16`, `embedding`, `data`, matmuls with f16
  inputs and the raw-kernel fallbacks of f32/bf16 matmuls (unreachable, since `select`
  does not depend on m for these dtypes).
- Upstream bug fix, unrelated to the prepack: in `src/quantized/dummy_metal.rs` (the
  Metal stub compiled without the `metal` feature), `QMetalStorage::quantize_onto`
  returned `Error::NotCompiledWithCudaSupport`; it now returns
  `NotCompiledWithMetalSupport` like the other Metal stubs. Worth sending upstream.

nosh calls the new method for the Q4K layer matrices only (not `token_embd` or `output`),
see `crates/nosh-llm/src/model/llama.rs`; `crates/nosh-llm/tests/prepack.rs` checks that
prepacked matmuls equal the lazily tiled ones bit for bit and candle's raw-weight kernels
within rounding (max relative error 4e-7), and that the raw-data paths fail.

## Updating the pinned rev

1. Copy `candle-core/src` of the new rev over `src/` and apply `nosh.patch` (fix conflicts
   by hand; the logic is ~60 lines).
2. Regenerate the manifest: `cargo package -p candle-core --no-verify --allow-dirty` in the
   candle checkout, take `Cargo.toml` from the `.crate`, drop the `[[example]]`,
   `[[test]]`, `[[bench]]` and dev-dependency sections, keep the header comment.
3. Update the rev in this file, in the file headers of `src/quantized/{mod,repack}.rs` and
   in the root `Cargo.toml`; run `cargo test -p nosh-llm --test prepack --test vendored_candle`.
   If upstream has fixed the Metal stub, drop that hunk.

## When to remove it

As soon as the candle rev nosh pins can drop the raw blocks after repacking (an upstream
option or an equivalent API; nosh intends to propose one), or nosh stops using candle's x86
tiles. Then delete this directory, the `[patch]` entry and `crates/nosh-llm/tests/vendored_candle.rs`,
and call the upstream API from `llama.rs` instead.
