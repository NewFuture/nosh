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

/// Query tokens per work unit in prefill; a unit covers these tokens for all
/// heads that share one KV head, so every K/V row is loaded once per unit.
const TOKEN_BLOCK: usize = 8;

/// `e^x` for `x <= 0` (softmax after max subtraction); ~2e-6 relative error,
/// written so the compiler vectorizes it. `-inf` maps to exactly 0.
#[inline(always)]
fn exp_neg(x: f32) -> f32 {
    const LOG2E: f32 = std::f32::consts::LOG2_E;
    let xc = x.max(-87.0);
    let t = xc * LOG2E;
    let n = (t + 0.5).floor();
    let f = t - n;
    let p = 1.0
        + f * (std::f32::consts::LN_2
            + f * (0.240_226_5 + f * (0.055_504_11 + f * (0.009_618_129 + f * 0.001_333_355))));
    let r = p * f32::from_bits(((n as i32 + 127) << 23) as u32);
    if x < -87.0 { 0.0 } else { r }
}

#[inline(always)]
fn axpy(y: &mut [f32], a: f32, x: &[f32]) {
    for (yi, xi) in y.iter_mut().zip(x) {
        *yi += a * xi;
    }
}

/// Geometry shared by all work units of one attention call.
struct Plan {
    s: usize,
    n_head: usize,
    n_kv: usize,
    n_rep: usize,
    hd: usize,
    pos: usize,
    bt: usize,
    splits: usize,
    span: usize,
    rows: usize,
}

/// Single-threaded `dst = lhs · rhs` with explicit (row, column) strides.
#[allow(clippy::too_many_arguments)]
fn matmul(
    m: usize,
    n: usize,
    k: usize,
    dst: &mut [f32],
    dst_rs: usize,
    lhs: &[f32],
    lhs_rs: usize,
    lhs_cs: usize,
    rhs: &[f32],
    rhs_rs: usize,
    rhs_cs: usize,
) {
    if m == 0 || n == 0 || k == 0 {
        return;
    }
    debug_assert!(dst.len() >= (m - 1) * dst_rs + n);
    debug_assert!(lhs.len() > (m - 1) * lhs_rs + (k - 1) * lhs_cs);
    debug_assert!(rhs.len() > (k - 1) * rhs_rs + (n - 1) * rhs_cs);
    // SAFETY: the slices cover every element addressed by the given shapes
    // and strides (checked above in debug builds; guaranteed by callers).
    unsafe {
        gemm::gemm(
            m,
            n,
            k,
            dst.as_mut_ptr(),
            1,
            dst_rs as isize,
            false,
            lhs.as_ptr(),
            lhs_cs as isize,
            lhs_rs as isize,
            rhs.as_ptr(),
            rhs_cs as isize,
            rhs_rs as isize,
            0.0,
            1.0,
            false,
            false,
            false,
            gemm::Parallelism::None,
        );
    }
}

/// One unit: KV head `g`, query tokens `t0..t1`, keys `k0..` of split `c`.
/// Writes the unnormalized output rows to `acc` and `(max, sum)` to `ml`.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn attend_unit(
    p: &Plan,
    q: &[f32],
    kv: &KvStore,
    u: usize,
    qbuf: &mut Vec<f32>,
    scores: &mut Vec<f32>,
    acc: &mut [f32],
    ml: &mut [f32],
) {
    let (hd, n_rep) = (p.hd, p.n_rep);
    let c = u % p.splits;
    let g = (u / p.splits) % p.n_kv;
    let tb = u / p.splits / p.n_kv;
    let t0 = tb * p.bt;
    let t1 = (t0 + p.bt).min(p.s);
    let nrows = (t1 - t0) * n_rep;
    let k0 = c * p.span;
    let k_end = ((c + 1) * p.span).min(p.pos + t1);
    let width = k_end.saturating_sub(k0);
    let (m, l) = ml.split_at_mut(p.rows);
    m[..nrows].fill(f32::NEG_INFINITY);
    l[..nrows].fill(0.0);
    acc[..nrows * hd].fill(0.0);
    if width == 0 {
        return;
    }
    let scale = 1.0 / (hd as f32).sqrt();
    qbuf.clear();
    for t in t0..t1 {
        let base = (t * p.n_head + g * n_rep) * hd;
        qbuf.extend(q[base..base + n_rep * hd].iter().map(|x| x * scale));
    }
    // S = Q · Kᵀ, row-major `[nrows][width]`.
    scores.clear();
    scores.resize(nrows * width, 0.0);
    let kbase = k0 * kv.row + g * hd;
    matmul(
        nrows,
        width,
        hd,
        scores,
        width,
        qbuf,
        hd,
        1,
        &kv.k[kbase..],
        1,
        kv.row,
    );
    for r in 0..nrows {
        let t = t0 + r / n_rep;
        let row = &mut scores[r * width..(r + 1) * width];
        // Causal: token t sees keys 0..=pos+t.
        let visible = (p.pos + t + 1).saturating_sub(k0).min(width);
        row[visible..].fill(f32::NEG_INFINITY);
        if visible == 0 {
            continue;
        }
        let mx = row[..visible]
            .iter()
            .fold(f32::NEG_INFINITY, |a, &b| a.max(b));
        let mut sum = 0f32;
        for x in row.iter_mut() {
            *x = exp_neg(*x - mx);
            sum += *x;
        }
        m[r] = mx;
        l[r] = sum;
    }
    // O = P · V, row-major `[nrows][hd]`.
    matmul(
        nrows,
        hd,
        width,
        acc,
        hd,
        scores,
        width,
        1,
        &kv.v[kbase..],
        kv.row,
        1,
    );
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
#[allow(clippy::too_many_arguments)]
unsafe fn attend_unit_avx2(
    p: &Plan,
    q: &[f32],
    kv: &KvStore,
    u: usize,
    qbuf: &mut Vec<f32>,
    scores: &mut Vec<f32>,
    acc: &mut [f32],
    ml: &mut [f32],
) {
    attend_unit(p, q, kv, u, qbuf, scores, acc, ml)
}

#[allow(clippy::too_many_arguments)]
fn run_unit(
    p: &Plan,
    q: &[f32],
    kv: &KvStore,
    u: usize,
    qbuf: &mut Vec<f32>,
    scores: &mut Vec<f32>,
    acc: &mut [f32],
    ml: &mut [f32],
) {
    #[cfg(target_arch = "x86_64")]
    {
        use std::sync::OnceLock;
        static AVX2: OnceLock<bool> = OnceLock::new();
        if *AVX2.get_or_init(|| is_x86_feature_detected!("avx2") && is_x86_feature_detected!("fma"))
        {
            // SAFETY: the CPU supports AVX2 and FMA (checked above).
            unsafe { attend_unit_avx2(p, q, kv, u, qbuf, scores, acc, ml) };
            return;
        }
    }
    attend_unit(p, q, kv, u, qbuf, scores, acc, ml)
}

/// Causal GQA. `q` is `[s][n_head][hd]` for tokens at positions `pos..pos+s`
/// (already in `kv`); returns `[s][n_head][hd]`.
///
/// Work is split by KV head and blocks of query tokens; when that gives too
/// few units to occupy the pool (decode), the key range is split as well and
/// the partial softmax results are merged afterwards.
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
    let bt = if s >= 2 * TOKEN_BLOCK { TOKEN_BLOCK } else { 1 };
    let n_tb = s.div_ceil(bt);
    let total_keys = pos + s;
    let pool = candle_core::utils::barrier_pool();
    let threads = pool.n_workers() + 1;
    let base = n_kv * n_tb;
    let splits = if base >= 2 * threads {
        1
    } else {
        (2 * threads)
            .div_ceil(base)
            .min(total_keys.div_ceil(128))
            .max(1)
    };
    let plan = Plan {
        s,
        n_head,
        n_kv,
        n_rep,
        hd,
        pos,
        bt,
        splits,
        span: total_keys.div_ceil(splits),
        rows: bt * n_rep,
    };
    let units = base * splits;
    let mut part_o = vec![0f32; units * plan.rows * hd];
    let mut part_ml = vec![0f32; units * plan.rows * 2];
    let po = part_o.as_mut_ptr() as usize;
    let pml = part_ml.as_mut_ptr() as usize;
    let work = |range: std::ops::Range<usize>| {
        let mut qbuf = Vec::new();
        let mut scores = Vec::new();
        for u in range {
            // SAFETY: unit `u` owns the disjoint slices `[u * rows * hd ..][.. rows * hd]`
            // and `[u * rows * 2 ..][.. rows * 2]`, which outlive the pool call.
            let (acc, ml) = unsafe {
                (
                    std::slice::from_raw_parts_mut(
                        (po as *mut f32).add(u * plan.rows * hd),
                        plan.rows * hd,
                    ),
                    std::slice::from_raw_parts_mut(
                        (pml as *mut f32).add(u * plan.rows * 2),
                        plan.rows * 2,
                    ),
                )
            };
            run_unit(&plan, q, kv, u, &mut qbuf, &mut scores, acc, ml);
        }
    };
    if units * total_keys * plan.rows < 1 << 14 {
        work(0..units);
    } else {
        pool.execute_chunked(units, work);
    }
    let mut out = vec![0f32; s * n_head * hd];
    for tb in 0..n_tb {
        let t0 = tb * bt;
        let nrows = ((t0 + bt).min(s) - t0) * n_rep;
        for g in 0..n_kv {
            let u0 = (tb * n_kv + g) * splits;
            for r in 0..nrows {
                let (t, h) = (t0 + r / n_rep, g * n_rep + r % n_rep);
                let dst = &mut out[(t * n_head + h) * hd..(t * n_head + h + 1) * hd];
                let ml = |c: usize| {
                    let b = (u0 + c) * plan.rows * 2;
                    (part_ml[b + r], part_ml[b + plan.rows + r])
                };
                let m = (0..splits)
                    .filter(|&c| ml(c).1 > 0.0)
                    .map(|c| ml(c).0)
                    .fold(f32::NEG_INFINITY, f32::max);
                let mut sum = 0f32;
                for c in 0..splits {
                    let (mc, lc) = ml(c);
                    if lc == 0.0 {
                        continue;
                    }
                    let w = (mc - m).exp();
                    sum += lc * w;
                    let src = &part_o[((u0 + c) * plan.rows + r) * hd..][..hd];
                    axpy(dst, w, src);
                }
                let inv = 1.0 / sum;
                for x in dst.iter_mut() {
                    *x *= inv;
                }
            }
        }
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
        for (n_head, n_kv, hd) in [(4, 2, 32), (16, 2, 128), (8, 8, 20)] {
            for (pos, s) in [
                (0usize, 5usize),
                (7, 1),
                (3, 60),
                (0, 9),
                (1500, 1),
                (700, 3),
                (0, 130),
                (300, 37),
            ] {
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
                assert!(
                    err < 1e-4,
                    "heads={n_head}/{n_kv} hd={hd} pos={pos} s={s} err={err}"
                );
            }
        }
    }

    #[test]
    #[ignore = "benchmark; run with --release --ignored --nocapture"]
    fn attn_bench() {
        let (n_head, n_kv, hd) = (16, 2, 128);
        for (pos, s) in [(1536usize, 512usize), (2048, 1), (4096, 1)] {
            let total = pos + s;
            let k = pseudo(total * n_kv * hd, 1);
            let v = pseudo(total * n_kv * hd, 2);
            let q = pseudo(s * n_head * hd, 3);
            let mut kv = KvStore::new(n_kv, hd);
            kv.append(&k, &v);
            let _ = attention(&q, &kv, s, n_head, n_kv, hd, pos);
            let n = if s > 1 { 5 } else { 200 };
            let t = std::time::Instant::now();
            for _ in 0..n {
                std::hint::black_box(attention(&q, &kv, s, n_head, n_kv, hd, pos));
            }
            let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
            let flops = 4.0 * (s * n_head * hd) as f64 * (pos as f64 + s as f64 / 2.0);
            println!(
                "pos={pos} s={s}: {ms:.3} ms/layer, {:.1} GFLOP/s, threads={}",
                flops / ms / 1e6,
                candle_core::utils::barrier_pool().n_workers() + 1
            );
        }
    }

    #[test]
    fn fast_exp() {
        assert_eq!(exp_neg(f32::NEG_INFINITY), 0.0);
        assert_eq!(exp_neg(-100.0), 0.0);
        let mut x = -80.0f32;
        while x <= 0.0 {
            let (a, b) = (exp_neg(x), x.exp());
            assert!((a - b).abs() <= b * 1e-5 + 1e-37, "x={x} {a} vs {b}");
            x += 0.013;
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
