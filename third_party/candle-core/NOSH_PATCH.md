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

On x86 and aarch64, candle repacks quantized weights into tiles on first use and keeps the tiles in a
cache next to the raw blocks; the raw blocks stay the source of truth for the life of the
tensor. For MiniCPM5-2B Q4_K_M the 252 Q4K layer matrices take 915 MiB raw and 1,221 MiB as
tiles, and every Q4K matmul (all batch sizes) only reads the tiles, so 915 MiB of raw data
sat unused in memory (design §2.3). ARM's Q4Kx8 and Q6Kx8 layouts have the same size as
the original blocks, but upstream falls back to raw blocks for batches other than one
or a multiple of four. Supporting those tails removes the need for the raw copy.

## What the patch changes

All changes are marked `nosh patch`; `nosh.patch` is the diff against the rev above
(`git apply third_party/candle-core/nosh.patch` in a candle checkout re-applies it).

- `QTensor::prepack_and_release_storage(&mut self) -> Result<bool>` builds CPU tiles now
  and replaces the raw blocks with an empty buffer, only when packed kernels serve every m:
  - x86_64: `repack_x86::select` accepts the tensor for every batch size m: CPU storage on
  x86_64 with AVX2 or VNNI, a 2D shape with n % 16 == 0 and k % 256 == 0, and a dtype whose
  tiles also serve m == 1 (Q4K; Q8_0 unless the CPU only has AVX2). Q6K is never released
  on x86 (its m == 1 gemv reads the raw blocks).
  - aarch64: CPU storage, dotprod, a nonempty 2D Q4K/Q6K matrix with n % 8 == 0 and
  k % 256 == 0. `PackedKind` keeps the existing kernel for full four-row groups and
  sends the remaining rows through the existing one-row dotprod GEMV, using the same
  cached layout. i8mm remains the preferred full-tile kernel when available. Without
  dotprod, raw storage and upstream dispatch are unchanged (even if i8mm is available).
  - Otherwise it returns `Ok(false)` without changing anything. Repeated successful
  calls return `Ok(true)` without rebuilding the cache.
- `prepack_x86_and_release_storage` remains a compatibility entry point, delegating to
  the new API only on x86_64 and still returning `Ok(false)` on other architectures.
- On CPUs with AMX, Q4K matmuls with m >= 32 use a second tile layout; it is built at the
  same time, so no matmul ever needs the raw blocks.
- `PackedCache` gets a `released` flag and eager x86/aarch64 prepacking.
- After release, every path that reads raw blocks returns an error saying so instead of
  touching them: `dequantize`, `dequantize_f16`, `embedding`, `data`, matmuls with f16
  inputs and the raw-kernel fallbacks of f32/bf16 matmuls (unreachable, since `select`
  serves every m for the released dtypes).
- Upstream bug fix, unrelated to the prepack: in `src/quantized/dummy_metal.rs` (the
  Metal stub compiled without the `metal` feature), `QMetalStorage::quantize_onto`
  returned `Error::NotCompiledWithCudaSupport`; it now returns
  `NotCompiledWithMetalSupport` like the other Metal stubs. Worth sending upstream.

nosh calls the new method for Q4K layer matrices on x86, and Q4K/Q6K layer matrices plus
`output` on aarch64. `token_embd` always retains its original blocks. The
`LoadOptions::prepack_weights` option (`nosh debug gen --no-prepack`) disables eager
prepacking and release, not candle's lazy packing. See `crates/nosh-llm/src/model/llama.rs`.
`crates/nosh-llm/tests/prepack.rs` checks every m from 1 to 64, empty inputs and prefill
boundaries, with single/multiple k-blocks and platform-specific row alignments. It checks that
prepacked matmuls equal the lazily tiled ones bit for bit and candle's raw-weight kernels
within rounding, and that the raw-data paths fail. Raw references have odd row counts,
so neither x86 nor ARM can silently repack them. Supported CPUs must release storage;
the test does not silently skip unexpected failures to release. CI logs CPU features,
eligibility and errors on Linux x86_64, Linux aarch64 and Apple Silicon.

## Real-model acceptance

`.github/workflows/arm64-memory.yml` is manual only (`workflow_dispatch`). It downloads
and verifies the registry-pinned MiniCPM5-2B Q4_K_M plus tokenizer, caches the model store,
and runs release builds on Linux ARM with two inference threads and f16 KV:

- `cargo test --release --locked -p nosh-cli --test arm64_memory arm64_8k_rss_and_outputs -- --ignored --exact --nocapture --test-threads 1`
  launches `nosh debug gen` in separate processes with and without `--no-prepack`.
  Both actually prefill 8064-8127 tokens (including the template), with a non-four-row
  tail, and generate at most 64 tokens in an 8192-token context. GNU time records each
  process's lifetime peak RSS; the prepacked run must stay at or below 2.5 GiB and below
  the retained run. CLI VmHWM is also checked (its current `MB` label denotes MiB).
- `cargo test --release --locked -p nosh-llm --test real_model prepacked_weights_match_retained_weights -- --ignored --exact --nocapture --test-threads 1`
  compares retained/prepacked weights on the existing 3.3K sample and an 8065-token
  prompt, each with 48 teacher-forced tokens. All design section 13.2 distribution
  thresholds must pass; equal generated text alone is not sufficient.

`NOSH_MEMORY_ARTIFACTS` selects the report directory (default: `target/arm64-memory`).
The workflow uploads inputs, outputs, model/build provenance, RSS JSON and the existing
numerical test log even on failure; numerical metrics use the shared `Divergence`
display rather than a duplicate JSON reporter. Model tests remain ignored in ordinary CI; missing models, missing
statistics or an ARM CPU without dotprod fail the explicit acceptance run.

## Updating the pinned rev

1. Copy `candle-core/src` of the new rev over `src/` and apply `nosh.patch` (fix conflicts
   by hand).
2. Regenerate the manifest: `cargo package -p candle-core --no-verify --allow-dirty` in the
   candle checkout, take `Cargo.toml` from the `.crate`, drop the `[[example]]`,
   `[[test]]`, `[[bench]]` and dev-dependency sections, keep the header comment.
3. Update the rev in this file, in the file headers of `src/quantized/{mod,repack}.rs` and
   in the root `Cargo.toml`; run `cargo test -p nosh-llm --test prepack --test vendored_candle`.
   If upstream has fixed the Metal stub, drop that hunk.

## When to remove it

As soon as the candle rev nosh pins can drop the raw blocks after repacking (an upstream
option or an equivalent API; nosh intends to propose one), or nosh stops using candle's CPU
tiles. Then delete this directory, the `[patch]` entry and `crates/nosh-llm/tests/vendored_candle.rs`,
and call the upstream API from `llama.rs` instead.
