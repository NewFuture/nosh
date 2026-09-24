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
//! 4. Q4K layer matrices get their x86 tile layout while loading and drop their
//!    raw blocks (design §2.3, vendored candle patch); upstream keeps both.
//!
//! Attention (causal GQA, no repeated K/V) and RoPE run on raw rows on candle's
//! barrier pool; see [`super::attn`]. Only the last position's logits are
//! computed, and the caller drives chunked prefill.

use std::fs::File;
use std::path::Path;

use candle_core::quantized::{GgmlDType, QMatMul, QTensor, gguf_file};
use candle_core::{DType, Device, IndexOp, Module, Result, Tensor};

use super::attn::{AttnScratch, KvDtype, KvStore, attention, rope_interleaved};

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
    /// Build the x86 tile layout of Q4K layer matrices while loading and drop
    /// their raw blocks, where candle's tiles serve every batch size. The token
    /// embedding, lm_head and Q6K matrices keep their raw data either way.
    pub prepack_q4k: bool,
}

impl Default for LoadOptions {
    fn default() -> Self {
        Self {
            kv_dtype: KvDtype::F16,
            prepack_q4k: true,
        }
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
    kv: KvStore,
}

/// RoPE cos/sin tables `[position][head_dim / 2]`.
struct Rope {
    cos: Vec<f32>,
    sin: Vec<f32>,
    len: usize,
    theta: f32,
    head_dim: usize,
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
        }
    }

    fn ensure(&mut self, len: usize) {
        if len > self.len {
            let new_len = len.next_power_of_two().max(self.len * 2);
            let (cos, sin) = rope_tables(self.theta, self.head_dim, new_len);
            self.cos = cos;
            self.sin = sin;
            self.len = new_len;
        }
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

/// Prepacks the Q4K matrices among `ts`, one thread each (see
/// [`LoadOptions::prepack_q4k`]); other dtypes keep their raw data. It is
/// called once per layer with that layer's seven matrices and joins them
/// before returning, so at most seven threads run at a time.
fn prepack_q4k(ts: &mut [QTensor], stats: &mut PrepackStats) -> Result<()> {
    let t0 = std::time::Instant::now();
    let done: Vec<Result<Option<usize>>> = std::thread::scope(|s| {
        let jobs: Vec<_> = ts
            .iter_mut()
            .filter(|t| t.dtype() == GgmlDType::Q4K)
            .map(|t| {
                s.spawn(move || -> Result<Option<usize>> {
                    let bytes = t.storage_size_in_bytes();
                    Ok(t.prepack_x86_and_release_storage()?.then_some(bytes))
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

impl Llama {
    /// Loads a llama-architecture GGUF; `context_length` bounds the KV cache.
    pub fn load(
        path: &Path,
        context_length: usize,
        opts: LoadOptions,
        device: &Device,
    ) -> Result<Self> {
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
        let output = match tensor("output.weight") {
            Ok(t) => QMatMul::from_qtensor(t)?,
            Err(_) => QMatMul::from_qtensor(tensor("token_embd.weight")?)?,
        };
        let mut layers = Vec::with_capacity(n_layer);
        let mut prepack = PrepackStats::default();
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
            if opts.prepack_q4k {
                prepack_q4k(&mut m, &mut prepack)?;
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
                kv: KvStore::new(n_kv_head, head_dim, opts.kv_dtype),
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

    /// Switches the KV cache element type; drops everything cached.
    pub fn set_kv_dtype(&mut self, dtype: KvDtype) {
        self.pos = 0;
        let (n_kv, hd) = (self.cfg.n_kv_head, self.cfg.head_dim);
        for l in &mut self.layers {
            l.kv = KvStore::new(n_kv, hd, dtype);
        }
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
        device.with_context(|| self.forward_impl(ids))
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
        let ids_t = Tensor::new(ids, &self.device)?.unsqueeze(0)?;
        let mut x = self.tok_embd.embedding(&ids_t)?;
        prof.lap(0);
        let (n_head, n_kv, hd) = (self.cfg.n_head, self.cfg.n_kv_head, self.cfg.head_dim);
        for layer in &mut self.layers {
            let h = layer.attn_norm.forward(&x)?;
            prof.lap(1);
            let mut q: Vec<f32> = layer.wq.forward(&h)?.flatten_all()?.to_vec1()?;
            let mut k: Vec<f32> = layer.wk.forward(&h)?.flatten_all()?.to_vec1()?;
            let v: Vec<f32> = layer.wv.forward(&h)?.flatten_all()?.to_vec1()?;
            prof.lap(2);
            rope_interleaved(&mut q, s, n_head, hd, pos, &self.rope.cos, &self.rope.sin);
            rope_interleaved(&mut k, s, n_kv, hd, pos, &self.rope.cos, &self.rope.sin);
            layer.kv.append(&k, &v);
            prof.lap(3);
            let y = attention(&q, &layer.kv, s, n_head, n_kv, hd, pos, &mut self.scratch);
            let y = Tensor::from_vec(y, (1, s, n_head * hd), &self.device)?;
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
