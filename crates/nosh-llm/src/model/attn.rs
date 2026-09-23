//! Causal grouped-query attention over a plain `Vec<f32>` KV store, run on
//! candle's CPU barrier pool (the same threads as the quantized matmuls, so no
//! second thread pool competes for cores). Also RoPE (interleaved) on raw rows.

/// Per-layer KV store, layout `[token][kv_head][head_dim]` (append-friendly).
#[derive(Debug, Default)]
pub struct KvStore {
    k: Vec<f32>,
    v: Vec<f32>,
    row: usize,
    len: usize,
}

/// Capacity grows in steps of this many tokens.
const GROW_TOKENS: usize = 1024;

impl KvStore {
    pub fn new(n_kv: usize, head_dim: usize) -> Self {
        Self {
            row: n_kv * head_dim,
            ..Self::default()
        }
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn truncate(&mut self, len: usize) {
        self.len = self.len.min(len);
        self.k.truncate(self.len * self.row);
        self.v.truncate(self.len * self.row);
    }

    /// Appends `s` tokens given as `[s][kv_head][head_dim]` rows.
    pub fn append(&mut self, k: &[f32], v: &[f32]) {
        debug_assert_eq!(k.len(), v.len());
        debug_assert_eq!(k.len() % self.row, 0);
        let need = self.k.len() + k.len();
        if need > self.k.capacity() {
            let step = GROW_TOKENS * self.row;
            let target = need.div_ceil(step) * step;
            self.k.reserve_exact(target - self.k.len());
            self.v.reserve_exact(target - self.v.len());
        }
        self.k.extend_from_slice(k);
        self.v.extend_from_slice(v);
        self.len += k.len() / self.row;
    }

    pub fn bytes(&self) -> usize {
        (self.k.capacity() + self.v.capacity()) * std::mem::size_of::<f32>()
    }
}

/// Applies interleaved RoPE in place to rows `[s][n_heads][head_dim]`, the
/// row for token `t` being at absolute position `pos + t`. `cos`/`sin` are
/// `[position][head_dim / 2]` tables.
pub fn rope_interleaved(
    x: &mut [f32],
    s: usize,
    n_heads: usize,
    head_dim: usize,
    pos: usize,
    cos: &[f32],
    sin: &[f32],
) {
    let half = head_dim / 2;
    for t in 0..s {
        let c = &cos[(pos + t) * half..(pos + t + 1) * half];
        let sn = &sin[(pos + t) * half..(pos + t + 1) * half];
        for h in 0..n_heads {
            let base = (t * n_heads + h) * head_dim;
            let row = &mut x[base..base + head_dim];
            for i in 0..half {
                let (a, b) = (row[2 * i], row[2 * i + 1]);
                row[2 * i] = a * c[i] - b * sn[i];
                row[2 * i + 1] = a * sn[i] + b * c[i];
            }
        }
    }
}

#[inline(always)]
fn dot_generic(a: &[f32], b: &[f32]) -> f32 {
    let mut acc = [0f32; 16];
    let (ca, ra) = a.as_chunks::<16>();
    let (cb, rb) = b.as_chunks::<16>();
    for (x, y) in ca.iter().zip(cb) {
        for i in 0..16 {
            acc[i] += x[i] * y[i];
        }
    }
    let mut s: f32 = acc.iter().sum();
    for (x, y) in ra.iter().zip(rb) {
        s += x * y;
    }
    s
}

#[inline(always)]
fn axpy_generic(y: &mut [f32], a: f32, x: &[f32]) {
    for (yi, xi) in y.iter_mut().zip(x) {
        *yi += a * xi;
    }
}

/// Attention for one query row against keys `0..n_keys` of one KV head.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn attend_row_generic(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    row: usize,
    head_off: usize,
    n_keys: usize,
    scale: f32,
    scores: &mut Vec<f32>,
    out: &mut [f32],
) {
    let hd = q.len();
    scores.clear();
    let mut max = f32::NEG_INFINITY;
    for j in 0..n_keys {
        let kj = &k[j * row + head_off..j * row + head_off + hd];
        let sc = dot_generic(q, kj) * scale;
        max = max.max(sc);
        scores.push(sc);
    }
    let mut sum = 0f32;
    for s in scores.iter_mut() {
        *s = (*s - max).exp();
        sum += *s;
    }
    let inv = 1.0 / sum;
    out.fill(0.0);
    for (j, p) in scores.iter().enumerate() {
        let vj = &v[j * row + head_off..j * row + head_off + hd];
        axpy_generic(out, p * inv, vj);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
#[allow(clippy::too_many_arguments)]
unsafe fn attend_row_avx2(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    row: usize,
    head_off: usize,
    n_keys: usize,
    scale: f32,
    scores: &mut Vec<f32>,
    out: &mut [f32],
) {
    attend_row_generic(q, k, v, row, head_off, n_keys, scale, scores, out)
}

#[allow(clippy::too_many_arguments)]
fn attend_row(
    q: &[f32],
    k: &[f32],
    v: &[f32],
    row: usize,
    head_off: usize,
    n_keys: usize,
    scale: f32,
    scores: &mut Vec<f32>,
    out: &mut [f32],
) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static AVX2: OnceLock<bool> = OnceLock::new();
        if *AVX2.get_or_init(|| is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma"))
        {
            // SAFETY: the CPU supports AVX2 and FMA (checked above).
            unsafe { attend_row_avx2(q, k, v, row, head_off, n_keys, scale, scores, out) };
            return;
        }
    }
    attend_row_generic(q, k, v, row, head_off, n_keys, scale, scores, out)
}

/// Causal GQA. `q` is `[s][n_head][hd]` for tokens at positions `pos..pos+s`
/// (already in `kv`); returns `[s][n_head][hd]`.
pub fn attention(
    q: &[f32],
    kv: &KvStore,
    s: usize,
    n_head: usize,
    n_kv: usize,
    hd: usize,
    pos: usize,
) -> Vec<f32> {
    let n_rep = n_head / n_kv;
    let scale = 1.0 / (hd as f32).sqrt();
    let mut out = vec![0f32; s * n_head * hd];
    let units = s * n_head;
    let out_ptr = out.as_mut_ptr() as usize;
    let work = |range: std::ops::Range<usize>| {
        let mut scores = Vec::with_capacity(pos + s);
        for u in range {
            let (t, h) = (u / n_head, u % n_head);
            let g = h / n_rep;
            let qrow = &q[(t * n_head + h) * hd..(t * n_head + h + 1) * hd];
            // SAFETY: each unit writes only its own disjoint `[t][h]` row of `out`,
            // which outlives the pool call.
            let o = unsafe {
                std::slice::from_raw_parts_mut((out_ptr as *mut f32).add((t * n_head + h) * hd), hd)
            };
            attend_row(
                qrow,
                &kv.k,
                &kv.v,
                kv.row,
                g * hd,
                pos + t + 1,
                scale,
                &mut scores,
                o,
            );
        }
    };
    if units * (pos + s) < 4096 {
        work(0..units);
    } else {
        candle_core::utils::barrier_pool().execute_chunked(units, work);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn naive(
        q: &[f32],
        k: &[f32],
        v: &[f32],
        s: usize,
        n_head: usize,
        n_kv: usize,
        hd: usize,
        pos: usize,
    ) -> Vec<f32> {
        let n_rep = n_head / n_kv;
        let row = n_kv * hd;
        let mut out = vec![0f32; s * n_head * hd];
        for t in 0..s {
            for h in 0..n_head {
                let g = h / n_rep;
                let qr = &q[(t * n_head + h) * hd..][..hd];
                let n = pos + t + 1;
                let sc: Vec<f64> = (0..n)
                    .map(|j| {
                        let kr = &k[j * row + g * hd..][..hd];
                        qr.iter()
                            .zip(kr)
                            .map(|(a, b)| (*a as f64) * (*b as f64))
                            .sum::<f64>()
                            / (hd as f64).sqrt()
                    })
                    .collect();
                let m = sc.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
                let e: Vec<f64> = sc.iter().map(|x| (x - m).exp()).collect();
                let z: f64 = e.iter().sum();
                for d in 0..hd {
                    let val: f64 = (0..n)
                        .map(|j| e[j] / z * v[j * row + g * hd + d] as f64)
                        .sum();
                    out[(t * n_head + h) * hd + d] = val as f32;
                }
            }
        }
        out
    }

    fn pseudo(n: usize, seed: u32) -> Vec<f32> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x % 2000) as f32 / 1000.0 - 1.0
            })
            .collect()
    }

    #[test]
    fn matches_naive_causal_gqa() {
        let (n_head, n_kv, hd) = (4, 2, 32);
        for (pos, s) in [(0usize, 5usize), (7, 1), (3, 60)] {
            let total = pos + s;
            let k = pseudo(total * n_kv * hd, 1);
            let v = pseudo(total * n_kv * hd, 2);
            let q = pseudo(s * n_head * hd, 3);
            let mut kv = KvStore::new(n_kv, hd);
            kv.append(&k, &v);
            let got = attention(&q, &kv, s, n_head, n_kv, hd, pos);
            let want = naive(&q, &k, &v, s, n_head, n_kv, hd, pos);
            let err = got
                .iter()
                .zip(&want)
                .map(|(a, b)| (a - b).abs())
                .fold(0f32, f32::max);
            assert!(err < 1e-4, "pos={pos} s={s} err={err}");
        }
    }

    #[test]
    fn kv_store_append_truncate() {
        let mut kv = KvStore::new(2, 4);
        kv.append(&[1.0; 16], &[2.0; 16]);
        assert_eq!(kv.len(), 2);
        kv.truncate(1);
        assert_eq!(kv.len(), 1);
        kv.append(&[3.0; 8], &[4.0; 8]);
        assert_eq!(kv.len(), 2);
        assert_eq!(&kv.k[8..], &[3.0; 8]);
        assert!(kv.bytes() >= 2 * GROW_TOKENS * 8 * 4);
    }

    #[test]
    fn rope_rotates_pairs() {
        let mut x = vec![1.0, 0.0, 0.0, 1.0];
        let cos = vec![1.0, 1.0, 0.0, 1.0];
        let sin = vec![0.0, 0.0, 1.0, 0.0];
        rope_interleaved(&mut x, 1, 1, 4, 1, &cos, &sin);
        assert_eq!(x, vec![0.0, 1.0, 0.0, 1.0]);
    }
}
