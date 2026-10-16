//! Token sampling: temperature / top-k / top-p / min-p, a lower temperature inside tool
//! calls, and a repetition penalty that switches on only once a loop is detected
//! (the same 16-gram ≥ 3 times within the last 256 tokens).

use std::collections::{HashSet, VecDeque};

use nosh_engine::SamplingParams;

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
    candidates: Vec<(u32, f32)>,
    penalized: HashSet<u32>,
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
            candidates: Vec::new(),
            penalized: HashSet::new(),
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
            self.penalized.clear();
            for &id in &self.recent {
                if self.penalized.insert(id)
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
            self.params.top_k,
            &mut self.rng,
            &mut self.candidates,
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

fn sample_top(
    logits: &[f32],
    temp: f32,
    top_p: f32,
    min_p: f32,
    top_k: usize,
    rng: &mut Rng,
    cand: &mut Vec<(u32, f32)>,
) -> u32 {
    let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    if !max.is_finite() {
        return argmax(logits);
    }
    // Probabilities below ~1e-9 of the max cannot matter for top-p ≤ 0.9999; skip them.
    let cutoff = max - 20.7 * temp;
    cand.clear();
    cand.extend(
        logits
            .iter()
            .enumerate()
            .filter(|(_, l)| **l >= cutoff)
            .map(|(i, l)| (i as u32, ((l - max) / temp).exp())),
    );
    if top_k > 0 && cand.len() > top_k {
        let order =
            |a: &(u32, f32), b: &(u32, f32)| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0));
        cand.select_nth_unstable_by(top_k - 1, order);
        cand.truncate(top_k);
        cand.sort_unstable_by(order);
    }
    let sum: f32 = cand.iter().map(|c| c.1).sum();
    for c in cand.iter_mut() {
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
    let count = window
        .windows(REP_NGRAM)
        .filter(|w| *w == tail)
        .take(REP_COUNT)
        .count();
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
    fn seeded_sampling_matches_pre_refactor_sequences() {
        let cases = [
            (
                1.0,
                0.95,
                0.0,
                [
                    23, 5, 53, 35, 18, 19, 72, 55, 73, 38, 71, 40, 58, 19, 90, 54, 5, 90, 38, 38,
                    21, 3, 3, 5,
                ],
            ),
            (
                0.3,
                0.8,
                0.0,
                [
                    39, 55, 73, 71, 71, 90, 89, 72, 21, 72, 4, 39, 88, 90, 21, 5, 55, 4, 72, 72,
                    72, 22, 22, 38,
                ],
            ),
            (
                1.0,
                1.0,
                0.1,
                [
                    23, 5, 53, 35, 18, 19, 72, 55, 73, 38, 71, 40, 58, 19, 90, 54, 5, 90, 38, 38,
                    21, 3, 3, 5,
                ],
            ),
            (1.0, 0.0, 0.0, [89; 24]),
        ];
        let logits: Vec<f32> = (0..100).map(|i| (i as f32 * 0.37).sin() * 3.0).collect();
        for (temperature, top_p, min_p, expected) in cases {
            let mut sampler = Sampler::new(SamplingParams {
                temperature,
                top_p,
                min_p,
                top_k: 0,
                seed: Some(7),
                ..SamplingParams::default()
            });
            let actual: Vec<_> = (0..expected.len())
                .map(|_| sampler.sample(&mut logits.clone(), false))
                .collect();
            assert_eq!(actual, expected);
        }
    }

    #[test]
    fn sampling_scratch_is_reused_without_stale_candidates() {
        let mut sampler = Sampler::new(params(1.0, 7));
        sampler.sample(&mut vec![0.0; 256], false);
        let capacity = sampler.candidates.capacity();
        let ptr = sampler.candidates.as_ptr();
        for len in [16, 128, 1, 256] {
            let mut logits = vec![f32::NEG_INFINITY; len];
            logits[len - 1] = 1.0;
            assert_eq!(sampler.sample(&mut logits, false), (len - 1) as u32);
            assert_eq!(sampler.candidates.len(), 1);
            assert_eq!(sampler.candidates.capacity(), capacity);
            assert_eq!(sampler.candidates.as_ptr(), ptr);
        }
    }

    #[test]
    fn repeated_tokens_are_penalized_once_per_sample() {
        let mut sampler = Sampler::new(SamplingParams {
            temperature: 0.0,
            repetition_penalty: 2.0,
            ..params(0.0, 7)
        });
        for _ in 0..REP_NGRAM * REP_COUNT {
            sampler.observe(1);
        }
        sampler.observe(2);
        sampler.observe(999);
        assert!(sampler.penalty_active());
        let mut logits = [1.0, 4.0, -3.0];
        sampler.sample(&mut logits, false);
        assert_eq!(logits, [1.0, 2.0, -6.0]);
        let capacity = sampler.penalized.capacity();
        let mut logits = [1.0, 4.0, -3.0];
        sampler.sample(&mut logits, false);
        assert_eq!(logits, [1.0, 2.0, -6.0]);
        assert_eq!(sampler.penalized.capacity(), capacity);
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
        let mut candidates = Vec::new();
        for _ in 0..200 {
            let l = [2.0f32, 1.9, -3.0, -3.0];
            let id = sample_top(&l, 1.0, 1.0, 0.1, 0, &mut rng, &mut candidates);
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
    fn default_top_k_limits_support_in_text_and_tool_calls() {
        assert_eq!(SamplingParams::default().top_k, 40);
        let mut sampler = Sampler::new(SamplingParams {
            top_p: 1.0,
            tool_call_temperature: 1.0,
            ..params(1.0, 7)
        });
        for in_tool_call in [false, true] {
            for _ in 0..200 {
                let mut logits: Vec<_> = (0..100).map(|i| i as f32 * 0.01).collect();
                assert!(sampler.sample(&mut logits, in_tool_call) >= 60);
            }
        }
    }

    #[test]
    fn zero_top_k_preserves_unfiltered_support() {
        let mut sampler = Sampler::new(SamplingParams {
            top_k: 0,
            top_p: 1.0,
            ..params(1.0, 7)
        });
        let ids: HashSet<_> = (0..500)
            .map(|_| sampler.sample(&mut vec![0.0; 100], false))
            .collect();
        assert!(ids.iter().any(|id| *id >= 40));
    }

    #[test]
    fn top_k_is_renormalized_before_top_p() {
        let mut sampler = Sampler::new(SamplingParams {
            top_k: 2,
            top_p: 0.5,
            ..params(1.0, 7)
        });
        for _ in 0..100 {
            let mut logits: Vec<_> = [0.4_f32, 0.3, 0.2, 0.1].into_iter().map(f32::ln).collect();
            assert_eq!(sampler.sample(&mut logits, false), 0);
        }
    }

    #[test]
    fn top_k_ties_are_resolved_by_token_id() {
        let mut sampler = Sampler::new(SamplingParams {
            top_k: 3,
            top_p: 1.0,
            ..params(1.0, 7)
        });
        for _ in 0..100 {
            assert!(sampler.sample(&mut vec![0.0; 100], false) < 3);
        }
    }

    #[test]
    fn top_k_selection_matches_full_sort_reference() {
        fn reference(logits: &[f32], params: SamplingParams, rng: &mut Rng) -> u32 {
            let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
            let mut candidates: Vec<_> = logits
                .iter()
                .enumerate()
                .filter(|(_, value)| **value >= max - 20.7 * params.temperature)
                .map(|(id, value)| (id as u32, ((value - max) / params.temperature).exp()))
                .collect();
            if params.top_k > 0 && candidates.len() > params.top_k {
                candidates.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
                candidates.truncate(params.top_k);
            }
            let sum: f32 = candidates.iter().map(|item| item.1).sum();
            for item in &mut candidates {
                item.1 /= sum;
            }
            candidates.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
            let pmax = candidates[0].1;
            if params.min_p > 0.0 {
                let keep = candidates
                    .iter()
                    .take_while(|item| item.1 >= params.min_p * pmax)
                    .count()
                    .max(1);
                candidates.truncate(keep);
            }
            if params.top_p < 1.0 {
                let mut probability = 0.0;
                let keep = candidates
                    .iter()
                    .take_while(|item| {
                        let keep = probability < params.top_p;
                        probability += item.1;
                        keep
                    })
                    .count()
                    .max(1);
                candidates.truncate(keep);
            }
            let mut target = rng.next_f32() * candidates.iter().map(|item| item.1).sum::<f32>();
            for &(id, probability) in &candidates {
                if target < probability {
                    return id;
                }
                target -= probability;
            }
            candidates.last().unwrap().0
        }

        for vocabulary in [16, 257, 131_072] {
            let logits: Vec<_> = (0..vocabulary)
                .map(|id| {
                    if id % 17 == 0 {
                        f32::NEG_INFINITY
                    } else {
                        (id % 29) as f32 * 0.031
                    }
                })
                .collect();
            for top_k in [0, 1, 3, 40, vocabulary + 1] {
                for (temperature, top_p, min_p) in
                    [(1.0, 0.95, 0.0), (0.3, 0.8, 0.1), (1.0, 1.0, 0.0)]
                {
                    let params = SamplingParams {
                        temperature,
                        top_p,
                        top_k,
                        min_p,
                        ..params(temperature, 7)
                    };
                    let mut sampler = Sampler::new(params);
                    let mut rng = Rng::new(7);
                    let mut current = logits.clone();
                    for _ in 0..16 {
                        assert_eq!(
                            sampler.sample(&mut current, false),
                            reference(&logits, params, &mut rng),
                            "vocabulary={vocabulary}, params={params:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "manual sampler throughput measurement"]
    fn sampling_top_k_throughput() {
        let logits: Vec<_> = (0..131_072)
            .map(|i| ((i as f32 * 0.017).sin() + (i as f32 * 0.041).cos()) * 4.0)
            .collect();
        for (top_k, draws) in [(40, 256), (0, 64)] {
            let mut samples = Vec::new();
            let mut checksum = 0_u64;
            for seed in 0..5 {
                let mut sampler = Sampler::new(SamplingParams {
                    top_k,
                    ..params(1.0, seed)
                });
                let mut current = logits.clone();
                let started = std::time::Instant::now();
                for _ in 0..draws {
                    checksum = checksum.wrapping_add(u64::from(
                        sampler.sample(std::hint::black_box(&mut current), false),
                    ));
                }
                samples.push(started.elapsed().as_secs_f64());
            }
            println!(
                "SAMPLING_BENCHMARK {}",
                serde_json::json!({
                    "vocabulary": logits.len(), "top_k": top_k, "draws_per_sample": draws,
                    "seconds": samples, "checksum": checksum,
                })
            );
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
