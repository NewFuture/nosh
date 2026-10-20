fn top_k(logits: &[f32], k: usize) -> Vec<usize> {
    let mut idx: Vec<usize> = (0..logits.len()).collect();
    idx.sort_by(|&a, &b| logits[b].total_cmp(&logits[a]));
    idx.truncate(k);
    idx
}

pub(crate) fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| *x as f64 * *y as f64).sum();
    let na: f64 = a.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| (*x as f64).powi(2)).sum::<f64>().sqrt();
    dot / (na * nb)
}

fn softmax(logits: &[f32]) -> Vec<f64> {
    let m = logits.iter().fold(f32::NEG_INFINITY, |a, &b| a.max(b)) as f64;
    let e: Vec<f64> = logits.iter().map(|&x| (x as f64 - m).exp()).collect();
    let z: f64 = e.iter().sum();
    e.into_iter().map(|x| x / z).collect()
}

fn kl(p: &[f64], q: &[f64]) -> f64 {
    p.iter()
        .zip(q)
        .filter(|(a, _)| **a > 0.0)
        .map(|(a, b)| a * (a / b.max(1e-300)).ln())
        .sum()
}

pub(crate) struct Divergence {
    cos: Vec<f64>,
    kl_mean: f64,
    nll: (f64, f64),
    top5_sets: usize,
    top5_ordered: usize,
    confident: (usize, usize),
}

pub(crate) fn divergence(want: &[Vec<f32>], got: &[Vec<f32>], tail: &[u32]) -> Divergence {
    assert!(!tail.is_empty());
    assert_eq!(want.len(), tail.len() + 1);
    assert_eq!(got.len(), want.len());
    let mut d = Divergence {
        cos: Vec::new(),
        kl_mean: 0.0,
        nll: (0.0, 0.0),
        top5_sets: 0,
        top5_ordered: 0,
        confident: (0, 0),
    };
    for (i, (a, b)) in want.iter().zip(got).enumerate() {
        assert!(!a.is_empty());
        assert_eq!(a.len(), b.len());
        assert!(a.iter().chain(b).all(|v| v.is_finite()));
        let (pa, pb) = (softmax(a), softmax(b));
        d.cos.push(cosine(a, b));
        d.kl_mean += kl(&pa, &pb) / want.len() as f64;
        if let Some(&next) = tail.get(i) {
            d.nll.0 -= pa[next as usize].ln() / tail.len() as f64;
            d.nll.1 -= pb[next as usize].ln() / tail.len() as f64;
        }
        let (ta, tb) = (top_k(a, 5), top_k(b, 5));
        d.top5_ordered += usize::from(ta == tb);
        let (mut sa, mut sb) = (ta.clone(), tb.clone());
        sa.sort_unstable();
        sb.sort_unstable();
        d.top5_sets += usize::from(sa == sb);
        if pa[ta[0]] > 0.5 {
            d.confident.0 += 1;
            d.confident.1 += usize::from(ta[0] == tb[0]);
        }
    }
    d
}

impl Divergence {
    pub(crate) fn assert_acceptable(&self) {
        assert!(self.cos.iter().all(|v| v.is_finite()));
        assert!(self.kl_mean.is_finite() && self.nll.0.is_finite() && self.nll.1.is_finite());
        assert!(self.confident.0 > 0, "no confident reference predictions");
        assert_eq!(
            self.confident.0, self.confident.1,
            "confident top-1 changed"
        );
        assert!(self.kl_mean < 0.03, "mean KL {}", self.kl_mean);
        assert!((self.nll.1 - self.nll.0).abs() < 0.05, "NLL {:?}", self.nll);
        let mut c = self.cos.clone();
        c.sort_by(f64::total_cmp);
        assert!(
            c[c.len() / 2] > 0.995 && self.cos[0] > 0.995,
            "cosine {c:?}"
        );
        assert!(
            self.top5_sets * 10 >= self.cos.len() * 6,
            "top-5 sets {}",
            self.top5_sets
        );
    }
}

impl std::fmt::Display for Divergence {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut c = self.cos.clone();
        c.sort_by(|a, b| a.total_cmp(b));
        write!(
            f,
            "cosine at prompt end {:.5}, median {:.5}, min {:.5} | mean KL {:.5} nats | NLL {:.4} vs {:.4} | top-5 sets {}/{} ordered {}/{} | confident top-1 {}/{}",
            self.cos[0],
            c[c.len() / 2],
            c[0],
            self.kl_mean,
            self.nll.0,
            self.nll.1,
            self.top5_sets,
            self.cos.len(),
            self.top5_ordered,
            self.cos.len(),
            self.confident.1,
            self.confident.0
        )
    }
}
