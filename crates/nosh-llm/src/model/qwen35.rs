//! Text-only quantized Qwen3.5. Attention KV and recurrent state share one position.

mod ops;
#[cfg(test)]
mod tests;

use std::fs::File;
use std::io::BufReader;
use std::path::Path;
use std::sync::Arc;

use candle_core::quantized::{GgmlDType, QMatMul, QTensor, gguf_file};
use candle_core::{DType, Device, IndexOp, Module, Result, Tensor};

use super::attn::{AttnScratch, KvDtype, attention};
use super::llama::{LayerKv, LoadOptions, PrepackStats, Prof, prepack_weights};
use ops::{DeltaState, partial_rope, rms_norm};

#[derive(Debug, Clone)]
pub struct Qwen35Config {
    pub n_layer: usize,
    pub nextn_layers: usize,
    pub hidden: usize,
    pub ffn: usize,
    pub vocab: usize,
    pub n_head: usize,
    pub n_kv_head: usize,
    pub head_dim: usize,
    pub rotary_dim: usize,
    pub rope_theta: f32,
    pub rms_eps: f32,
    pub native_context: usize,
    pub conv_kernel: usize,
    pub state_dim: usize,
    pub key_heads: usize,
    pub value_heads: usize,
    pub value_dim: usize,
    pub recurrent: Vec<bool>,
}

fn metadata<'a>(ct: &'a gguf_file::Content, name: &str) -> Result<&'a gguf_file::Value> {
    ct.metadata
        .get(name)
        .ok_or_else(|| candle_core::Error::Msg(format!("GGUF metadata {name} missing")))
}

fn checked_product(values: &[usize]) -> Result<usize> {
    values.iter().try_fold(1usize, |product, value| {
        product
            .checked_mul(*value)
            .ok_or_else(|| candle_core::Error::Msg("Qwen35 dimension overflow".into()))
    })
}

impl Qwen35Config {
    pub fn from_gguf(ct: &gguf_file::Content) -> Result<Self> {
        if metadata(ct, "general.architecture")?.to_string()? != "qwen35" {
            candle_core::bail!("expected qwen35 GGUF architecture");
        }
        let u = |name: &str| -> Result<usize> {
            Ok(metadata(ct, &format!("qwen35.{name}"))?.to_u32()? as usize)
        };
        let f = |name: &str| -> Result<f32> { metadata(ct, &format!("qwen35.{name}"))?.to_f32() };
        let total = u("block_count")?;
        let nextn_layers = match ct.metadata.get("qwen35.nextn_predict_layers") {
            Some(value) => value.to_u32()? as usize,
            None => 0,
        };
        let n_layer = total
            .checked_sub(nextn_layers)
            .filter(|n| *n > 0 && *n <= 256)
            .ok_or_else(|| candle_core::Error::Msg("malformed Qwen35 layer/NextN count".into()))?;
        if nextn_layers > 1 {
            candle_core::bail!("unsupported Qwen35 NextN layer count {nextn_layers}");
        }
        let hidden = u("embedding_length")?;
        let ffn = u("feed_forward_length")?;
        let n_head = u("attention.head_count")?;
        let n_kv_head = u("attention.head_count_kv")?;
        let head_dim = u("attention.key_length")?;
        let value_length = u("attention.value_length")?;
        let rotary_dim = u("rope.dimension_count")?;
        let conv_kernel = u("ssm.conv_kernel")?;
        let state_dim = u("ssm.state_size")?;
        let key_heads = u("ssm.group_count")?;
        let value_heads = u("ssm.time_step_rank")?;
        let inner = u("ssm.inner_size")?;
        let interval = u("full_attention_interval")?;
        let native_context = u("context_length")?;
        let rope_theta = f("rope.freq_base")?;
        let rms_eps = f("attention.layer_norm_rms_epsilon")?;
        if [
            hidden,
            ffn,
            n_head,
            n_kv_head,
            head_dim,
            rotary_dim,
            conv_kernel,
            state_dim,
            key_heads,
            value_heads,
            inner,
            interval,
            native_context,
        ]
        .contains(&0)
            || n_head % n_kv_head != 0
            || value_heads % key_heads != 0
            || inner % value_heads != 0
            || value_length != head_dim
            || rotary_dim % 2 != 0
            || rotary_dim > head_dim
            || !rope_theta.is_finite()
            || rope_theta <= 0.0
            || !rms_eps.is_finite()
            || rms_eps <= 0.0
        {
            candle_core::bail!("malformed Qwen35 attention/SSM dimensions or normalization");
        }
        let value_dim = inner / value_heads;
        // The shipped text model uses equal key/value head dimensions.
        if value_dim != state_dim || conv_kernel > 64 {
            candle_core::bail!("unsupported Qwen35 SSM head dimension or convolution kernel");
        }
        let sections = metadata(ct, "qwen35.rope.dimension_sections")?.to_vec()?;
        if sections.len() != 4 {
            candle_core::bail!("Qwen35 MRoPE needs four dimension sections");
        }
        let section_dims: Vec<_> = sections
            .iter()
            .map(|value| value.to_i32())
            .collect::<Result<_>>()?;
        if section_dims.iter().any(|value| *value < 0)
            || section_dims.iter().map(|v| i64::from(*v)).sum::<i64>() * 2 != rotary_dim as i64
        {
            candle_core::bail!("Qwen35 MRoPE sections do not match rotary dimension");
        }
        let vocab = metadata(ct, "tokenizer.ggml.tokens")?.to_vec()?.len();
        if vocab == 0 || vocab > u32::MAX as usize {
            candle_core::bail!("invalid Qwen35 vocabulary size {vocab}");
        }
        let recurrent = match ct.metadata.get("qwen35.attention.recurrent_layers") {
            Some(value) => {
                let values = value.to_vec()?;
                if values.len() != total {
                    candle_core::bail!("Qwen35 recurrent layer map has the wrong length");
                }
                let values = values
                    .iter()
                    .map(|value| value.to_bool())
                    .collect::<Result<Vec<_>>>()?;
                if values[n_layer..].iter().any(|value| *value) {
                    candle_core::bail!("Qwen35 NextN layer must not be recurrent");
                }
                values[..n_layer].to_vec()
            }
            None => (0..n_layer).map(|i| (i + 1) % interval != 0).collect(),
        };
        let cfg = Self {
            n_layer,
            nextn_layers,
            hidden,
            ffn,
            vocab,
            n_head,
            n_kv_head,
            head_dim,
            rotary_dim,
            rope_theta,
            rms_eps,
            native_context,
            conv_kernel,
            state_dim,
            key_heads,
            value_heads,
            value_dim,
            recurrent,
        };
        checked_product(&[cfg.vocab, cfg.hidden])?;
        checked_product(&[cfg.value_heads, cfg.state_dim, cfg.value_dim])?;
        cfg.tensor_shapes()?;
        Ok(cfg)
    }

    fn conv_channels(&self) -> Result<usize> {
        let key = checked_product(&[2, self.key_heads, self.state_dim])?;
        key.checked_add(checked_product(&[self.value_heads, self.value_dim])?)
            .ok_or_else(|| candle_core::Error::Msg("Qwen35 convolution dimension overflow".into()))
    }

    fn tensor_shapes(&self) -> Result<Vec<(String, Vec<usize>)>> {
        let mut shapes = vec![
            ("token_embd.weight".into(), vec![self.vocab, self.hidden]),
            ("output_norm.weight".into(), vec![self.hidden]),
        ];
        let value = checked_product(&[self.value_heads, self.value_dim])?;
        let conv = self.conv_channels()?;
        for i in 0..self.n_layer {
            let mut add = |name: &str, shape: &[usize]| {
                shapes.push((format!("blk.{i}.{name}"), shape.to_vec()));
            };
            add("attn_norm.weight", &[self.hidden]);
            add("post_attention_norm.weight", &[self.hidden]);
            add("ffn_gate.weight", &[self.ffn, self.hidden]);
            add("ffn_up.weight", &[self.ffn, self.hidden]);
            add("ffn_down.weight", &[self.hidden, self.ffn]);
            if self.recurrent[i] {
                add("attn_qkv.weight", &[conv, self.hidden]);
                add("attn_gate.weight", &[value, self.hidden]);
                add("ssm_conv1d.weight", &[conv, self.conv_kernel]);
                add("ssm_dt.bias", &[self.value_heads]);
                add("ssm_a", &[self.value_heads]);
                add("ssm_beta.weight", &[self.value_heads, self.hidden]);
                add("ssm_alpha.weight", &[self.value_heads, self.hidden]);
                add("ssm_norm.weight", &[self.value_dim]);
                add("ssm_out.weight", &[self.hidden, value]);
            } else {
                add(
                    "attn_q.weight",
                    &[
                        checked_product(&[2, self.n_head, self.head_dim])?,
                        self.hidden,
                    ],
                );
                add(
                    "attn_k.weight",
                    &[
                        checked_product(&[self.n_kv_head, self.head_dim])?,
                        self.hidden,
                    ],
                );
                add(
                    "attn_v.weight",
                    &[
                        checked_product(&[self.n_kv_head, self.head_dim])?,
                        self.hidden,
                    ],
                );
                add(
                    "attn_output.weight",
                    &[self.hidden, checked_product(&[self.n_head, self.head_dim])?],
                );
                add("attn_q_norm.weight", &[self.head_dim]);
                add("attn_k_norm.weight", &[self.head_dim]);
            }
        }
        Ok(shapes)
    }

    fn validate_tensors(&self, ct: &gguf_file::Content) -> Result<()> {
        let mut shapes = self.tensor_shapes()?;
        if ct.tensor_infos.contains_key("output.weight") {
            shapes.push(("output.weight".into(), vec![self.vocab, self.hidden]));
        }
        for (name, shape) in shapes {
            let info = ct
                .tensor_infos
                .get(&name)
                .ok_or_else(|| candle_core::Error::Msg(format!("Qwen35 tensor {name} missing")))?;
            if info.shape.dims() != shape {
                candle_core::bail!(
                    "Qwen35 tensor {name}: expected {shape:?}, got {:?}",
                    info.shape.dims()
                );
            }
            let dtype = info.ggml_dtype;
            let dense = matches!(dtype, GgmlDType::F32 | GgmlDType::F16 | GgmlDType::BF16);
            if !dense
                && (dtype != GgmlDType::Q8_0
                    || !name.ends_with(".weight")
                    || name.contains("norm.")
                    || name.contains("conv1d."))
            {
                candle_core::bail!("unsupported Qwen35 dtype {dtype:?} for {name}");
            }
            let elements = checked_product(&shape)?;
            if !elements.is_multiple_of(dtype.block_size())
                || (elements / dtype.block_size())
                    .checked_mul(dtype.type_size())
                    .is_none()
            {
                candle_core::bail!("malformed Qwen35 tensor storage size for {name}");
            }
        }
        Ok(())
    }

    pub fn cuda_memory_bytes(&self, weights: u64, context: usize, chunk: usize) -> Result<u64> {
        let overflow = || candle_core::Error::Msg("Qwen35 CUDA memory estimate overflow".into());
        let product = |values: &[u64]| {
            values
                .iter()
                .try_fold(1u64, |n, v| n.checked_mul(*v).ok_or_else(overflow))
        };
        let context = context.max(16) as u64;
        let chunk = (chunk.max(1) as u64).min(context);
        let dense = self.recurrent.iter().filter(|v| !**v).count() as u64;
        let linear = self.n_layer as u64 - dense;
        let kv = product(&[
            8,
            dense,
            self.n_kv_head as u64,
            self.head_dim as u64,
            context,
        ])?;
        let state = product(&[
            4,
            4,
            linear,
            self.value_heads as u64,
            self.state_dim as u64,
            self.value_dim as u64,
        ])?;
        let conv = product(&[
            8,
            linear,
            self.conv_channels()? as u64,
            self.conv_kernel as u64,
        ])?;
        let scores = product(&[12, self.n_head as u64, chunk, context])?;
        let activations = product(&[
            32,
            chunk,
            self.conv_channels()?.max(self.ffn).max(self.hidden) as u64,
        ])?;
        let output = product(&[4, self.vocab as u64])?;
        let tied = product(&[2, self.vocab as u64, self.hidden as u64])?;
        let rope = product(&[8, context, self.rotary_dim as u64])?;
        let sum = [
            weights,
            kv,
            state,
            conv,
            scores,
            activations,
            output,
            tied,
            rope,
            512 << 20,
        ]
        .into_iter()
        .try_fold(0u64, |a, b| a.checked_add(b).ok_or_else(overflow))?;
        sum.checked_add((sum / 10).max(512 << 20))
            .ok_or_else(overflow)
    }
}

pub fn cuda_memory_estimate(path: &Path, context: usize, chunk: usize) -> Result<u64> {
    let mut file = BufReader::new(File::open(path)?);
    let ct = gguf_file::Content::read(&mut file)?;
    let cfg = Qwen35Config::from_gguf(&ct)?;
    cfg.validate_tensors(&ct)?;
    let mut weights = file.get_ref().metadata()?.len();
    let enabled = |key| std::env::var(key).is_ok_and(|v| !v.is_empty() && v != "0");
    let bytes = if enabled("CANDLE_DEQUANTIZE_ALL") {
        Some(4u64)
    } else if enabled("CANDLE_DEQUANTIZE_ALL_F16") {
        Some(2u64)
    } else {
        None
    };
    if let Some(bytes) = bytes {
        let mut expanded = 0u64;
        for (_, shape) in cfg.tensor_shapes()? {
            let size = (checked_product(&shape)? as u64)
                .checked_mul(bytes)
                .ok_or_else(|| candle_core::Error::Msg("Qwen35 expanded weight overflow".into()))?;
            expanded = expanded
                .checked_add(size)
                .ok_or_else(|| candle_core::Error::Msg("Qwen35 expanded weight overflow".into()))?;
        }
        weights = weights.max(expanded);
    }
    cfg.cuda_memory_bytes(weights, context, chunk)
}

struct DenseAttention {
    q: QMatMul,
    k: QMatMul,
    v: QMatMul,
    output: QMatMul,
    q_norm: Tensor,
    k_norm: Tensor,
    kv: LayerKv,
}

struct LinearAttention {
    qkv: QMatMul,
    gate: QMatMul,
    alpha: QMatMul,
    beta: QMatMul,
    output: QMatMul,
    conv: Tensor,
    dt: Tensor,
    a: Tensor,
    norm: Tensor,
    state: DeltaState,
}

enum Attention {
    Dense(DenseAttention),
    Linear(LinearAttention),
}

struct Layer {
    norm: Tensor,
    post_norm: Tensor,
    gate: QMatMul,
    up: QMatMul,
    down: QMatMul,
    attention: Attention,
}

fn read_matrix(
    ct: &gguf_file::Content,
    file: &mut BufReader<File>,
    name: &str,
    opts: LoadOptions,
    device: &Device,
    prepack: &mut PrepackStats,
) -> Result<QMatMul> {
    let mut tensor = ct
        .tensor(file, name, device)
        .map_err(|error| error.context(format!("loading Qwen35 {name}")))?;
    if opts.prepack_weights && device.is_cpu() {
        prepack_weights(std::slice::from_mut(&mut tensor), prepack)?;
    }
    QMatMul::from_qtensor(tensor)
}

pub struct Qwen35 {
    cfg: Qwen35Config,
    embedding: Arc<QTensor>,
    output: QMatMul,
    norm: Tensor,
    layers: Vec<Layer>,
    pos: usize,
    max_context: usize,
    kv_dtype: KvDtype,
    device: Device,
    prepack: PrepackStats,
    scratch: AttnScratch,
}

impl Qwen35 {
    pub fn load(path: &Path, context: usize, opts: LoadOptions, device: &Device) -> Result<Self> {
        if !device.is_cpu() && !device.is_cuda() {
            candle_core::bail!("Qwen35 inference supports CPU or CUDA only");
        }
        if device.is_cuda() {
            opts.validate_cuda()?;
        }
        let mut file = BufReader::new(File::open(path)?);
        let ct = gguf_file::Content::read(&mut file)?;
        let cfg = Qwen35Config::from_gguf(&ct)?;
        cfg.validate_tensors(&ct)?;
        let max_context = context.max(16);
        if max_context > cfg.native_context {
            candle_core::bail!(
                "Qwen35 context {max_context} exceeds native context {}",
                cfg.native_context
            );
        }
        let mut prepack = PrepackStats::default();
        let mut read = |name: &str| {
            ct.tensor(&mut file, name, device)
                .map_err(|error| error.context(format!("loading Qwen35 {name}")))
        };
        let embedding = Arc::new(read("token_embd.weight")?);
        let norm = read("output_norm.weight")?.dequantize(device)?;
        let output = if device.is_cuda() && !ct.tensor_infos.contains_key("output.weight") {
            QMatMul::from_arc(Arc::clone(&embedding))?
        } else {
            // CPU output prepacking can release raw storage needed by embedding lookups.
            let name = if ct.tensor_infos.contains_key("output.weight") {
                "output.weight"
            } else {
                "token_embd.weight"
            };
            read_matrix(&ct, &mut file, name, opts, device, &mut prepack)?
        };
        let mut layers = Vec::with_capacity(cfg.n_layer);
        for i in 0..cfg.n_layer {
            let mut mat = |name: &str| {
                read_matrix(
                    &ct,
                    &mut file,
                    &format!("blk.{i}.{name}.weight"),
                    opts,
                    device,
                    &mut prepack,
                )
            };
            let gate = mat("ffn_gate")?;
            let up = mat("ffn_up")?;
            let down = mat("ffn_down")?;
            let attention = if cfg.recurrent[i] {
                Attention::Linear(LinearAttention {
                    qkv: mat("attn_qkv")?,
                    gate: mat("attn_gate")?,
                    alpha: mat("ssm_alpha")?,
                    beta: mat("ssm_beta")?,
                    output: mat("ssm_out")?,
                    conv: ct
                        .tensor(&mut file, &format!("blk.{i}.ssm_conv1d.weight"), device)?
                        .dequantize(device)?,
                    dt: ct
                        .tensor(&mut file, &format!("blk.{i}.ssm_dt.bias"), device)?
                        .dequantize(device)?,
                    a: ct
                        .tensor(&mut file, &format!("blk.{i}.ssm_a"), device)?
                        .dequantize(device)?,
                    norm: ct
                        .tensor(&mut file, &format!("blk.{i}.ssm_norm.weight"), device)?
                        .dequantize(device)?,
                    state: DeltaState::default(),
                })
            } else {
                Attention::Dense(DenseAttention {
                    q: mat("attn_q")?,
                    k: mat("attn_k")?,
                    v: mat("attn_v")?,
                    output: mat("attn_output")?,
                    q_norm: ct
                        .tensor(&mut file, &format!("blk.{i}.attn_q_norm.weight"), device)?
                        .dequantize(device)?,
                    k_norm: ct
                        .tensor(&mut file, &format!("blk.{i}.attn_k_norm.weight"), device)?
                        .dequantize(device)?,
                    kv: LayerKv::new(cfg.n_kv_head, cfg.head_dim, opts.kv_dtype, device),
                })
            };
            let norm = ct
                .tensor(&mut file, &format!("blk.{i}.attn_norm.weight"), device)?
                .dequantize(device)?;
            let post_norm = ct
                .tensor(
                    &mut file,
                    &format!("blk.{i}.post_attention_norm.weight"),
                    device,
                )?
                .dequantize(device)?;
            layers.push(Layer {
                norm,
                post_norm,
                gate,
                up,
                down,
                attention,
            });
        }
        Ok(Self {
            cfg,
            embedding,
            output,
            norm,
            layers,
            pos: 0,
            max_context,
            kv_dtype: opts.kv_dtype,
            device: device.clone(),
            prepack,
            scratch: AttnScratch::default(),
        })
    }

    pub fn config(&self) -> &Qwen35Config {
        &self.cfg
    }
    pub fn max_context(&self) -> usize {
        self.max_context
    }
    pub fn kv_len(&self) -> usize {
        self.pos
    }
    pub fn kv_dtype(&self) -> KvDtype {
        self.kv_dtype
    }
    pub fn prepack_stats(&self) -> PrepackStats {
        self.prepack
    }

    pub fn reset(&mut self) {
        self.pos = 0;
        for layer in &mut self.layers {
            match &mut layer.attention {
                Attention::Dense(attention) => attention.kv.truncate(0),
                Attention::Linear(attention) => attention.state = DeltaState::default(),
            }
        }
    }

    pub fn prepare_prefix(&mut self, len: usize) -> usize {
        if len != self.pos {
            self.reset();
        }
        self.pos
    }

    pub fn set_kv_dtype(&mut self, dtype: KvDtype) -> Result<()> {
        if self.device.is_cuda() {
            LoadOptions {
                kv_dtype: dtype,
                ..Default::default()
            }
            .validate_cuda()?;
        }
        self.reset();
        self.kv_dtype = dtype;
        for layer in &mut self.layers {
            if let Attention::Dense(attention) = &mut layer.attention {
                attention.kv =
                    LayerKv::new(self.cfg.n_kv_head, self.cfg.head_dim, dtype, &self.device);
            }
        }
        Ok(())
    }

    pub fn forward(&mut self, ids: &[u32]) -> Result<Tensor> {
        let device = self.device.clone();
        let result = device
            .with_context(|| self.forward_impl(ids))
            .and_then(|out| {
                device.synchronize()?;
                Ok(out)
            });
        if result.is_err() {
            self.reset();
        }
        result
    }

    fn forward_impl(&mut self, ids: &[u32]) -> Result<Tensor> {
        let cfg = &self.cfg;
        let s = ids.len();
        if s == 0 || s > self.max_context.saturating_sub(self.pos) {
            candle_core::bail!(
                "Qwen35 empty input or context overflow: {} + {s} > {}",
                self.pos,
                self.max_context
            );
        }
        if ids.iter().any(|id| *id as usize >= cfg.vocab) {
            candle_core::bail!("Qwen35 input token is outside the vocabulary");
        }
        let mut profile = Prof::new();
        let ids = Tensor::new(ids, &self.device)?;
        let mut x = self.embedding.embedding(&ids)?.to_dtype(DType::F32)?;
        profile.lap(0);
        for layer in &mut self.layers {
            let h = rms_norm(&x, &layer.norm, cfg.rms_eps)?;
            profile.lap(1);
            let y = match &mut layer.attention {
                Attention::Dense(a) => {
                    let qg =
                        a.q.forward(&h)?
                            .reshape((s, cfg.n_head, 2 * cfg.head_dim))?;
                    let q = rms_norm(
                        &qg.narrow(2, 0, cfg.head_dim)?.contiguous()?,
                        &a.q_norm,
                        cfg.rms_eps,
                    )?;
                    let gate = qg
                        .narrow(2, cfg.head_dim, cfg.head_dim)?
                        .contiguous()?
                        .reshape((1, s, cfg.n_head * cfg.head_dim))?;
                    let k = rms_norm(
                        &a.k.forward(&h)?.reshape((s, cfg.n_kv_head, cfg.head_dim))?,
                        &a.k_norm,
                        cfg.rms_eps,
                    )?;
                    let v = a.v.forward(&h)?;
                    profile.lap(2);
                    let q = partial_rope(&q, self.pos, cfg.rotary_dim, cfg.rope_theta)?;
                    let k = partial_rope(&k, self.pos, cfg.rotary_dim, cfg.rope_theta)?;
                    let heads = |t: Tensor, n| {
                        t.reshape((1, s, n, cfg.head_dim))?
                            .transpose(1, 2)?
                            .contiguous()
                    };
                    profile.lap(3);
                    let y = match &mut a.kv {
                        LayerKv::Cpu(kv) => {
                            let q = q.flatten_all()?.to_vec1::<f32>()?;
                            let k = k.flatten_all()?.to_vec1::<f32>()?;
                            let v = v.flatten_all()?.to_vec1::<f32>()?;
                            kv.append(&k, &v);
                            let y = attention(
                                &q,
                                kv,
                                s,
                                cfg.n_head,
                                cfg.n_kv_head,
                                cfg.head_dim,
                                self.pos,
                                &mut self.scratch,
                            );
                            Tensor::from_vec(y, (1, s, cfg.n_head * cfg.head_dim), &self.device)?
                        }
                        LayerKv::Cuda(kv) => kv.attention_chunked(
                            &heads(q, cfg.n_head)?,
                            &heads(k, cfg.n_kv_head)?,
                            &heads(v, cfg.n_kv_head)?,
                        )?,
                    };
                    profile.lap(4);
                    let y = y.mul(&candle_nn::ops::sigmoid(&gate)?)?.squeeze(0)?;
                    a.output.forward(&y)?
                }
                Attention::Linear(a) => {
                    let qkv = a.qkv.forward(&h)?;
                    let gate = a.gate.forward(&h)?;
                    let alpha = a.alpha.forward(&h)?;
                    let beta = a.beta.forward(&h)?;
                    profile.lap(2);
                    let y = a.state.forward(
                        &qkv, &gate, &alpha, &beta, &a.conv, &a.dt, &a.a, &a.norm, cfg,
                    )?;
                    profile.lap(4);
                    a.output.forward(&y)?
                }
            };
            x = (x + y)?;
            profile.lap(5);
            let h = rms_norm(&x, &layer.post_norm, cfg.rms_eps)?;
            profile.lap(6);
            let gate = layer.gate.forward(&h)?;
            let up = layer.up.forward(&h)?;
            profile.lap(7);
            let y = candle_nn::ops::silu(&gate)?.mul(&up)?;
            profile.lap(8);
            x = (x + layer.down.forward(&y)?)?;
            profile.lap(9);
        }
        let last = rms_norm(&x.i(s - 1)?.unsqueeze(0)?, &self.norm, cfg.rms_eps)?;
        let out = self
            .output
            .forward(&last)?
            .flatten_all()?
            .to_dtype(DType::F32)?;
        profile.lap(10);
        profile.report(s, self.pos);
        self.pos += s;
        Ok(out)
    }
}
