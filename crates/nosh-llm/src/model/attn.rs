//! Causal grouped-query attention over a plain KV store (f16 by default), run
//! on candle's CPU barrier pool (the same threads as the quantized matmuls, so
//! no second thread pool competes for cores). Also RoPE (interleaved) on raw rows.

use half::f16;
use half::slice::HalfFloatSliceExt;

/// Element type of the KV cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KvDtype {
    /// Half the memory of f32; attention widens blocks to f32 before the GEMMs.
    #[default]
    F16,
    F32,
}

#[derive(Debug)]
enum KvBuf {
    F16(Vec<f16>),
    F32(Vec<f32>),
}

impl KvBuf {
    fn new(dtype: KvDtype) -> Self {
        match dtype {
            KvDtype::F16 => Self::F16(Vec::new()),
            KvDtype::F32 => Self::F32(Vec::new()),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::F16(v) => v.len(),
            Self::F32(v) => v.len(),
        }
    }

    fn capacity(&self) -> usize {
        match self {
            Self::F16(v) => v.capacity(),
            Self::F32(v) => v.capacity(),
        }
    }

    fn truncate(&mut self, n: usize) {
        match self {
            Self::F16(v) => v.truncate(n),
            Self::F32(v) => v.truncate(n),
        }
    }

    fn reserve_exact(&mut self, additional: usize) {
        match self {
            Self::F16(v) => v.reserve_exact(additional),
            Self::F32(v) => v.reserve_exact(additional),
        }
    }

    fn extend(&mut self, x: &[f32]) {
        match self {
            Self::F16(v) => {
                let start = v.len();
                v.resize(start + x.len(), f16::ZERO);
                v[start..].convert_from_f32_slice(x);
            }
            Self::F32(v) => v.extend_from_slice(x),
        }
    }

    fn bytes(&self) -> usize {
        match self {
            Self::F16(v) => v.capacity() * std::mem::size_of::<f16>(),
            Self::F32(v) => v.capacity() * std::mem::size_of::<f32>(),
        }
    }
}

/// Per-layer KV store, layout `[token][kv_head][head_dim]` (append-friendly).
#[derive(Debug)]
pub struct KvStore {
    k: KvBuf,
    v: KvBuf,
    row: usize,
    len: usize,
}

/// Capacity grows in steps of this many tokens.
const GROW_TOKENS: usize = 1024;

impl KvStore {
    pub fn new(n_kv: usize, head_dim: usize, dtype: KvDtype) -> Self {
        Self {
            k: KvBuf::new(dtype),
            v: KvBuf::new(dtype),
            row: n_kv * head_dim,
            len: 0,
        }
    }

    pub fn dtype(&self) -> KvDtype {
        match self.k {
            KvBuf::F16(_) => KvDtype::F16,
            KvBuf::F32(_) => KvDtype::F32,
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
        self.k.extend(k);
        self.v.extend(v);
        self.len += k.len() / self.row;
    }

    pub fn bytes(&self) -> usize {
        self.k.bytes() + self.v.bytes()
    }
}

/// Reusable f32 copies of f16 K/V for prefill chunks (see [`attention`]).
#[derive(Debug, Default)]
pub struct AttnScratch {
    k: Vec<f32>,
    v: Vec<f32>,
}

impl AttnScratch {
    pub fn bytes(&self) -> usize {
        (self.k.capacity() + self.v.capacity()) * std::mem::size_of::<f32>()
    }
}

/// K/V rows `[token][kv_head][head_dim]` as one attention call reads them.
#[derive(Clone, Copy)]
enum KvView<'a> {
    F32 {
        k: &'a [f32],
        v: &'a [f32],
    },
    /// Widened to f32 inside each work unit; used when every key belongs to
    /// exactly one unit (a single query-token block, i.e. decode).
    F16 {
        k: &'a [f16],
        v: &'a [f16],
    },
}

/// `dst[..src.len()] = src` as f32, split across the barrier pool.
fn widen_into(src: &[f16], dst: &mut Vec<f32>) {
    const CHUNK: usize = 1 << 16;
    if dst.len() < src.len() {
        dst.resize(src.len(), 0.0);
    }
    let n = src.len().div_ceil(CHUNK);
    if n <= 1 {
        src.convert_to_f32_slice(&mut dst[..src.len()]);
        return;
    }
    let out = dst.as_mut_ptr() as usize;
    candle_core::utils::barrier_pool().execute_chunked(n, |range| {
        for c in range {
            let lo = c * CHUNK;
            let hi = (lo + CHUNK).min(src.len());
            // SAFETY: chunk `c` writes only `dst[lo..hi]`, disjoint from every
            // other chunk and inside `dst`, which outlives the pool call.
            let d = unsafe { std::slice::from_raw_parts_mut((out as *mut f32).add(lo), hi - lo) };
            src[lo..hi].convert_to_f32_slice(d);
        }
    });
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
    /// Elements per token in the KV rows (`n_kv * hd`).
    kv_row: usize,
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

/// Per-thread buffers of the work units.
#[derive(Default)]
struct UnitBufs {
    q: Vec<f32>,
    scores: Vec<f32>,
    k: Vec<f32>,
    v: Vec<f32>,
}

/// One unit: KV head `g`, query tokens `t0..t1`, keys `k0..` of split `c`.
/// Writes the unnormalized output rows to `acc` and `(max, sum)` to `ml`.
#[inline(always)]
fn attend_unit(
    p: &Plan,
    q: &[f32],
    kv: KvView,
    u: usize,
    b: &mut UnitBufs,
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
    b.q.clear();
    for t in t0..t1 {
        let base = (t * p.n_head + g * n_rep) * hd;
        b.q.extend(q[base..base + n_rep * hd].iter().map(|x| x * scale));
    }
    // Keys k0..k_end of KV head g as f32 rows, and the stride between keys.
    let kbase = k0 * p.kv_row + g * hd;
    let (keys, vals, stride): (&[f32], &[f32], usize) = match kv {
        KvView::F32 { k, v } => (&k[kbase..], &v[kbase..], p.kv_row),
        KvView::F16 { k, v } => {
            b.k.resize(width * hd, 0.0);
            b.v.resize(width * hd, 0.0);
            for j in 0..width {
                let src = kbase + j * p.kv_row;
                let dst = j * hd..(j + 1) * hd;
                k[src..src + hd].convert_to_f32_slice(&mut b.k[dst.clone()]);
                v[src..src + hd].convert_to_f32_slice(&mut b.v[dst]);
            }
            (&b.k[..], &b.v[..], hd)
        }
    };
    // S = Q · Kᵀ, row-major `[nrows][width]`.
    b.scores.clear();
    b.scores.resize(nrows * width, 0.0);
    matmul(
        nrows,
        width,
        hd,
        &mut b.scores,
        width,
        &b.q,
        hd,
        1,
        keys,
        1,
        stride,
    );
    for r in 0..nrows {
        let t = t0 + r / n_rep;
        let row = &mut b.scores[r * width..(r + 1) * width];
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
        nrows, hd, width, acc, hd, &b.scores, width, 1, vals, stride, 1,
    );
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2,fma")]
unsafe fn attend_unit_avx2(
    p: &Plan,
    q: &[f32],
    kv: KvView,
    u: usize,
    b: &mut UnitBufs,
    acc: &mut [f32],
    ml: &mut [f32],
) {
    attend_unit(p, q, kv, u, b, acc, ml)
}

fn run_unit(
    p: &Plan,
    q: &[f32],
    kv: KvView,
    u: usize,
    b: &mut UnitBufs,
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
            unsafe { attend_unit_avx2(p, q, kv, u, b, acc, ml) };
            return;
        }
    }
    attend_unit(p, q, kv, u, b, acc, ml)
}

/// Causal GQA. `q` is `[s][n_head][hd]` for tokens at positions `pos..pos+s`
/// (already in `kv`); returns `[s][n_head][hd]`.
///
/// Work is split by KV head and blocks of query tokens; when that gives too
/// few units to occupy the pool (decode), the key range is split as well and
/// the partial softmax results are merged afterwards.
///
/// An f16 cache is widened to f32 per unit when there is a single block of
/// query tokens (every key is read by one unit), and otherwise once for the
/// whole call into `scratch`, since all token blocks read the same keys.
#[allow(clippy::too_many_arguments)]
pub fn attention(
    q: &[f32],
    kv: &KvStore,
    s: usize,
    n_head: usize,
    n_kv: usize,
    hd: usize,
    pos: usize,
    scratch: &mut AttnScratch,
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
        kv_row: kv.row,
    };
    let used = total_keys * kv.row;
    let view = match (&kv.k, &kv.v) {
        (KvBuf::F32(k), KvBuf::F32(v)) => KvView::F32 {
            k: &k[..used],
            v: &v[..used],
        },
        (KvBuf::F16(k), KvBuf::F16(v)) if n_tb == 1 => KvView::F16 {
            k: &k[..used],
            v: &v[..used],
        },
        (KvBuf::F16(k), KvBuf::F16(v)) => {
            widen_into(&k[..used], &mut scratch.k);
            widen_into(&v[..used], &mut scratch.v);
            KvView::F32 {
                k: &scratch.k[..used],
                v: &scratch.v[..used],
            }
        }
        _ => unreachable!("K and V share one dtype"),
    };
    let units = base * splits;
    let mut part_o = vec![0f32; units * plan.rows * hd];
    let mut part_ml = vec![0f32; units * plan.rows * 2];
    let po = part_o.as_mut_ptr() as usize;
    let pml = part_ml.as_mut_ptr() as usize;
    let work = |range: std::ops::Range<usize>| {
        let mut bufs = UnitBufs::default();
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
            run_unit(&plan, q, view, u, &mut bufs, acc, ml);
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

    fn round_f16(x: &[f32]) -> Vec<f32> {
        x.iter().map(|v| f16::from_f32(*v).to_f32()).collect()
    }

    #[test]
    fn matches_naive_causal_gqa() {
        // One scratch for all calls, so later (smaller) calls see a stale, larger one.
        let mut scratch = AttnScratch::default();
        for dtype in [KvDtype::F32, KvDtype::F16] {
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
                    let mut kv = KvStore::new(n_kv, hd, dtype);
                    kv.append(&k, &v);
                    let got = attention(&q, &kv, s, n_head, n_kv, hd, pos, &mut scratch);
                    let max_err = |want: &[f32]| {
                        got.iter()
                            .zip(want)
                            .map(|(a, b)| (a - b).abs())
                            .fold(0f32, f32::max)
                    };
                    let exact = naive(&q, &k, &v, s, n_head, n_kv, hd, pos);
                    let what = format!("{dtype:?} heads={n_head}/{n_kv} hd={hd} pos={pos} s={s}");
                    match dtype {
                        KvDtype::F32 => {
                            let err = max_err(&exact);
                            assert!(err < 1e-4, "{what} err={err}");
                        }
                        KvDtype::F16 => {
                            // Exact on the values the cache holds, close to the f32 result.
                            let (kh, vh) = (round_f16(&k), round_f16(&v));
                            let err = max_err(&naive(&q, &kh, &vh, s, n_head, n_kv, hd, pos));
                            assert!(err < 1e-4, "{what} err={err}");
                            let err = max_err(&exact);
                            assert!(err < 2e-3, "{what} vs f32 err={err}");
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "benchmark; run with --release --ignored --nocapture"]
    fn attn_bench() {
        let (n_head, n_kv, hd) = (16, 2, 128);
        let mut scratch = AttnScratch::default();
        for dtype in [KvDtype::F32, KvDtype::F16] {
            for (pos, s) in [
                (1536usize, 512usize),
                (7680, 512),
                (2048, 1),
                (4096, 1),
                (8000, 1),
            ] {
                let total = pos + s;
                let k = pseudo(total * n_kv * hd, 1);
                let v = pseudo(total * n_kv * hd, 2);
                let q = pseudo(s * n_head * hd, 3);
                let mut kv = KvStore::new(n_kv, hd, dtype);
                kv.append(&k, &v);
                let _ = attention(&q, &kv, s, n_head, n_kv, hd, pos, &mut scratch);
                let n = if s > 1 { 5 } else { 200 };
                let t = std::time::Instant::now();
                for _ in 0..n {
                    std::hint::black_box(attention(
                        &q,
                        &kv,
                        s,
                        n_head,
                        n_kv,
                        hd,
                        pos,
                        &mut scratch,
                    ));
                }
                let ms = t.elapsed().as_secs_f64() * 1000.0 / n as f64;
                let flops = 4.0 * (s * n_head * hd) as f64 * (pos as f64 + s as f64 / 2.0);
                println!(
                    "{dtype:?} pos={pos} s={s}: {ms:.3} ms/layer, {:.1} GFLOP/s, threads={}",
                    flops / ms / 1e6,
                    candle_core::utils::barrier_pool().n_workers() + 1
                );
            }
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
        for dtype in [KvDtype::F32, KvDtype::F16] {
            let mut kv = KvStore::new(2, 4, dtype);
            assert_eq!(kv.dtype(), dtype);
            kv.append(&[1.0; 16], &[2.0; 16]);
            assert_eq!(kv.len(), 2);
            kv.truncate(1);
            assert_eq!(kv.len(), 1);
            kv.append(&[3.0; 8], &[4.0; 8]);
            assert_eq!(kv.len(), 2);
            let tail: Vec<f32> = match &kv.k {
                KvBuf::F32(k) => k[8..].to_vec(),
                KvBuf::F16(k) => k[8..].iter().map(|x| x.to_f32()).collect(),
            };
            assert_eq!(tail, [3.0; 8]);
            let elem = if dtype == KvDtype::F16 { 2 } else { 4 };
            assert_eq!(kv.bytes(), 2 * GROW_TOKENS * 8 * elem);
        }
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
