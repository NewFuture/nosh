//! Device-resident GQA for CUDA, using Candle operators rather than host vectors.
//! The same path runs on CPU in small numerical tests.

use candle_core::{DType, Result, Tensor};

use super::attn::KvDtype;

pub struct TensorKv {
    data: Option<(Tensor, Tensor)>,
    len: usize,
    dtype: KvDtype,
}

impl TensorKv {
    pub fn new(dtype: KvDtype) -> Self {
        Self {
            data: None,
            len: 0,
            dtype,
        }
    }

    pub fn dtype(&self) -> KvDtype {
        self.dtype
    }

    pub fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
        if self.len == 0 {
            self.data = None;
        }
    }

    pub fn bytes(&self) -> usize {
        self.data.as_ref().map_or(0, |(k, v)| {
            (k.elem_count() + v.elem_count()) * k.dtype().size_in_bytes()
        })
    }

    /// Q/K/V are `[1, heads, tokens, head_dim]`, after interleaved RoPE.
    pub fn attention(&mut self, q: &Tensor, k: &Tensor, v: &Tensor) -> Result<Tensor> {
        let (_, n_head, s, hd) = q.dims4()?;
        let (_, n_kv, _, _) = k.dims4()?;
        let pos = self.len;
        let len = pos + s;
        let dtype = match self.dtype {
            KvDtype::F16 => DType::F16,
            KvDtype::F32 => DType::F32,
        };
        let (k, v) = (k.to_dtype(dtype)?, v.to_dtype(dtype)?);
        let (k, v) = match &self.data {
            Some((old_k, old_v)) => (
                Tensor::cat(&[&old_k.narrow(2, 0, pos)?, &k], 2)?,
                Tensor::cat(&[&old_v.narrow(2, 0, pos)?, &v], 2)?,
            ),
            None => (k, v),
        };
        // Group adjacent query heads under their KV head, without repeating K/V.
        // Widen f16 KV for f32 accumulation, matching the CPU cache semantics.
        let q = q.reshape((n_kv, n_head / n_kv * s, hd))?;
        let keys = k.to_dtype(DType::F32)?.reshape((n_kv, len, hd))?;
        let values = v.to_dtype(DType::F32)?.reshape((n_kv, len, hd))?;
        let scores = (q.matmul(&keys.t()?)? / (hd as f64).sqrt())?.reshape((n_head, s, len))?;
        let scores = if s > 1 {
            let mask: Vec<f32> = (0..s)
                .flat_map(|i| {
                    (0..len).map(move |j| if j > pos + i { f32::NEG_INFINITY } else { 0.0 })
                })
                .collect();
            scores.broadcast_add(&Tensor::from_vec(mask, (1, s, len), q.device())?)?
        } else {
            scores
        };
        let probabilities =
            candle_nn::ops::softmax_last_dim(&scores)?.reshape((n_kv, n_head / n_kv * s, len))?;
        let y = probabilities
            .matmul(&values)?
            .reshape((1, n_head, s, hd))?
            .transpose(1, 2)?
            .contiguous()?
            .reshape((1, s, n_head * hd))?;
        self.data = Some((k, v));
        self.len = len;
        Ok(y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::attn::{AttnScratch, KvStore, attention};
    use candle_core::Device;

    fn compare(
        device: &Device,
        n_head: usize,
        n_kv: usize,
        hd: usize,
        chunks: &[(usize, usize)],
    ) -> Result<()> {
        for dtype in [KvDtype::F16, KvDtype::F32] {
            let mut cpu = KvStore::new(n_kv, hd, dtype);
            let mut tensor = TensorKv::new(dtype);
            let mut scratch = AttnScratch::default();
            // Chunked prefill, decode, prefix rewind and overwrite.
            for &(pos, s) in chunks {
                cpu.truncate(pos);
                tensor.truncate(pos);
                let values = |n: usize, phase: f32| -> Vec<f32> {
                    (0..n)
                        .map(|i| ((i + pos * 17) as f32 * 0.13 + phase).sin())
                        .collect()
                };
                let q = values(s * n_head * hd, 0.1);
                let k = values(s * n_kv * hd, 0.7);
                let v = values(s * n_kv * hd, 1.1);
                cpu.append(&k, &v);
                let expected = attention(&q, &cpu, s, n_head, n_kv, hd, pos, &mut scratch);
                let t = |x: Vec<f32>, heads| {
                    Tensor::from_vec(x, (1, s, heads, hd), device)?
                        .transpose(1, 2)?
                        .contiguous()
                };
                let actual = tensor
                    .attention(&t(q, n_head)?, &t(k, n_kv)?, &t(v, n_kv)?)?
                    .flatten_all()?
                    .to_vec1::<f32>()?;
                let error = actual
                    .iter()
                    .zip(&expected)
                    .map(|(a, b)| (a - b).abs())
                    .fold(0f32, f32::max);
                assert!(error < 2e-5, "{dtype:?} pos={pos} s={s}: {error}");
            }
            tensor.truncate(0);
            assert_eq!(tensor.bytes(), 0);
        }
        Ok(())
    }

    #[test]
    fn tensor_gqa_matches_cpu_with_rewind() -> Result<()> {
        compare(
            &Device::Cpu,
            4,
            2,
            8,
            &[(0, 5), (5, 3), (8, 1), (3, 2), (0, 1)],
        )
    }

    #[test]
    #[cfg(feature = "cuda")]
    #[ignore = "requires an NVIDIA GPU"]
    fn cuda_gqa_matches_cpu_with_rewind() -> Result<()> {
        let device = Device::new_cuda(0)?;
        compare(&device, 4, 2, 8, &[(0, 5), (5, 3), (8, 1), (3, 2), (0, 1)])?;
        compare(
            &device,
            24,
            2,
            64,
            &[
                (0, 512),
                (512, 512),
                (1024, 188),
                (1212, 1),
                (1111, 3),
                (0, 1),
            ],
        )
    }
}
