use super::*;
use candle_core::quantized::{GgmlDType, gguf_file::Value as V};
use serde_json::Value;

fn metadata() -> Vec<(String, V)> {
    let source: Value =
        serde_json::from_str(include_str!("../../../tests/fixtures/qwen35_metadata.json")).unwrap();
    let keys = [
        ("block_count", "block_count"),
        ("nextn_predict_layers", "nextn_predict_layers"),
        ("embedding_length", "hidden"),
        ("feed_forward_length", "ffn"),
        ("attention.head_count", "heads"),
        ("attention.head_count_kv", "kv_heads"),
        ("attention.key_length", "head_dim"),
        ("attention.value_length", "head_dim"),
        ("rope.dimension_count", "rotary_dim"),
        ("context_length", "context"),
        ("ssm.conv_kernel", "conv_kernel"),
        ("ssm.state_size", "state_dim"),
        ("ssm.group_count", "key_heads"),
        ("ssm.time_step_rank", "value_heads"),
        ("ssm.inner_size", "inner_size"),
        ("full_attention_interval", "full_attention_interval"),
    ];
    let mut values = vec![("general.architecture".into(), V::String("qwen35".into()))];
    values.extend(keys.map(|(key, name)| {
        (
            format!("qwen35.{key}"),
            V::U32(source[name].as_u64().unwrap() as u32),
        )
    }));
    values.extend([
        (
            "qwen35.rope.freq_base".into(),
            V::F32(source["rope_theta"].as_f64().unwrap() as f32),
        ),
        (
            "qwen35.attention.layer_norm_rms_epsilon".into(),
            V::F32(source["rms_eps"].as_f64().unwrap() as f32),
        ),
        (
            "qwen35.rope.dimension_sections".into(),
            V::Array(vec![V::I32(11), V::I32(11), V::I32(10), V::I32(0)]),
        ),
        (
            "tokenizer.ggml.tokens".into(),
            V::Array((0..248320).map(|i| V::String(format!("t{i}"))).collect()),
        ),
    ]);
    values
}

fn content(meta: &[(String, V)]) -> gguf_file::Content {
    let mut bytes = std::io::Cursor::new(Vec::new());
    let refs: Vec<_> = meta
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    gguf_file::write(&mut bytes, &refs, &[]).unwrap();
    bytes.set_position(0);
    gguf_file::Content::read(&mut bytes).unwrap()
}

fn replace(meta: &mut [(String, V)], key: &str, value: V) {
    meta.iter_mut().find(|(name, _)| name == key).unwrap().1 = value;
}

#[test]
fn pinned_metadata_excludes_nextn_and_keeps_attention_dimensions() {
    let cfg = Qwen35Config::from_gguf(&content(&metadata())).unwrap();
    assert_eq!((cfg.n_layer, cfg.nextn_layers), (24, 1));
    assert_eq!(cfg.recurrent.iter().filter(|v| **v).count(), 18);
    assert_eq!((cfg.hidden, cfg.n_head * cfg.head_dim), (1024, 2048));
    assert_eq!(cfg.conv_channels().unwrap(), 6144);
    assert_eq!((cfg.vocab, cfg.rotary_dim), (248320, 64));
    assert!(
        !cfg.tensor_shapes()
            .unwrap()
            .iter()
            .any(|(name, _)| name.starts_with("blk.24."))
    );
}

#[test]
fn malformed_metadata_fails_before_loading_or_allocating_weights() {
    for (key, bad) in [
        ("qwen35.nextn_predict_layers", V::U32(25)),
        ("qwen35.attention.head_count_kv", V::U32(0)),
        ("qwen35.attention.head_count_kv", V::U32(3)),
        ("qwen35.ssm.time_step_rank", V::U32(0)),
        ("qwen35.ssm.inner_size", V::U32(2047)),
        ("qwen35.full_attention_interval", V::U32(0)),
        ("qwen35.rope.dimension_count", V::U32(65)),
        ("qwen35.rope.freq_base", V::F32(f32::NAN)),
        ("qwen35.attention.layer_norm_rms_epsilon", V::F32(-1.0)),
    ] {
        let mut meta = metadata();
        replace(&mut meta, key, bad);
        assert!(Qwen35Config::from_gguf(&content(&meta)).is_err(), "{key}");
    }
}

#[test]
fn budget_counts_hybrid_state_context_and_checked_overflow() {
    let cfg = Qwen35Config::from_gguf(&content(&metadata())).unwrap();
    assert!(cfg.cuda_memory_bytes(833592096, 8192, 512).unwrap() > 833592096);
    assert!(
        cfg.cuda_memory_bytes(833592096, 16384, 512).unwrap()
            > cfg.cuda_memory_bytes(833592096, 8192, 512).unwrap()
    );
    assert!(cfg.cuda_memory_bytes(u64::MAX, 8192, 512).is_err());
    assert!(cfg.cuda_memory_bytes(1, usize::MAX, usize::MAX).is_err());
}

fn close(actual: &[f32], expected: &[f32]) {
    assert_eq!(actual.len(), expected.len());
    for (i, (a, b)) in actual.iter().zip(expected).enumerate() {
        assert!(a.is_finite() && b.is_finite());
        assert!(
            (a - b).abs() <= 1e-5 + 1e-4 * b.abs(),
            "element {i}: {a} != {b}"
        );
    }
}

fn norm_compare(device: &Device) -> Result<()> {
    for width in [4, 128, 256, 1024] {
        let values: Vec<_> = (0..7 * width * 2)
            .map(|i| (i as f32 * 0.13).sin())
            .collect();
        let weights: Vec<_> = (0..width).map(|i| 1.5 + (i as f32 * 0.4).cos()).collect();
        let weight = Tensor::from_vec(weights.clone(), width, device)?;
        let base = Tensor::from_vec(values, (7, width * 2), device)?;
        for strided in [false, true] {
            let input = base.narrow(1, 0, width)?;
            let input = if strided { input } else { input.contiguous()? };
            let raw = input.flatten_all()?.to_vec1::<f32>()?;
            let mut expected = Vec::with_capacity(raw.len());
            for row in raw.chunks_exact(width) {
                let variance = row.iter().map(|value| value * value).sum::<f32>() / width as f32;
                let scale = (variance + 1e-6).sqrt();
                expected.extend(
                    row.iter()
                        .zip(&weights)
                        .map(|(value, weight)| value / scale * weight),
                );
            }
            let actual = rms_norm(&input, &weight, 1e-6)?
                .flatten_all()?
                .to_vec1::<f32>()?;
            close(&actual, &expected);
        }
    }
    Ok(())
}

#[test]
fn fused_norm_matches_scalar_for_contiguous_and_strided_inputs() -> Result<()> {
    norm_compare(&Device::Cpu)
}

#[test]
#[cfg(feature = "cuda")]
#[ignore = "requires a real NVIDIA CUDA device"]
fn cuda_fused_norm_matches_scalar_for_contiguous_and_strided_inputs() -> Result<()> {
    norm_compare(&Device::new_cuda(0)?)
}

#[test]
fn partial_rope_matches_scalar_and_preserves_unrotated_tail() -> Result<()> {
    let values: Vec<_> = (0..3 * 2 * 12).map(|i| (i as f32 * 0.3).sin()).collect();
    let input = Tensor::from_vec(values.clone(), (3, 2, 12), &Device::Cpu)?;
    let out = partial_rope(&input, 7, 8, 10000.0)?
        .flatten_all()?
        .to_vec1::<f32>()?;
    let mut want = values.clone();
    for t in 0..3 {
        for h in 0..2 {
            for i in 0..4 {
                let angle = (7 + t) as f64 / 10000f64.powf(2.0 * i as f64 / 8.0);
                let (sin, cos) = (angle.sin() as f32, angle.cos() as f32);
                let base = (t * 2 + h) * 12;
                want[base + i] = values[base + i] * cos - values[base + i + 4] * sin;
                want[base + i + 4] = values[base + i] * sin + values[base + i + 4] * cos;
            }
        }
    }
    close(&out, &want);
    Ok(())
}

fn small_config() -> Qwen35Config {
    Qwen35Config {
        n_layer: 4,
        nextn_layers: 1,
        hidden: 32,
        ffn: 64,
        vocab: 64,
        n_head: 2,
        n_kv_head: 1,
        head_dim: 16,
        rotary_dim: 8,
        rope_theta: 10000.0,
        rms_eps: 1e-6,
        native_context: 256,
        conv_kernel: 3,
        state_dim: 4,
        key_heads: 2,
        value_heads: 2,
        value_dim: 4,
        recurrent: vec![true, true, true, false],
    }
}

fn recurrence_reference(
    qkv: &[f32],
    kernel: &[f32],
    cfg: &Qwen35Config,
    s: usize,
) -> (Vec<f32>, Vec<f32>) {
    let channels = cfg.conv_channels().unwrap();
    let mut state = vec![0f32; cfg.value_heads * cfg.state_dim * cfg.value_dim];
    let mut out = Vec::new();
    for t in 0..s {
        let mut mixed = vec![0f32; channels];
        for (c, mixed) in mixed.iter_mut().enumerate() {
            for j in 0..cfg.conv_kernel {
                if t + j + 1 >= cfg.conv_kernel {
                    *mixed += qkv[(t + j + 1 - cfg.conv_kernel) * channels + c]
                        * kernel[c * cfg.conv_kernel + j];
                }
            }
            *mixed /= 1.0 + (-*mixed).exp();
        }
        for h in 0..cfg.value_heads {
            let d = cfg.state_dim;
            let mut q = mixed[h * d..(h + 1) * d].to_vec();
            let mut k = mixed[(cfg.key_heads + h) * d..(cfg.key_heads + h + 1) * d].to_vec();
            let v = &mixed[(2 * cfg.key_heads + h) * d..(2 * cfg.key_heads + h + 1) * d];
            let qnorm = (q.iter().map(|v| v * v).sum::<f32>() + cfg.rms_eps).sqrt();
            let knorm = (k.iter().map(|v| v * v).sum::<f32>() + cfg.rms_eps).sqrt();
            for q in &mut q {
                *q /= qnorm * (d as f32).sqrt();
            }
            for k in &mut k {
                *k /= knorm;
            }
            let offset = h * d * d;
            let state = &mut state[offset..offset + d * d];
            let decay = (-0.2f32 * (1.0 + 0.1f32.exp()).ln()).exp();
            for value in state.iter_mut() {
                *value *= decay;
            }
            let mut delta = v.to_vec();
            for j in 0..d {
                for i in 0..d {
                    delta[j] -= k[i] * state[i * d + j];
                }
                delta[j] *= 0.5;
            }
            for i in 0..d {
                for j in 0..d {
                    state[i * d + j] += k[i] * delta[j];
                }
            }
            let mut y = vec![0f32; d];
            for j in 0..d {
                for i in 0..d {
                    y[j] += q[i] * state[i * d + j];
                }
            }
            let rms = (y.iter().map(|v| v * v).sum::<f32>() / d as f32 + cfg.rms_eps).sqrt();
            out.extend(
                y.into_iter()
                    .map(|y| y / rms * (0.4 / (1.0 + (-0.4f32).exp()))),
            );
        }
    }
    (out, state)
}

fn delta_compare(device: &Device) -> Result<()> {
    let cfg = small_config();
    let s = 11;
    let channels = cfg.conv_channels()?;
    let values: Vec<_> = (0..s * channels)
        .map(|i| (i as f32 * 0.17).sin() * 0.5)
        .collect();
    let kernel: Vec<_> = (0..channels * cfg.conv_kernel)
        .map(|i| (i as f32 * 0.13).cos() * 0.2)
        .collect();
    let (want, state) = recurrence_reference(&values, &kernel, &cfg, s);
    let qkv = Tensor::from_vec(values, (s, channels), device)?;
    let kernel = Tensor::from_vec(kernel, (channels, cfg.conv_kernel), device)?;
    let gate = Tensor::full(0.4f32, (s, cfg.value_heads * cfg.value_dim), device)?;
    let alpha = Tensor::zeros((s, cfg.value_heads), DType::F32, device)?;
    let beta = alpha.clone();
    let dt = Tensor::full(0.1f32, cfg.value_heads, device)?;
    let a = Tensor::full(-0.2f32, cfg.value_heads, device)?;
    let norm = Tensor::ones(cfg.value_dim, DType::F32, device)?;
    for chunks in [vec![11], vec![1; 11], vec![3, 1, 7]] {
        let mut delta = DeltaState::default();
        let mut pos = 0;
        let mut actual = Vec::new();
        for len in chunks {
            actual.extend(
                delta
                    .forward(
                        &qkv.narrow(0, pos, len)?,
                        &gate.narrow(0, pos, len)?,
                        &alpha.narrow(0, pos, len)?,
                        &beta.narrow(0, pos, len)?,
                        &kernel,
                        &dt,
                        &a,
                        &norm,
                        &cfg,
                    )?
                    .flatten_all()?
                    .to_vec1::<f32>()?,
            );
            pos += len;
        }
        close(&actual, &want);
        close(
            &delta
                .recurrent
                .as_ref()
                .unwrap()
                .flatten_all()?
                .to_vec1::<f32>()?,
            &state,
        );
    }
    Ok(())
}

#[test]
fn recurrent_outputs_and_final_state_match_scalar_across_chunks() -> Result<()> {
    delta_compare(&Device::Cpu)
}

#[test]
#[cfg(feature = "cuda")]
#[ignore = "requires a real NVIDIA CUDA device"]
fn cuda_recurrent_outputs_and_state_match_scalar_across_chunks() -> Result<()> {
    delta_compare(&Device::new_cuda(0)?)
}

fn tiny_model(with_output: bool) -> Result<tempfile::NamedTempFile> {
    let cfg = small_config();
    let mut meta = metadata();
    for (name, value) in [
        ("block_count", 5),
        ("embedding_length", 32),
        ("feed_forward_length", 64),
        ("attention.head_count", 2),
        ("attention.head_count_kv", 1),
        ("attention.key_length", 16),
        ("attention.value_length", 16),
        ("rope.dimension_count", 8),
        ("context_length", 256),
        ("ssm.conv_kernel", 3),
        ("ssm.state_size", 4),
        ("ssm.group_count", 2),
        ("ssm.time_step_rank", 2),
        ("ssm.inner_size", 8),
    ] {
        replace(&mut meta, &format!("qwen35.{name}"), V::U32(value));
    }
    replace(
        &mut meta,
        "qwen35.rope.dimension_sections",
        V::Array(vec![V::I32(2), V::I32(1), V::I32(1), V::I32(0)]),
    );
    replace(&mut meta, "qwen35.rope.freq_base", V::F32(cfg.rope_theta));
    replace(
        &mut meta,
        "tokenizer.ggml.tokens",
        V::Array((0..64).map(|i| V::String(format!("t{i}"))).collect()),
    );
    let mut tensors = Vec::new();
    for (index, (name, dims)) in cfg.tensor_shapes()?.into_iter().enumerate() {
        let count = dims.iter().product();
        let values = if name.ends_with("ssm_a") {
            vec![-0.2; count]
        } else if dims.len() == 1 {
            vec![1.0; count]
        } else {
            (0..count)
                .map(|i| ((i + index * 31) as f32 * 0.17).sin() * 0.05)
                .collect()
        };
        let tensor = Tensor::from_vec(values, dims.clone(), &Device::Cpu)?;
        let dtype = if dims.len() == 2 && dims[1] % 32 == 0 {
            GgmlDType::Q8_0
        } else {
            GgmlDType::F32
        };
        tensors.push((name, QTensor::quantize(&tensor, dtype)?));
    }
    if with_output {
        let output = Tensor::full(0.125f32, (cfg.vocab, cfg.hidden), &Device::Cpu)?;
        tensors.push((
            "output.weight".into(),
            QTensor::quantize(&output, GgmlDType::Q8_0)?,
        ));
    }
    let refs: Vec<_> = meta
        .iter()
        .map(|(key, value)| (key.as_str(), value))
        .collect();
    let tensor_refs: Vec<_> = tensors
        .iter()
        .map(|(name, tensor)| (name.as_str(), tensor))
        .collect();
    let mut file = tempfile::NamedTempFile::new()?;
    gguf_file::write(&mut file, &refs, &tensor_refs)?;
    Ok(file)
}

#[test]
fn quantized_loader_decode_rewind_and_error_reset() -> Result<()> {
    let file = tiny_model(false)?;
    let mut model = Qwen35::load(file.path(), 64, LoadOptions::default(), &Device::Cpu)?;
    assert_eq!(model.config().n_layer, 4);
    let _ = model.forward(&[1, 2, 3])?;
    assert_eq!(model.prepare_prefix(3), 3);
    let want = model.forward(&[4])?.to_vec1::<f32>()?;
    assert_eq!(model.prepare_prefix(2), 0);
    let actual = model.forward(&[1, 2, 3, 4])?.to_vec1::<f32>()?;
    close(&actual, &want);
    assert!(model.forward(&[1000]).is_err());
    assert_eq!(model.kv_len(), 0);
    close(&model.forward(&[1, 2, 3, 4])?.to_vec1::<f32>()?, &want);
    assert!(model.forward(&[1; 64]).is_err());
    assert_eq!(model.kv_len(), 0);
    model.set_kv_dtype(KvDtype::F32)?;
    assert_eq!(model.kv_dtype(), KvDtype::F32);
    assert_eq!(model.forward(&[1])?.dims(), [64]);
    Ok(())
}

fn output_storage_compare(device: &Device) -> Result<()> {
    for with_output in [false, true] {
        let file = tiny_model(with_output)?;
        let model = Qwen35::load(file.path(), 64, LoadOptions::default(), device)?;
        let QMatMul::QTensor(output) = &model.output else {
            candle_core::bail!("expected quantized output without dequantization overrides");
        };
        assert_eq!(
            Arc::ptr_eq(&model.embedding, output),
            device.is_cuda() && !with_output,
            "tied storage sharing must not bypass explicit output or CPU prepacking"
        );
        assert!(!model.embedding.data()?.is_empty());
    }
    Ok(())
}

#[test]
fn cpu_keeps_embedding_raw_storage_independent_from_output() -> Result<()> {
    output_storage_compare(&Device::Cpu)
}

#[test]
#[cfg(feature = "cuda")]
#[ignore = "requires a real NVIDIA CUDA device"]
fn cuda_shares_only_tied_quantized_output_storage() -> Result<()> {
    output_storage_compare(&Device::new_cuda(0)?)
}
