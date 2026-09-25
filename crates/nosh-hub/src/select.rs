//! Source selection: parallel HEAD + 2 MB speed probe, ranked by throughput.

use std::io::Read;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::net;
use crate::sources::{Candidate, Hub};

pub const PROBE_BYTES: u64 = 2 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct ProbeResult {
    pub hub: Hub,
    pub latency: Option<Duration>,
    /// Bytes per second observed while fetching up to [`PROBE_BYTES`].
    pub throughput: Option<f64>,
    /// Body bytes received by the speed probe.
    pub bytes: u64,
    pub error: Option<String>,
}

impl ProbeResult {
    pub fn ok(&self) -> bool {
        self.error.is_none() && self.throughput.is_some()
    }
}

fn probe_one(c: &Candidate, deadline: Instant) -> ProbeResult {
    let mut res = ProbeResult {
        hub: c.hub,
        latency: None,
        throughput: None,
        bytes: 0,
        error: None,
    };
    let remaining = |d: Instant| d.saturating_duration_since(Instant::now());
    let t0 = Instant::now();
    let size = match net::head(&c.url, remaining(deadline).max(Duration::from_millis(200))) {
        Ok(h) if (200..400).contains(&h.status) => {
            res.latency = Some(t0.elapsed());
            h.content_length
        }
        Ok(h) => {
            res.error = Some(format!("HTTP {}", h.status));
            return res;
        }
        Err(e) => {
            res.error = Some(e.to_string());
            return res;
        }
    };
    let t1 = Instant::now();
    let resp = match net::get_range(
        &c.url,
        0,
        Some(PROBE_BYTES - 1),
        remaining(deadline).max(Duration::from_millis(200)),
    ) {
        Ok(r) => r,
        Err(e) => {
            res.error = Some(e.to_string());
            return res;
        }
    };
    // A complete probe is PROBE_BYTES, or the whole file if it is smaller.
    let expected = size.map_or(PROBE_BYTES, |s| s.min(PROBE_BYTES));
    let mut reader = resp.reader.take(PROBE_BYTES);
    let mut buf = vec![0u8; 64 * 1024];
    let mut got = 0u64;
    let mut reads = 0u32;
    // Timed from the first body byte so TLS/redirect setup does not dominate.
    let mut first_byte: Option<Instant> = None;
    let mut ended = false;
    while Instant::now() < deadline {
        match reader.read(&mut buf) {
            Ok(0) => {
                ended = true;
                break;
            }
            Ok(n) => {
                if first_byte.is_none() {
                    first_byte = Some(Instant::now());
                }
                got += n as u64;
                reads += 1;
            }
            Err(e) => {
                if got == 0 {
                    res.error = Some(e.to_string());
                    return res;
                }
                // Cut off mid-body (the response is shorter than its length).
                ended = true;
                break;
            }
        }
    }
    res.bytes = got;
    if got == 0 {
        res.error = Some("no data".into());
        return res;
    }
    if ended && got < expected {
        res.error = Some(format!("connection closed after {got} of {expected} bytes"));
        return res;
    }
    // One read gives no interval after the first byte; then time the whole request.
    let secs = match first_byte {
        Some(t) if reads > 1 => t.elapsed(),
        _ => t1.elapsed(),
    }
    .as_secs_f64()
    .max(1e-3);
    res.throughput = Some(got as f64 / secs);
    res
}

/// Probes all candidates in parallel within `budget` (design: ≤ 3 s total).
pub fn probe_all(cands: &[Candidate], budget: Duration) -> Vec<ProbeResult> {
    let deadline = Instant::now() + budget;
    let (tx, rx) = mpsc::channel();
    for c in cands {
        let tx = tx.clone();
        let c = c.clone();
        std::thread::spawn(move || {
            let _ = tx.send(probe_one(&c, deadline));
        });
    }
    drop(tx);
    let mut out = Vec::new();
    let grace = deadline + Duration::from_millis(500);
    while out.len() < cands.len() {
        let wait = grace.saturating_duration_since(Instant::now());
        match rx.recv_timeout(wait) {
            Ok(r) => out.push(r),
            Err(_) => break,
        }
    }
    for c in cands {
        if !out.iter().any(|r| r.hub == c.hub) {
            out.push(ProbeResult {
                hub: c.hub,
                latency: None,
                throughput: None,
                bytes: 0,
                error: Some("timed out".into()),
            });
        }
    }
    out
}

/// Orders candidates: measured throughput first (fastest first), then the
/// unmeasured/failed ones in `preferred` order.
pub fn rank(cands: &[Candidate], probes: &[ProbeResult], preferred: &[Hub]) -> Vec<Candidate> {
    let speed = |h: Hub| {
        probes
            .iter()
            .find(|p| p.hub == h && p.ok())
            .and_then(|p| p.throughput)
    };
    let pref = |h: Hub| preferred.iter().position(|p| *p == h).unwrap_or(usize::MAX);
    let mut out: Vec<Candidate> = cands.to_vec();
    out.sort_by(|a, b| match (speed(a.hub), speed(b.hub)) {
        (Some(x), Some(y)) => y.total_cmp(&x),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => pref(a.hub).cmp(&pref(b.hub)),
    });
    out
}

/// Orders candidates by preference only (no network).
pub fn order_by_preference(cands: &[Candidate], preferred: &[Hub]) -> Vec<Candidate> {
    rank(cands, &[], preferred)
}

pub fn format_probe_summary(probes: &[ProbeResult]) -> String {
    let terminal = crate::terminal::stderr();
    let mut sorted: Vec<&ProbeResult> = probes.iter().collect();
    sorted.sort_by(|a, b| {
        b.throughput
            .unwrap_or(0.0)
            .total_cmp(&a.throughput.unwrap_or(0.0))
    });
    sorted
        .iter()
        .map(|p| match (p.throughput, &p.error) {
            (Some(t), None) => format!("{} {:.1} MB/s", p.hub, t / 1e6),
            (_, Some(e)) => format!("{} {} ({})", p.hub, terminal.glyph("✗", "x"), short(e)),
            _ => format!("{} {}", p.hub, terminal.glyph("✗", "x")),
        })
        .collect::<Vec<_>>()
        .join(terminal.glyph(" · ", " | "))
}

fn short(s: &str) -> String {
    let s = s.lines().next().unwrap_or_default();
    if s.chars().count() > 40 {
        format!(
            "{}{}",
            s.chars().take(40).collect::<String>(),
            crate::terminal::stderr().glyph("…", "...")
        )
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cand(hub: Hub) -> Candidate {
        Candidate {
            hub,
            url: format!("http://{}/f", hub.name()),
            revision: "main".into(),
        }
    }

    fn probe(hub: Hub, tput: Option<f64>) -> ProbeResult {
        ProbeResult {
            hub,
            latency: tput.map(|_| Duration::from_millis(50)),
            throughput: tput,
            bytes: if tput.is_some() { PROBE_BYTES } else { 0 },
            error: if tput.is_none() {
                Some("fail".into())
            } else {
                None
            },
        }
    }

    #[test]
    fn ranks_by_throughput_then_preference() {
        let cands = vec![
            cand(Hub::HuggingFace),
            cand(Hub::HfMirror),
            cand(Hub::ModelScope),
        ];
        let probes = vec![
            probe(Hub::HuggingFace, Some(3.1e6)),
            probe(Hub::HfMirror, None),
            probe(Hub::ModelScope, Some(21.4e6)),
        ];
        let order: Vec<Hub> = rank(&cands, &probes, &[Hub::HfMirror, Hub::HuggingFace])
            .iter()
            .map(|c| c.hub)
            .collect();
        assert_eq!(
            order,
            vec![Hub::ModelScope, Hub::HuggingFace, Hub::HfMirror]
        );
    }

    #[test]
    fn no_probes_uses_region_preference() {
        let cands = vec![
            cand(Hub::HuggingFace),
            cand(Hub::HfMirror),
            cand(Hub::ModelScope),
        ];
        let order: Vec<Hub> =
            order_by_preference(&cands, &[Hub::ModelScope, Hub::HfMirror, Hub::HuggingFace])
                .iter()
                .map(|c| c.hub)
                .collect();
        assert_eq!(
            order,
            vec![Hub::ModelScope, Hub::HfMirror, Hub::HuggingFace]
        );
    }

    #[test]
    fn probes_local_servers() {
        use crate::testserver::{Behavior, TestServer};
        let body: Vec<u8> = (0..(PROBE_BYTES as usize + 10))
            .map(|i| (i % 256) as u8)
            .collect();
        let good = TestServer::start(body.clone(), Behavior::Normal);
        let bad = TestServer::start(body, Behavior::Status(503));
        let cands = vec![
            Candidate {
                hub: Hub::HuggingFace,
                url: bad.url("f"),
                revision: "main".into(),
            },
            Candidate {
                hub: Hub::ModelScope,
                url: good.url("f"),
                revision: "main".into(),
            },
        ];
        let probes = probe_all(&cands, Duration::from_secs(3));
        assert_eq!(probes.len(), 2);
        let ms = probes.iter().find(|p| p.hub == Hub::ModelScope).unwrap();
        assert!(ms.ok(), "{ms:?}");
        // Every read counts, including the first one.
        assert_eq!(ms.bytes, PROBE_BYTES, "{ms:?}");
        let hf = probes.iter().find(|p| p.hub == Hub::HuggingFace).unwrap();
        assert!(!hf.ok());
        let ranked = rank(&cands, &probes, &[Hub::HuggingFace, Hub::ModelScope]);
        assert_eq!(ranked[0].hub, Hub::ModelScope);
    }

    #[test]
    fn a_source_that_closes_early_is_not_a_full_probe() {
        use crate::testserver::{Behavior, TestServer};
        let body: Vec<u8> = (0..(PROBE_BYTES as usize + 10))
            .map(|i| (i % 256) as u8)
            .collect();
        // Sends one read's worth and hangs up.
        let broken = TestServer::start(body.clone(), Behavior::DropAfter(16 * 1024));
        let good = TestServer::start(body, Behavior::Normal);
        let cands = vec![
            Candidate {
                hub: Hub::HuggingFace,
                url: broken.url("f"),
                revision: "main".into(),
            },
            Candidate {
                hub: Hub::HfMirror,
                url: good.url("f"),
                revision: "main".into(),
            },
        ];
        let probes = probe_all(&cands, Duration::from_secs(3));
        let hf = probes.iter().find(|p| p.hub == Hub::HuggingFace).unwrap();
        assert!(!hf.ok(), "{hf:?}");
        assert_eq!(hf.bytes, 16 * 1024);
        assert!(hf.error.as_deref().unwrap().contains("closed"), "{hf:?}");
        let ranked = rank(&cands, &probes, &[Hub::HuggingFace, Hub::HfMirror]);
        assert_eq!(ranked[0].hub, Hub::HfMirror);
    }

    #[test]
    fn a_small_file_is_a_complete_probe() {
        use crate::testserver::{Behavior, TestServer};
        let srv = TestServer::start(vec![7u8; 5000], Behavior::Normal);
        let c = Candidate {
            hub: Hub::ModelScope,
            url: srv.url("tok"),
            revision: "main".into(),
        };
        let p = probe_one(&c, Instant::now() + Duration::from_secs(3));
        assert!(p.ok(), "{p:?}");
        assert_eq!(p.bytes, 5000);
    }

    #[test]
    fn summary_format() {
        let s = format_probe_summary(&[
            probe(Hub::HuggingFace, Some(3.1e6)),
            probe(Hub::ModelScope, Some(21.4e6)),
        ]);
        assert_eq!(
            s,
            format!(
                "modelscope.cn 21.4 MB/s{}huggingface.co 3.1 MB/s",
                crate::terminal::stderr().glyph(" · ", " | ")
            )
        );
    }
}
