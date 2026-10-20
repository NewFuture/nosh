use candle_core::{D, IndexOp, Result, Tensor};

use super::Qwen35Config;

fn cpu_delta(
    q: &Tensor,
    k: &Tensor,
    v: &Tensor,
    decay: &Tensor,
    beta: &Tensor,
    state: &Tensor,
) -> Result<(Tensor, Tensor)> {
    use std::sync::Mutex;
    let (s, heads, dim) = q.dims3()?;
    if k.dims() != q.dims()
        || v.dims() != q.dims()
        || decay.dims() != [s, heads]
        || beta.dims() != [s, heads]
        || state.dims() != [heads, dim, dim]
        || dim == 0
        || s == 0
        || heads == 0
    {
        candle_core::bail!("inconsistent CPU DeltaNet state/input dimensions");
    }
    let q = q.flatten_all()?.to_vec1::<f32>()?;
    let k = k.flatten_all()?.to_vec1::<f32>()?;
    let v = v.flatten_all()?.to_vec1::<f32>()?;
    let decay = decay.flatten_all()?.to_vec1::<f32>()?;
    let beta = beta.flatten_all()?.to_vec1::<f32>()?;
    let mut states = state.flatten_all()?.to_vec1::<f32>()?;
    struct Work<'a> {
        state: &'a mut [f32],
        output: Vec<f32>,
    }
    let work: Vec<_> = states
        .chunks_exact_mut(dim * dim)
        .map(|state| {
            Mutex::new(Work {
                state,
                output: vec![0.0; s * dim],
            })
        })
        .collect();
    candle_core::utils::barrier_pool().execute_chunked(heads, |range| {
        for h in range {
            // A poisoned worker is reported by into_inner below, before committing state.
            let Ok(mut work) = work[h].lock() else {
                continue;
            };
            let Work { state, output } = &mut *work;
            let mut delta = vec![0f32; dim];
            for t in 0..s {
                let offset = (t * heads + h) * dim;
                let q = &q[offset..offset + dim];
                let k = &k[offset..offset + dim];
                delta.copy_from_slice(&v[offset..offset + dim]);
                let decay = decay[t * heads + h];
                for (&key, row) in k.iter().zip(state.chunks_exact_mut(dim)) {
                    for (delta, value) in delta.iter_mut().zip(row) {
                        *value *= decay;
                        *delta -= key * *value;
                    }
                }
                for delta in &mut delta {
                    *delta *= beta[t * heads + h];
                }
                let out = &mut output[t * dim..(t + 1) * dim];
                for ((&key, &query), row) in k.iter().zip(q).zip(state.chunks_exact_mut(dim)) {
                    for ((value, delta), out) in row.iter_mut().zip(&delta).zip(out.iter_mut()) {
                        *value += key * *delta;
                        *out += query * *value;
                    }
                }
            }
        }
    });
    let mut output = vec![0f32; s * heads * dim];
    for (h, work) in work.into_iter().enumerate() {
        let work = work.into_inner().map_err(|_| {
            candle_core::Error::Msg("CPU DeltaNet worker state was poisoned".into())
        })?;
        for (t, row) in work.output.chunks_exact(dim).enumerate() {
            output[(t * heads + h) * dim..(t * heads + h + 1) * dim].copy_from_slice(row);
        }
    }
    let device = state.device();
    Ok((
        Tensor::from_vec(output, (s, heads, dim), device)?,
        Tensor::from_vec(states, (heads, dim, dim), device)?,
    ))
}

pub(super) fn tensor_delta(
    q: &Tensor,
    k: &Tensor,
    v: &Tensor,
    decay: &Tensor,
    beta: &Tensor,
    initial: &Tensor,
) -> Result<(Tensor, Tensor)> {
    let (s, heads, _) = q.dims3()?;
    let mut state = initial.clone();
    let mut output = Vec::with_capacity(s);
    for t in 0..s {
        state = state.broadcast_mul(&decay.i(t)?.reshape((heads, 1, 1))?)?;
        let kt = k.i(t)?.unsqueeze(1)?;
        let prediction = kt.matmul(&state)?.squeeze(1)?;
        let delta = (v.i(t)? - prediction)?.broadcast_mul(&beta.i(t)?.unsqueeze(1)?)?;
        state = (state + kt.transpose(1, 2)?.matmul(&delta.unsqueeze(1)?)?)?;
        output.push(q.i(t)?.unsqueeze(1)?.matmul(&state)?.squeeze(1)?);
    }
    Ok((Tensor::stack(&output, 0)?, state))
}

pub(super) fn rms_norm(x: &Tensor, weight: &Tensor, eps: f32) -> Result<Tensor> {
    candle_nn::ops::rms_norm(&x.contiguous()?, &weight.contiguous()?, eps)
}

pub(super) fn partial_rope(x: &Tensor, pos: usize, dim: usize, theta: f32) -> Result<Tensor> {
    let (s, _, hd) = x.dims3()?;
    let half = dim / 2;
    let mut cos = Vec::with_capacity(s * half);
    let mut sin = Vec::with_capacity(s * half);
    for p in pos..pos + s {
        for i in 0..half {
            let angle = p as f64 / f64::from(theta).powf(2.0 * i as f64 / dim as f64);
            cos.push(angle.cos() as f32);
            sin.push(angle.sin() as f32);
        }
    }
    let cos = Tensor::from_vec(cos, (s, 1, half), x.device())?;
    let sin = Tensor::from_vec(sin, (s, 1, half), x.device())?;
    let lo = x.narrow(2, 0, half)?;
    let hi = x.narrow(2, half, half)?;
    let left = (lo.broadcast_mul(&cos)? - hi.broadcast_mul(&sin)?)?;
    let right = (lo.broadcast_mul(&sin)? + hi.broadcast_mul(&cos)?)?;
    let tail = x.narrow(2, dim, hd - dim)?;
    Tensor::cat(&[&left, &right, &tail], 2)
}

#[derive(Default)]
pub(super) struct DeltaState {
    conv: Option<Tensor>,
    pub(super) recurrent: Option<Tensor>,
}

impl DeltaState {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn forward(
        &mut self,
        qkv: &Tensor,
        gate: &Tensor,
        alpha: &Tensor,
        beta: &Tensor,
        kernel: &Tensor,
        dt: &Tensor,
        a: &Tensor,
        norm: &Tensor,
        cfg: &Qwen35Config,
    ) -> Result<Tensor> {
        let (s, channels) = qkv.dims2()?;
        let device = qkv.device();
        let tail = match &self.conv {
            Some(tail) => tail.clone(),
            None => Tensor::zeros((cfg.conv_kernel - 1, channels), qkv.dtype(), device)?,
        };
        let input = Tensor::cat(&[&tail, qkv], 0)?;
        let mut convolved = Tensor::zeros((s, channels), qkv.dtype(), device)?;
        for i in 0..cfg.conv_kernel {
            let weight = kernel.narrow(1, i, 1)?.squeeze(1)?;
            let part = input.narrow(0, i, s)?.broadcast_mul(&weight)?;
            convolved = (convolved + part)?;
        }

        let convolved = candle_nn::ops::silu(&convolved)?;
        let key_dim = cfg.key_heads * cfg.state_dim;
        let value = cfg.value_heads * cfg.value_dim;
        let q = convolved
            .narrow(1, 0, key_dim)?
            .reshape((s, cfg.key_heads, cfg.state_dim))?;
        let k =
            convolved
                .narrow(1, key_dim, key_dim)?
                .reshape((s, cfg.key_heads, cfg.state_dim))?;
        let v = convolved.narrow(1, 2 * key_dim, value)?.reshape((
            s,
            cfg.value_heads,
            cfg.value_dim,
        ))?;
        let l2 = |x: &Tensor| -> Result<Tensor> {
            x.broadcast_div(&(x.sqr()?.sum_keepdim(D::Minus1)? + f64::from(cfg.rms_eps))?.sqrt()?)
        };
        let q = l2(&q)?;
        let k = l2(&k)?;
        let repeat = cfg.value_heads / cfg.key_heads;
        let repeat_heads = |x: Tensor| -> Result<Tensor> {
            if repeat == 1 {
                Ok(x)
            } else {
                x.unsqueeze(2)?
                    .expand((s, cfg.key_heads, repeat, cfg.state_dim))?
                    .contiguous()?
                    .reshape((s, cfg.value_heads, cfg.state_dim))
            }
        };
        let q = (repeat_heads(q)? / (cfg.state_dim as f64).sqrt())?;
        let k = repeat_heads(k)?;
        let alpha = alpha.broadcast_add(dt)?;
        let softplus = (alpha.relu()? + (alpha.abs()?.neg()?.exp()? + 1.0)?.log()?)?;
        let decay = softplus.broadcast_mul(a)?.exp()?;
        let beta = candle_nn::ops::sigmoid(beta)?;
        let mut state = match &self.recurrent {
            Some(state) => state.clone(),
            None => Tensor::zeros(
                (cfg.value_heads, cfg.state_dim, cfg.value_dim),
                qkv.dtype(),
                device,
            )?,
        };
        let output = if device.is_cpu() {
            let (output, updated) = cpu_delta(&q, &k, &v, &decay, &beta, &state)?;
            state = updated;
            output
        } else {
            let (output, updated) = tensor_delta(&q, &k, &v, &decay, &beta, &state)?;
            state = updated;
            output
        };
        let output = rms_norm(&output, norm, cfg.rms_eps)?
            .mul(&candle_nn::ops::silu(&gate.reshape((
                s,
                cfg.value_heads,
                cfg.value_dim,
            ))?)?)?
            .reshape((s, value))?;
        self.conv = Some(input.narrow(0, s, cfg.conv_kernel - 1)?.contiguous()?);
        self.recurrent = Some(state);
        Ok(output)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candle_core::{DType, Device};

    #[test]
    fn cpu_recurrence_matches_device_tensor_operations_at_real_head_width() -> Result<()> {
        for dim in [4, 128] {
            let (s, heads) = (17, 2);
            let values = |phase: f32| -> Result<Tensor> {
                let data = (0..s * heads * dim)
                    .map(|i| (i as f32 * 0.17 + phase).sin() / (dim as f32).sqrt())
                    .collect();
                Tensor::from_vec(data, (s, heads, dim), &Device::Cpu)
            };
            let (q, k, v) = (values(0.1)?, values(0.3)?, values(0.7)?);
            let decay = Tensor::full(0.9f32, (s, heads), &Device::Cpu)?;
            let beta = Tensor::full(0.4f32, (s, heads), &Device::Cpu)?;
            let state = Tensor::zeros((heads, dim, dim), DType::F32, &Device::Cpu)?;
            let (want, want_state) = tensor_delta(&q, &k, &v, &decay, &beta, &state)?;
            let (got, got_state) = cpu_delta(&q, &k, &v, &decay, &beta, &state)?;
            for (want, got) in [(want, got), (want_state, got_state)] {
                for (a, b) in want
                    .flatten_all()?
                    .to_vec1::<f32>()?
                    .iter()
                    .zip(got.flatten_all()?.to_vec1::<f32>()?)
                {
                    assert!((a - b).abs() <= 1e-5 + 1e-4 * a.abs(), "{a} != {b}");
                }
            }
        }
        Ok(())
    }
}
