//! Quantized llama for GGUF, forked from candle-transformers
//! `models/quantized_llama.rs` with the changes from design §7.1:
//!
//! 1. RoPE tables sized from `context_length` (default 8K) and grown on demand
//!    instead of a hard-coded `MAX_SEQ_LEN = 4096`.
//! 2. The token embedding stays quantized; rows are dequantized per lookup
//!    (`QTensor::embedding`) instead of materializing ~1 GB of f32.
//! 3. The KV cache is an append-only per-layer store grown in large steps, with
//!    `truncate` for prefix reuse, instead of `Tensor::cat` per token. It holds
//!    f16 by default (design §2.3).
//! 4. Layer matrices get their CPU tile layout while loading and drop their raw
//!    blocks: Q4K on x86, Q4K/Q6K (also output) on ARM with dotprod (design §2.3,
//!    vendored candle patch); upstream keeps both.
//!
//! CPU attention and RoPE run on raw rows on candle's barrier pool; see
//! [`super::attn`]. CUDA keeps weights, RoPE, activations and KV on device and
//! uses Candle tensor GQA. Only the last position's logits are computed, and
//! the caller drives chunked prefill.

use std::fs::File;
use std::path::Path;

use candle_core::quantized::{GgmlDType, QMatMul, QTensor, gguf_file};
use candle_core::{DType, Device, IndexOp, Module, Result, Tensor};

use super::attn::{AttnScratch, KvDtype, KvStore, attention, rope_interleaved};
use super::tensor_attn::TensorKv;

/// Hyper-parameters read from GGUF metadata.
#[derive(Debug, Clone)]
pub struct LlamaConfig {
    pub arch: String,
    pub file_type: Option<u32>,
    pub n_layer: usize,
    pub n_head: usize,
    pub n_kv_head: usize,
    pub head_dim: usize,
    pub hidden: usize,
    pub ffn: usize,
    pub vocab: usize,
    pub rope_theta: f32,
    pub rms_eps: f32,
    pub native_context: usize,
}

/// How [`Llama::load`] lays out weights and the KV cache.
#[derive(Debug, Clone, Copy)]
pub struct LoadOptions {
    pub kv_dtype: KvDtype,
    /// Build CPU tiles while loading and drop raw blocks where they serve every
    /// batch size: Q4K layers on x86, Q4K/Q6K layers and output on ARM with
    /// dotprod. The token embedding always keeps its raw data.
    pub prepack_weights: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            kv_dtype: KvDtype::F16,
            prepack_weights: true,
        }
    }
}

impl LoadOptions {
    pub(crate) fn validate_cuda(self) -> Result<()> {
        if self.kv_dtype != KvDtype::F16 {
            candle_core::bail!(
                "CUDA currently supports only f16 KV; f32 KV has not passed numerical validation (use --kv f16 or --device cpu)"
            );
        }
        Ok(())
    }
}

/// What prepacking did while loading.
#[derive(Debug, Clone, Copy, Default)]
pub struct PrepackStats {
    pub tensors: usize,
    /// Raw quantized bytes released.
    pub released_bytes: usize,
    pub secs: f64,
}

struct RmsNorm {
    weight: Tensor,
    eps: f32,
}

impl RmsNorm {
    fn forward(&self, x: &Tensor) -> Result<Tensor> {
        candle_nn::ops::rms_norm(x, &self.weight, self.eps)
    }
}

struct Layer {
    wq: QMatMul,
    wk: QMatMul,
    wv: QMatMul,
    wo: QMatMul,
    attn_norm: RmsNorm,
    w_gate: QMatMul,
    w_up: QMatMul,
    w_down: QMatMul,
    ffn_norm: RmsNorm,
    kv: LayerKv,
}

enum LayerKv {
    Cpu(KvStore),
    Cuda(TensorKv),
}

impl LayerKv {
    fn new(n_kv: usize, hd: usize, dtype: KvDtype, device: &Device) -> Self {
        if device.is_cuda() {
            Self::Cuda(TensorKv::new(dtype))
        } else {
            Self::Cpu(KvStore::new(n_kv, hd, dtype))
        }
    }

    fn dtype(&self) -> KvDtype {
        match self {
            Self::Cpu(kv) => kv.dtype(),
            Self::Cuda(kv) => kv.dtype(),
        }
    }

    fn truncate(&mut self, len: usize) {
        match self {
            Self::Cpu(kv) => kv.truncate(len),
            Self::Cuda(kv) => kv.truncate(len),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::Cpu(kv) => kv.bytes(),
            Self::Cuda(kv) => kv.bytes(),
        }
    }
}

/// RoPE cos/sin tables `[position][head_dim / 2]`.
struct Rope {
    cos: Vec<f32>,
    sin: Vec<f32>,
    len: usize,
    theta: f32,
    head_dim: usize,
    tensors: Option<(Tensor, Tensor)>,
}

impl Rope {
    fn new(theta: f32, head_dim: usize, len: usize) -> Self {
        let (cos, sin) = rope_tables(theta, head_dim, len);
        Self {
            cos,
            sin,
            len,
            theta,
            head_dim,
            tensors: None,
        }
    }

    fn ensure(&mut self, len: usize) {
        if len > self.len {
            let new_len = len.next_power_of_two().max(self.len * 2);
            let (cos, sin) = rope_tables(self.theta, self.head_dim, new_len);
            self.cos = cos;
            self.sin = sin;
            self.len = new_len;
            self.tensors = None;
        }
    }

    fn tensors(&mut self, pos: usize, len: usize, device: &Device) -> Result<(Tensor, Tensor)> {
        if self.tensors.is_none() {
            let shape = (self.len, self.head_dim / 2);
            self.tensors = Some((
                Tensor::from_slice(&self.cos, shape, device)?,
                Tensor::from_slice(&self.sin, shape, device)?,
            ));
        }
        let (cos, sin) = self.tensors.as_ref().unwrap();
        Ok((cos.narrow(0, pos, len)?, sin.narrow(0, pos, len)?))
    }
}

fn rope_tables(theta: f32, head_dim: usize, len: usize) -> (Vec<f32>, Vec<f32>) {
    let half = head_dim / 2;
    let inv: Vec<f64> = (0..half)
        .map(|i| 1.0 / (theta as f64).powf(2.0 * i as f64 / head_dim as f64))
        .collect();
    let mut cos = Vec::with_capacity(len * half);
    let mut sin = Vec::with_capacity(len * half);
    for p in 0..len {
        for f in &inv {
            let a = p as f64 * f;
            cos.push(a.cos() as f32);
            sin.push(a.sin() as f32);
        }
    }
    (cos, sin)
}

pub struct Llama {
    cfg: LlamaConfig,
    tok_embd: QTensor,
    layers: Vec<Layer>,
    norm: RmsNorm,
    output: QMatMul,
    rope: Rope,
    device: Device,
    max_context: usize,
    pos: usize,
    scratch: AttnScratch,
    prepack: PrepackStats,
}

fn md<'a>(ct: &'a gguf_file::Content, key: &str) -> Result<&'a gguf_file::Value> {
    ct.metadata
        .get(key)
        .ok_or_else(|| candle_core::Error::Msg(format!("GGUF metadata {key} missing")))
}

/// Prepacks eligible matrices among `ts`, one thread each (see
/// [`LoadOptions::prepack_weights`]); other dtypes keep their raw data. It is
/// called per layer with its seven matrices (and separately for ARM output),
/// joining them before returning, so at most seven threads run at a time.
fn prepack_weights(ts: &mut [QTensor], stats: &mut PrepackStats) -> Result<()> {
    let t0 = std::time::Instant::now();
    let done: Vec<Result<Option<usize>>> = std::thread::scope(|s| {
        let jobs: Vec<_> = ts
            .iter_mut()
            .filter(|t| {
                t.dtype() == GgmlDType::Q4K
                    || (cfg!(target_arch = "aarch64") && t.dtype() == GgmlDType::Q6K)
            })
            .map(|t| {
                s.spawn(move || -> Result<Option<usize>> {
                    let bytes = t.storage_size_in_bytes();
                    Ok(t.prepack_and_release_storage()?.then_some(bytes))
                })
            })
            .collect();
        jobs.into_iter()
            .map(|j| {
                j.join().unwrap_or_else(|_| {
                    Err(candle_core::Error::Msg("prepack thread panicked".into()))
                })
            })
            .collect()
    });
    for r in done {
        if let Some(bytes) = r? {
            stats.tensors += 1;
            stats.released_bytes += bytes;
        }
    }
    stats.secs += t0.elapsed().as_secs_f64();
    Ok(())
}

impl LlamaConfig {
    /// Conservative f16-KV budget, including non-FlashAttention prefill temporaries.
    pub fn cuda_memory_bytes(&self, weights: u64, context: usize, chunk: usize) -> Result<u64> {
        let overflow = || candle_core::Error::Msg("CUDA memory estimate overflow".into());
        let product = |factors: &[u64]| {
            factors
                .iter()
                .try_fold(1u64, |v, n| v.checked_mul(*n).ok_or_else(overflow))
        };
        let context = context.max(16) as u64;
        let chunk = (chunk.max(1) as u64).min(context);
        // Two copies allow for KV append/rewind and f32 attention reads.
        let kv = product(&[
            2,
            2,
            2,
            self.n_layer as u64,
            self.n_kv_head as u64,
            self.head_dim as u64,
            context,
        ])?;
        let scores = product(&[3, 4, self.n_head as u64, chunk, context])?;
        let hidden = product(&[8, 4, chunk, self.hidden as u64])?;
        let ffn = product(&[4, 4, chunk, self.ffn as u64])?;
        let rope = product(&[8, context, self.head_dim as u64])?;
        let logits = product(&[4, 4, self.vocab as u64])?;
        let subtotal = [weights, kv, scores, hidden, ffn, rope, logits, 512 << 20]
            .into_iter()
            .try_fold(0u64, |a, b| a.checked_add(b).ok_or_else(overflow))?;
        // Allocator/library overhead plus spare memory for other GPU users.
        subtotal
            .checked_add((subtotal / 10).max(512 << 20))
            .ok_or_else(overflow)
    }

    /// Reads and checks the hyper-parameters; malformed values (zero or
    /// non-dividing head counts, inconsistent sizes) are load errors, checked
    /// before anything divides by them.
    pub fn from_gguf(ct: &gguf_file::Content) -> Result<Self> {
        let arch = md(ct, "general.architecture")?.to_string()?.clone();
        if arch != "llama" {
            candle_core::bail!("unsupported GGUF architecture '{arch}' (expected llama)");
        }
        let u = |k: &str| -> Result<usize> { Ok(md(ct, k)?.to_u32()? as usize) };
        let n_head = u("llama.attention.head_count")?;
        let n_kv_head = u("llama.attention.head_count_kv")?;
        let n_layer = u("llama.block_count")?;
        let hidden = u("llama.embedding_length")?;
        let ffn = u("llama.feed_forward_length")?;
        let bad = |what: String| -> Result<Self> { candle_core::bail!("malformed GGUF: {what}") };
        if n_head == 0 || n_kv_head == 0 || n_head % n_kv_head != 0 {
            return bad(format!(
                "{n_head} attention heads and {n_kv_head} kv heads (both must be > 0, heads a multiple of kv heads)"
            ));
        }
        if n_layer == 0 || hidden == 0 || ffn == 0 {
            return bad(format!(
                "{n_layer} layers, embedding length {hidden}, feed-forward length {ffn}"
            ));
        }
        let head_dim = u("llama.rope.dimension_count").unwrap_or(hidden / n_head);
        if head_dim == 0 || head_dim % 2 != 0 || head_dim.checked_mul(n_head) != Some(hidden) {
            return bad(format!(
                "inconsistent attention shape: {n_head} heads x head_dim {head_dim} != hidden {hidden}"
            ));
        }
        let native_context = u("llama.context_length").unwrap_or(4096);
        let rms_eps = md(ct, "llama.attention.layer_norm_rms_epsilon")?.to_f32()?;
        let rope_theta = md(ct, "llama.rope.freq_base")
            .and_then(|v| v.to_f32())
            .unwrap_or(10_000.0);
        let file_type = md(ct, "general.file_type").and_then(|v| v.to_u32()).ok();
        let vocab = match u("llama.vocab_size") {
            Ok(v) => v,
            Err(_) => md(ct, "tokenizer.ggml.tokens")?.to_vec()?.len(),
        };
        Ok(Self {
            arch,
            file_type,
            n_layer,
            n_head,
            n_kv_head,
            head_dim,
            hidden,
            ffn,
            vocab,
            rope_theta,
            rms_eps,
            native_context,
        })
    }
}

/// Reads metadata only, not the model's tensors. Invalid models remain errors.
pub fn cuda_memory_estimate(path: &Path, context: usize, chunk: usize) -> Result<u64> {
    let mut file = File::open(path)?;
    let mut weights = file.metadata()?.len();
    let content = gguf_file::Content::read(&mut file)?;
    let cfg = LlamaConfig::from_gguf(&content)?;
    let enabled = |key| std::env::var(key).is_ok_and(|v| !v.is_empty() && v != "0");
    let expanded_bytes = if enabled("CANDLE_DEQUANTIZE_ALL") {
        Some(4u64)
    } else if enabled("CANDLE_DEQUANTIZE_ALL_F16") {
        Some(2u64)
    } else {
        None
    };
    if let Some(element_bytes) = expanded_bytes {
        let overflow = || candle_core::Error::Msg("CUDA expanded-weight estimate overflow".into());
        let expanded = content
            .tensor_infos
            .values()
            .try_fold(0u64, |sum, tensor| {
                let bytes = tensor.shape.dims().iter().try_fold(element_bytes, |n, d| {
                    n.checked_mul(*d as u64).ok_or_else(overflow)
                })?;
                sum.checked_add(bytes).ok_or_else(overflow)
            })?;
        weights = weights.max(expanded);
    }
    cfg.cuda_memory_bytes(weights, context, chunk)
}

impl Llama {
    /// Loads a llama-architecture GGUF; `context_length` bounds the KV cache.
    pub fn load(
        path: &Path,
        context_length: usize,
        opts: LoadOptions,
        device: &Device,
    ) -> Result<Self> {
        if !device.is_cpu() && !device.is_cuda() {
            candle_core::bail!("llama inference supports CPU or CUDA devices only");
        }
        if device.is_cuda() {
            opts.validate_cuda()?;
        }
        let mut file = File::open(path)?;
        let ct = gguf_file::Content::read(&mut file)?;
        let cfg = LlamaConfig::from_gguf(&ct)?;
        let (vocab, hidden, rms_eps) = (cfg.vocab, cfg.hidden, cfg.rms_eps);
        let (n_layer, n_kv_head, head_dim) = (cfg.n_layer, cfg.n_kv_head, cfg.head_dim);
        let rope_theta = cfg.rope_theta;
        let mut tensor = |name: &str| ct.tensor(&mut file, name, device);
        let tok_embd = tensor("token_embd.weight")?;
        if tok_embd.shape().dims2()? != (vocab, hidden) {
            candle_core::bail!(
                "token_embd shape {:?} does not match vocab {vocab} x {hidden}",
                tok_embd.shape()
            );
        }
        let norm = RmsNorm {
            weight: tensor("output_norm.weight")?.dequantize(device)?,
            eps: rms_eps,
        };
        let mut prepack = PrepackStats::default();
        let mut output = match tensor("output.weight") {
            Ok(t) => t,
            Err(_) => tensor("token_embd.weight")?,
        };
        if device.is_cpu() && cfg!(target_arch = "aarch64") && opts.prepack_weights {
            prepack_weights(std::slice::from_mut(&mut output), &mut prepack)?;
        }
        let output = QMatMul::from_qtensor(output)?;
        let mut layers = Vec::with_capacity(n_layer);
        for i in 0..n_layer {
            let p = format!("blk.{i}");
            let mut read = |n: &str| tensor(&format!("{p}.{n}.weight"));
            let mut m = [
                read("attn_q")?,
                read("attn_k")?,
                read("attn_v")?,
                read("attn_output")?,
                read("ffn_gate")?,
                read("ffn_up")?,
                read("ffn_down")?,
            ];
            if device.is_cpu() && opts.prepack_weights {
                prepack_weights(&mut m, &mut prepack)?;
            }
            let [wq, wk, wv, wo, w_gate, w_up, w_down] = m.map(QMatMul::from_qtensor);
            let (wq, wk, wv, wo) = (wq?, wk?, wv?, wo?);
            let (w_gate, w_up, w_down) = (w_gate?, w_up?, w_down?);
            let attn_norm = RmsNorm {
                weight: tensor(&format!("{p}.attn_norm.weight"))?.dequantize(device)?,
                eps: rms_eps,
            };
            let ffn_norm = RmsNorm {
                weight: tensor(&format!("{p}.ffn_norm.weight"))?.dequantize(device)?,
                eps: rms_eps,
            };
            layers.push(Layer {
                wq,
                wk,
                wv,
                wo,
                attn_norm,
                w_gate,
                w_up,
                w_down,
                ffn_norm,
                kv: LayerKv::new(n_kv_head, head_dim, opts.kv_dtype, device),
            });
        }
        let max_context = context_length.max(16);
        let rope = Rope::new(rope_theta, head_dim, max_context.min(8192));
        Ok(Self {
            cfg,
            tok_embd,
            layers,
            norm,
            output,
            rope,
            device: device.clone(),
            max_context,
            pos: 0,
            scratch: AttnScratch::default(),
            prepack,
        })
    }

    pub fn config(&self) -> &LlamaConfig {
        &self.cfg
    }

    pub fn prepack_stats(&self) -> PrepackStats {
        self.prepack
    }

    pub fn kv_dtype(&self) -> KvDtype {
        self.layers
            .first()
            .map_or(KvDtype::default(), |l| l.kv.dtype())
    }

    /// Switches the KV cache element type; drops everything cached on success.
    /// Unsupported CUDA f32 requests leave the existing cache intact.
    pub fn set_kv_dtype(&mut self, dtype: KvDtype) -> Result<()> {
        if self.device.is_cuda() {
            LoadOptions {
                kv_dtype: dtype,
                ..LoadOptions::default()
            }
            .validate_cuda()?;
        }
        self.pos = 0;
        let (n_kv, hd) = (self.cfg.n_kv_head, self.cfg.head_dim);
        for l in &mut self.layers {
            l.kv = LayerKv::new(n_kv, hd, dtype, &self.device);
        }
        Ok(())
    }

    pub fn max_context(&self) -> usize {
        self.max_context
    }

    /// Tokens currently held in the KV cache.
    pub fn kv_len(&self) -> usize {
        self.pos
    }

    /// Drops cached positions `len..`.
    pub fn truncate(&mut self, len: usize) {
        let len = len.min(self.pos);
        self.pos = len;
        for l in &mut self.layers {
            l.kv.truncate(len);
        }
    }

    /// Bytes currently reserved for KV caches (and the f32 prefill scratch).
    pub fn kv_bytes(&self) -> usize {
        self.layers.iter().map(|l| l.kv.bytes()).sum::<usize>() + self.scratch.bytes()
    }

    /// Runs `ids` at the current position and returns f32 logits for the last one.
    ///
    /// Runs inside candle's CPU context so the few rayon-parallel candle ops
    /// execute inline on a warm thread instead of waking global workers.
    pub fn forward(&mut self, ids: &[u32]) -> Result<Tensor> {
        let device = self.device.clone();
        let out = device.with_context(|| self.forward_impl(ids))?;
        // CUDA launches are asynchronous: include completion in prefill/decode timings.
        device.synchronize()?;
        Ok(out)
    }

    fn forward_impl(&mut self, ids: &[u32]) -> Result<Tensor> {
        let s = ids.len();
        if s == 0 {
            candle_core::bail!("forward called with no tokens");
        }
        let pos = self.pos;
        if pos + s > self.max_context {
            candle_core::bail!("context overflow: {} + {s} > {}", pos, self.max_context);
        }
        let mut prof = Prof::new();
        self.rope.ensure(pos + s);
        let rope = if self.device.is_cuda() {
            Some(self.rope.tensors(pos, s, &self.device)?)
        } else {
            None
        };
        let ids_t = Tensor::new(ids, &self.device)?.unsqueeze(0)?;
        let mut x = self.tok_embd.embedding(&ids_t)?;
        prof.lap(0);
        let (n_head, n_kv, hd) = (self.cfg.n_head, self.cfg.n_kv_head, self.cfg.head_dim);
        for layer in &mut self.layers {
            let h = layer.attn_norm.forward(&x)?;
            prof.lap(1);
            let q = layer.wq.forward(&h)?;
            let k = layer.wk.forward(&h)?;
            let v = layer.wv.forward(&h)?;
            prof.lap(2);
            let y = match &mut layer.kv {
                LayerKv::Cpu(kv) => {
                    let mut q = q.flatten_all()?.to_vec1::<f32>()?;
                    let mut k = k.flatten_all()?.to_vec1::<f32>()?;
                    let v = v.flatten_all()?.to_vec1::<f32>()?;
                    rope_interleaved(&mut q, s, n_head, hd, pos, &self.rope.cos, &self.rope.sin);
                    rope_interleaved(&mut k, s, n_kv, hd, pos, &self.rope.cos, &self.rope.sin);
                    kv.append(&k, &v);
                    prof.lap(3);
                    let y = attention(&q, kv, s, n_head, n_kv, hd, pos, &mut self.scratch);
                    Tensor::from_vec(y, (1, s, n_head * hd), &self.device)?
                }
                LayerKv::Cuda(kv) => {
                    let (cos, sin) = rope.as_ref().unwrap();
                    let heads =
                        |x: Tensor, n| x.reshape((1, s, n, hd))?.transpose(1, 2)?.contiguous();
                    let q = candle_nn::rotary_emb::rope_i(&heads(q, n_head)?, cos, sin)?;
                    let k = candle_nn::rotary_emb::rope_i(&heads(k, n_kv)?, cos, sin)?;
                    let v = heads(v, n_kv)?;
                    prof.lap(3);
                    kv.attention(&q, &k, &v)?
                }
            };
            prof.lap(4);
            x = (x + layer.wo.forward(&y)?)?;
            prof.lap(5);
            let h = layer.ffn_norm.forward(&x)?;
            prof.lap(6);
            let gate = layer.w_gate.forward(&h)?;
            let up = layer.w_up.forward(&h)?;
            prof.lap(7);
            let act = (candle_nn::ops::silu(&gate)? * up)?;
            prof.lap(8);
            x = (x + layer.w_down.forward(&act)?)?;
            prof.lap(9);
        }
        self.pos += s;
        let last = x.i((.., s - 1..s, ..))?;
        let last = self.norm.forward(&last)?;
        let out = self
            .output
            .forward(&last)?
            .flatten_all()?
            .to_dtype(DType::F32);
        prof.lap(10);
        prof.report(s, pos);
        out
    }
}

/// Opt-in per-op timing (`NOSH_PROFILE=1`), printed to stderr.
struct Prof {
    on: bool,
    t: std::time::Instant,
    acc: [f64; 11],
}

impl Prof {
    fn new() -> Self {
        static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
        Self {
            on: *ON.get_or_init(|| std::env::var_os("NOSH_PROFILE").is_some()),
            t: std::time::Instant::now(),
            acc: [0.0; 11],
        }
    }

    fn lap(&mut self, i: usize) {
        if self.on {
            let now = std::time::Instant::now();
            self.acc[i] += (now - self.t).as_secs_f64();
            self.t = now;
        }
    }

    fn report(&self, s: usize, pos: usize) {
        if !self.on || (s == 1 && !pos.is_multiple_of(32)) {
            return;
        }
        const NAMES: [&str; 11] = [
            "embed",
            "attn_norm",
            "qkv",
            "rope_kv",
            "attn",
            "wo",
            "ffn_norm",
            "gate_up",
            "silu",
            "down",
            "output",
        ];
        let total: f64 = self.acc.iter().sum();
        let parts: Vec<String> = NAMES
            .iter()
            .zip(self.acc)
            .map(|(n, v)| format!("{n} {:.1}", v * 1e3))
            .collect();
        eprintln!(
            "[profile s={s} pos={pos} total {:.1} ms: {}]",
            total * 1e3,
            parts.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuda_f32_is_rejected_without_allocating_a_device() {
        assert!(LoadOptions::default().validate_cuda().is_ok());
        let error = LoadOptions {
            kv_dtype: KvDtype::F32,
            ..LoadOptions::default()
        }
        .validate_cuda()
        .unwrap_err()
        .to_string();
        assert!(error.contains("only f16 KV"), "{error}");
    }

    #[test]
    #[cfg(feature = "cuda")]
    #[ignore = "requires an NVIDIA GPU"]
    fn cuda_rope_matches_cpu_at_nonzero_positions() -> Result<()> {
        let device = Device::new_cuda(0)?;
        let (s, heads, hd, pos) = (7, 24, 64, 1234);
        let mut rope = Rope::new(10_000.0, hd, 16);
        rope.ensure(pos + s);
        let mut expected: Vec<f32> = (0..s * heads * hd)
            .map(|i| (i as f32 * 0.17).sin())
            .collect();
        let input = Tensor::from_slice(&expected, (1, s, heads, hd), &device)?
            .transpose(1, 2)?
            .contiguous()?;
        rope_interleaved(&mut expected, s, heads, hd, pos, &rope.cos, &rope.sin);
        let (cos, sin) = rope.tensors(pos, s, &device)?;
        let actual = candle_nn::rotary_emb::rope_i(&input, &cos, &sin)?
            .transpose(1, 2)?
            .flatten_all()?
            .to_vec1::<f32>()?;
        let error = actual
            .iter()
            .zip(expected)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(error < 1e-6, "CUDA RoPE error {error}");
        Ok(())
    }

    fn gguf_bytes(meta: &[(&str, gguf_file::Value)]) -> Vec<u8> {
        let refs: Vec<(&str, &gguf_file::Value)> = meta.iter().map(|(k, v)| (*k, v)).collect();
        let mut buf = std::io::Cursor::new(Vec::new());
        gguf_file::write(&mut buf, &refs, &[]).unwrap();
        buf.into_inner()
    }

    fn header(heads: u32, kv: u32, head_dim: Option<u32>) -> Vec<u8> {
        use gguf_file::Value as V;
        let mut meta = vec![
            ("general.architecture", V::String("llama".into())),
            ("llama.attention.head_count", V::U32(heads)),
            ("llama.attention.head_count_kv", V::U32(kv)),
            ("llama.block_count", V::U32(2)),
            ("llama.embedding_length", V::U32(64)),
            ("llama.feed_forward_length", V::U32(128)),
            ("llama.attention.layer_norm_rms_epsilon", V::F32(1e-5)),
            ("llama.vocab_size", V::U32(10)),
        ];
        if let Some(d) = head_dim {
            meta.push(("llama.rope.dimension_count", V::U32(d)));
        }
        gguf_bytes(&meta)
    }

    #[test]
    fn malformed_head_counts_are_load_errors_not_panics() {
        for (heads, kv, dim) in [
            (0, 0, None),
            (0, 2, None),
            (4, 0, None),
            (4, 3, None),
            (3, 3, None),
            (8, 2, Some(16)),
        ] {
            let bytes = header(heads, kv, dim);
            let ct = gguf_file::Content::read(&mut std::io::Cursor::new(&bytes)).unwrap();
            let err = LlamaConfig::from_gguf(&ct).unwrap_err().to_string();
            assert!(
                err.contains("malformed GGUF"),
                "{heads}/{kv}/{dim:?}: {err}"
            );
        }
        let bytes = header(8, 2, None);
        let ct = gguf_file::Content::read(&mut std::io::Cursor::new(&bytes)).unwrap();
        let cfg = LlamaConfig::from_gguf(&ct).unwrap();
        assert_eq!((cfg.n_head, cfg.n_kv_head, cfg.head_dim), (8, 2, 8));
        // Through the loader (as with --model-path): an error, not a panic.
        let path = std::env::temp_dir().join(format!("nosh-bad-heads-{}.gguf", std::process::id()));
        std::fs::write(&path, header(0, 0, None)).unwrap();
        let r = Llama::load(&path, 1024, LoadOptions::default(), &Device::Cpu);
        let _ = std::fs::remove_file(&path);
        let err = r.err().expect("load fails").to_string();
        assert!(err.contains("malformed GGUF"), "{err}");
    }

    #[test]
    fn cuda_budget_covers_context_workspaces_and_overflow() {
        let bytes = header(8, 2, None);
        let content = gguf_file::Content::read(&mut std::io::Cursor::new(&bytes)).unwrap();
        let mut cfg = LlamaConfig::from_gguf(&content).unwrap();
        cfg.n_layer = 42;
        cfg.n_head = 24;
        cfg.n_kv_head = 2;
        cfg.head_dim = 64;
        cfg.hidden = 1536;
        cfg.ffn = 8960;
        let weights = 1_561_318_368;
        let need = cfg.cuda_memory_bytes(weights, 8192, 512).unwrap();
        // Real RTX4090 measurements reached ~3.43 GiB; leave meaningful headroom.
        assert!(need >= 4 * (1 << 30));
        assert!(need < 6 * (1 << 30));
        assert!(cfg.cuda_memory_bytes(weights, 32768, 512).unwrap() > need);
        assert!(cfg.cuda_memory_bytes(weights, 8192, 256).unwrap() < need);
        assert!(cfg.cuda_memory_bytes(weights * 2, 8192, 512).unwrap() > need);
        assert!(cfg.cuda_memory_bytes(u64::MAX, 8192, 512).is_err());
        assert_eq!(
            cfg.cuda_memory_bytes(weights, 0, 0).unwrap(),
            cfg.cuda_memory_bytes(weights, 16, 1).unwrap()
        );
    }

    #[test]
    fn cuda_sizing_reports_model_errors_not_cpu_fallbacks() {
        let path =
            std::env::temp_dir().join(format!("nosh-bad-cuda-budget-{}.gguf", std::process::id()));
        std::fs::write(&path, header(0, 0, None)).unwrap();
        let error = cuda_memory_estimate(&path, 8192, 512)
            .unwrap_err()
            .to_string();
        assert!(error.contains("malformed GGUF"), "{error}");
        #[cfg(feature = "cuda")]
        assert!(
            crate::InferenceDevice::Auto
                .select(&path, 8192, 512, KvDtype::F16)
                .is_err()
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn prepacking_covers_layers_and_arm_output_but_not_embeddings() {
        use gguf_file::Value as V;
        let meta = [
            ("general.architecture", V::String("llama".into())),
            ("llama.attention.head_count", V::U32(2)),
            ("llama.attention.head_count_kv", V::U32(1)),
            ("llama.block_count", V::U32(1)),
            ("llama.embedding_length", V::U32(256)),
            ("llama.feed_forward_length", V::U32(256)),
            ("llama.attention.layer_norm_rms_epsilon", V::F32(1e-5)),
            ("llama.vocab_size", V::U32(16)),
        ];
        let matrix = |n, dtype| {
            let values: Vec<f32> = (0..n * 256).map(|i| (i % 31) as f32 / 31.0 - 0.5).collect();
            QTensor::quantize(
                &Tensor::from_vec(values, (n, 256), &Device::Cpu).unwrap(),
                dtype,
            )
            .unwrap()
        };
        for has_output in [true, false] {
            let mut tensors = vec![
                ("token_embd.weight", matrix(16, GgmlDType::Q4K)),
                ("blk.0.attn_q.weight", matrix(256, GgmlDType::Q4K)),
                ("blk.0.attn_k.weight", matrix(128, GgmlDType::Q4K)),
                ("blk.0.attn_v.weight", matrix(128, GgmlDType::Q6K)),
                ("blk.0.attn_output.weight", matrix(256, GgmlDType::Q4K)),
                ("blk.0.ffn_gate.weight", matrix(256, GgmlDType::Q4K)),
                ("blk.0.ffn_up.weight", matrix(256, GgmlDType::Q4K)),
                ("blk.0.ffn_down.weight", matrix(256, GgmlDType::Q6K)),
            ];
            if has_output {
                tensors.push(("output.weight", matrix(16, GgmlDType::Q6K)));
            }
            for name in [
                "output_norm.weight",
                "blk.0.attn_norm.weight",
                "blk.0.ffn_norm.weight",
            ] {
                let norm = Tensor::ones(256, DType::F32, &Device::Cpu).unwrap();
                tensors.push((name, QTensor::quantize(&norm, GgmlDType::F32).unwrap()));
            }
            let mut file = std::io::Cursor::new(Vec::new());
            let metadata: Vec<_> = meta.iter().map(|(k, v)| (*k, v)).collect();
            let weights: Vec<_> = tensors.iter().map(|(k, v)| (*k, v)).collect();
            gguf_file::write(&mut file, &metadata, &weights).unwrap();
            let path = std::env::temp_dir().join(format!(
                "nosh-prepack-{}-{has_output}.gguf",
                std::process::id()
            ));
            std::fs::write(&path, file.into_inner()).unwrap();

            let mut expected = PrepackStats::default();
            for (name, t) in &mut tensors {
                let layer = name.starts_with("blk.")
                    && (t.dtype() == GgmlDType::Q4K
                        || (cfg!(target_arch = "aarch64") && t.dtype() == GgmlDType::Q6K));
                let output = cfg!(target_arch = "aarch64")
                    && (*name == "output.weight" || (!has_output && *name == "token_embd.weight"));
                let bytes = t.storage_size_in_bytes();
                if (layer || output) && t.prepack_and_release_storage().unwrap() {
                    expected.tensors += 1;
                    expected.released_bytes += bytes;
                }
            }
            let mut plain = Llama::load(
                &path,
                64,
                LoadOptions {
                    prepack_weights: false,
                    ..LoadOptions::default()
                },
                &Device::Cpu,
            )
            .unwrap();
            assert_eq!(plain.prepack_stats().released_bytes, 0);
            let want = plain.forward(&[0, 1, 2]).unwrap().to_vec1::<f32>().unwrap();
            drop(plain);
            let mut packed = Llama::load(&path, 64, LoadOptions::default(), &Device::Cpu).unwrap();
            std::fs::remove_file(&path).unwrap();
            assert_eq!(packed.prepack_stats().tensors, expected.tensors);
            assert_eq!(
                packed.prepack_stats().released_bytes,
                expected.released_bytes
            );
            assert!(!packed.tok_embd.data().unwrap().is_empty());
            let got = packed
                .forward(&[0, 1, 2])
                .unwrap()
                .to_vec1::<f32>()
                .unwrap();
            assert!(got.iter().all(|v| v.is_finite()));
            assert_eq!(got, want, "output.weight present: {has_output}");
        }
    }

    #[test]
    fn rope_table_values() {
        let (cos, sin) = rope_tables(10_000.0, 4, 3);
        assert_eq!(&cos[0..2], &[1.0, 1.0]);
        assert!((sin[2] - 1f32.sin()).abs() < 1e-6);
        assert!((sin[5] - (2.0f32 * 0.01).sin()).abs() < 1e-6);
    }

    #[test]
    fn rope_matches_candle_rope_i() {
        let dev = Device::Cpu;
        let (s, h, d, pos) = (3usize, 2usize, 8usize, 5usize);
        let (cos, sin) = rope_tables(10_000.0, d, pos + s);
        let x: Vec<f32> = (0..s * h * d).map(|i| (i as f32 * 0.37).sin()).collect();
        // candle layout (b, h, t, d)
        let xt = Tensor::from_vec(x.clone(), (1, s, h, d), &dev)
            .unwrap()
            .transpose(1, 2)
            .unwrap()
            .contiguous()
            .unwrap();
        let ct = Tensor::from_vec(cos.clone(), (pos + s, d / 2), &dev)
            .unwrap()
            .narrow(0, pos, s)
            .unwrap();
        let st = Tensor::from_vec(sin.clone(), (pos + s, d / 2), &dev)
            .unwrap()
            .narrow(0, pos, s)
            .unwrap();
        let want: Vec<f32> = candle_nn::rotary_emb::rope_i(&xt, &ct, &st)
            .unwrap()
            .transpose(1, 2)
            .unwrap()
            .flatten_all()
            .unwrap()
            .to_vec1()
            .unwrap();
        let mut got = x;
        rope_interleaved(&mut got, s, h, d, pos, &cos, &sin);
        let err = got
            .iter()
            .zip(&want)
            .map(|(a, b)| (a - b).abs())
            .fold(0f32, f32::max);
        assert!(err < 1e-5, "{err}");
    }
}
