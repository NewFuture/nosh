//! Token sampling: temperature / top-p / min-p, a lower temperature inside tool
//! calls, and a repetition penalty that switches on only once a loop is detected
//! (the same 16-gram ≥ 3 times within the last 256 tokens).

use std::collections::VecDeque;

use crate::engine::SamplingParams;

const REP_WINDOW: usize = 256;
const REP_NGRAM: usize = 16;
const REP_COUNT: usize = 3;

/// xoshiro256** seeded through SplitMix64 (deterministic across platforms).
#[derive(Debug, Clone)]
pub struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^ (z >> 31)
        };
        Self {
            s: [next(), next(), next(), next()],
        }
    }

    pub fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in [0, 1).
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }
}

pub struct Sampler {
    params: SamplingParams,
    rng: Rng,
    recent: VecDeque<u32>,
    penalty_on: bool,
}

impl Sampler {
    pub fn new(params: SamplingParams) -> Self {
        let seed = params.seed.unwrap_or_else(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x5eed)
        });
        Self {
            params,
            rng: Rng::new(seed),
            recent: VecDeque::with_capacity(REP_WINDOW + 1),
            penalty_on: false,
        }
    }

    pub fn penalty_active(&self) -> bool {
        self.penalty_on
    }

    /// Records an emitted token and updates repetition detection.
    pub fn observe(&mut self, id: u32) {
        self.recent.push_back(id);
        if self.recent.len() > REP_WINDOW {
            self.recent.pop_front();
        }
        if !self.penalty_on && detect_repetition(self.recent.make_contiguous()) {
            self.penalty_on = true;
        }
    }

    /// Samples the next token id from raw logits.
    pub fn sample(&mut self, logits: &mut [f32], in_tool_call: bool) -> u32 {
        if self.penalty_on && self.params.repetition_penalty > 1.0 {
            let p = self.params.repetition_penalty;
            let mut seen = std::collections::HashSet::new();
            for &id in &self.recent {
                if seen.insert(id)
                    && let Some(l) = logits.get_mut(id as usize)
                {
                    *l = if *l > 0.0 { *l / p } else { *l * p };
                }
            }
        }
        let temp = if in_tool_call {
            self.params.tool_call_temperature
        } else {
            self.params.temperature
        };
        if temp <= 1e-4 {
            return argmax(logits);
        }
        sample_top(
            logits,
            temp,
            self.params.top_p,
            self.params.min_p,
            &mut self.rng,
        )
    }
}

pub fn argmax(logits: &[f32]) -> u32 {
    let mut best = 0;
    let mut bv = f32::NEG_INFINITY;
    for (i, &v) in logits.iter().enumerate() {
        if v > bv {
            bv = v;
            best = i;
        }
    }
    best as u32
}

fn sample_top(logits: &[f32], temp: f32, top_p: f32, min_p: f32, rng: &mut Rng) -> u32 {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !max.is_finite() {
        return argmax(logits);
    }
    // Probabilities below ~1e-9 of the max cannot matter for top-p ≤ 0.9999; skip them.
    let cutoff = max - 20.7 * temp;
    let mut cand: Vec<(u32, f32)> = logits
        .iter()
        .enumerate()
        .filter(|(_, l)| **l >= cutoff)
        .map(|(i, l)| (i as u32, ((l - max) / temp).exp()))
        .collect();
    let sum: f32 = cand.iter().map(|c| c.1).sum();
    for c in &mut cand {
        c.1 /= sum;
    }
    cand.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
    let pmax = cand[0].1;
    let mut keep = cand.len();
    if min_p > 0.0 {
        keep = cand
            .iter()
            .take_while(|c| c.1 >= min_p * pmax)
            .count()
            .max(1);
    }
    if top_p < 1.0 {
        let mut acc = 0.0;
        let mut n = 0;
        for c in cand.iter().take(keep) {
            acc += c.1;
            n += 1;
            if acc >= top_p {
                break;
            }
        }
        keep = n.max(1);
    }
    let cand = &cand[..keep];
    let total: f32 = cand.iter().map(|c| c.1).sum();
    let mut r = rng.next_f32() * total;
    for c in cand {
        if r < c.1 {
            return c.0;
        }
        r -= c.1;
    }
    cand[cand.len() - 1].0
}

/// True when the trailing 16-gram occurs ≥ 3 times in the window.
pub fn detect_repetition(window: &[u32]) -> bool {
    if window.len() < REP_NGRAM * REP_COUNT {
        return false;
    }
    let tail = &window[window.len() - REP_NGRAM..];
    let count = window.windows(REP_NGRAM).filter(|w| *w == tail).count();
    count >= REP_COUNT
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(temp: f32, seed: u64) -> SamplingParams {
        SamplingParams {
            temperature: temp,
            seed: Some(seed),
            ..SamplingParams::default()
        }
    }

    #[test]
    fn seeded_sampling_is_reproducible() {
        let logits: Vec<f32> = (0..100).map(|i| (i as f32 * 0.37).sin() * 3.0).collect();
        let run = |seed| {
            let mut s = Sampler::new(params(1.0, seed));
            (0..50)
                .map(|_| s.sample(&mut logits.clone(), false))
                .collect::<Vec<_>>()
        };
        assert_eq!(run(7), run(7));
        assert_ne!(run(7), run(8));
    }

    #[test]
    fn zero_temperature_is_greedy() {
        let mut s = Sampler::new(params(0.0, 1));
        let mut l = vec![0.1, 5.0, 4.9, -1.0];
        assert_eq!(s.sample(&mut l, false), 1);
    }

    #[test]
    fn top_p_limits_support() {
        // One dominant token (p≈0.97) → with top_p=0.95 only it may be chosen.
        let mut s = Sampler::new(params(1.0, 3));
        for _ in 0..200 {
            let mut l = vec![10.0, 6.5, 0.0, 0.0];
            assert_eq!(s.sample(&mut l, false), 0);
        }
    }

    #[test]
    fn min_p_filters_tail() {
        let mut rng = Rng::new(1);
        for _ in 0..200 {
            let l = [2.0f32, 1.9, -3.0, -3.0];
            let id = sample_top(&l, 1.0, 1.0, 0.1, &mut rng);
            assert!(id < 2);
        }
    }

    #[test]
    fn tool_call_temperature_is_colder() {
        let p = SamplingParams {
            temperature: 1.0,
            tool_call_temperature: 0.01,
            top_p: 1.0,
            seed: Some(5),
            ..SamplingParams::default()
        };
        let mut s = Sampler::new(p);
        for _ in 0..100 {
            let mut l = vec![1.0, 1.2, 0.9];
            assert_eq!(s.sample(&mut l, true), 1);
        }
    }

    #[test]
    fn repetition_detection_enables_penalty() {
        let mut s = Sampler::new(params(1.0, 1));
        for i in 0..40 {
            s.observe(i);
        }
        assert!(!s.penalty_active());
        for _ in 0..3 {
            for i in 100..116 {
                s.observe(i);
            }
        }
        assert!(s.penalty_active());
        let mut l = vec![0.0f32; 200];
        l[105] = 2.0;
        l[150] = 1.99;
        let mut g = Sampler::new(SamplingParams {
            temperature: 0.0,
            repetition_penalty: 1.05,
            ..SamplingParams::default()
        });
        g.penalty_on = true;
        g.recent.extend([105u32]);
        assert_eq!(g.sample(&mut l, false), 150);
    }

    #[test]
    fn rng_uniform_range() {
        let mut r = Rng::new(42);
        for _ in 0..1000 {
            let x = r.next_f32();
            assert!((0.0..1.0).contains(&x));
        }
    }
}
