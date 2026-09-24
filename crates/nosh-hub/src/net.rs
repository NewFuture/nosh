//! The single choke point for network access. When offline mode is on, every
//! request fails with [`HubError::Offline`] before a socket is opened.

use std::io::Read;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::HubError;

static FORCED_OFFLINE: AtomicBool = AtomicBool::new(false);
static CANCELLED: AtomicBool = AtomicBool::new(false);

/// Asks running downloads to stop (safe to call from a Ctrl-C hook); the
/// partial file is kept for resuming.
pub fn cancel() {
    CANCELLED.store(true, Ordering::SeqCst);
}

pub fn clear_cancel() {
    CANCELLED.store(false, Ordering::SeqCst);
}

pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

/// Forces offline mode for the rest of the process (e.g. `--offline`).
pub fn set_offline(offline: bool) {
    FORCED_OFFLINE.store(offline, Ordering::SeqCst);
}

fn truthy(v: Option<String>) -> bool {
    v.map(|v| {
        let v = v.trim().to_ascii_lowercase();
        !(v.is_empty() || v == "0" || v == "false" || v == "no" || v == "off")
    })
    .unwrap_or(false)
}

fn offline_from(forced: bool, env: impl Fn(&str) -> Option<String>) -> bool {
    forced || truthy(env("NOSH_OFFLINE")) || truthy(env("HF_HUB_OFFLINE"))
}

/// Offline if forced, or if `NOSH_OFFLINE` / `HF_HUB_OFFLINE` is truthy.
pub fn is_offline() -> bool {
    offline_from(FORCED_OFFLINE.load(Ordering::SeqCst), |k| {
        std::env::var(k).ok()
    })
}

fn check_online() -> Result<(), HubError> {
    if is_offline() {
        Err(HubError::Offline)
    } else {
        Ok(())
    }
}

const USER_AGENT: &str = concat!("nosh/", env!("CARGO_PKG_VERSION"));

fn agent(per_call: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .user_agent(USER_AGENT)
        .timeout_connect(Some(Duration::from_secs(10)))
        .timeout_recv_response(Some(Duration::from_secs(20)))
        .timeout_per_call(Some(per_call))
        .http_status_as_error(false)
        .max_redirects(10)
        .build()
        .into()
}

/// One agent for HEAD requests, so probes reuse connections; each request
/// sets its own timeout.
fn head_agent() -> &'static ureq::Agent {
    static AGENT: OnceLock<ureq::Agent> = OnceLock::new();
    AGENT.get_or_init(|| agent(Duration::from_secs(5)))
}

fn net_err(url: &str, e: ureq::Error) -> HubError {
    HubError::Network(format!("{url}: {e}"))
}

/// Result of a HEAD request.
#[derive(Debug, Clone)]
pub struct HeadInfo {
    pub status: u16,
    pub content_length: Option<u64>,
    pub linked_etag: Option<String>,
    pub accept_ranges: bool,
}

/// HEAD `url`; the whole request (redirects included) ends within `timeout`.
pub fn head(url: &str, timeout: Duration) -> Result<HeadInfo, HubError> {
    check_online()?;
    let resp = head_agent()
        .head(url)
        .config()
        .timeout_global(Some(timeout))
        .timeout_per_call(Some(timeout))
        .build()
        .call()
        .map_err(|e| net_err(url, e))?;
    let h = resp.headers();
    let get = |k: &str| h.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
    Ok(HeadInfo {
        status: resp.status().as_u16(),
        content_length: get("content-length").and_then(|v| v.parse().ok()),
        linked_etag: get("x-linked-etag").map(|v| v.trim_matches('"').to_string()),
        accept_ranges: get("accept-ranges").is_some_and(|v| v.contains("bytes")),
    })
}

/// An open ranged GET response.
pub struct RangeResponse {
    /// 206 when the server honored the range, 200 when it sent the whole file.
    pub status: u16,
    /// First byte offset of the body.
    pub start: u64,
    pub reader: Box<dyn Read + Send>,
}

/// GET `url` for bytes `[start, end]` (inclusive; `None` = to the end).
pub fn get_range(
    url: &str,
    start: u64,
    end: Option<u64>,
    timeout: Duration,
) -> Result<RangeResponse, HubError> {
    check_online()?;
    let range = match end {
        Some(end) => format!("bytes={start}-{end}"),
        None => format!("bytes={start}-"),
    };
    let resp = agent(timeout)
        .get(url)
        .header("Range", &range)
        .call()
        .map_err(|e| net_err(url, e))?;
    let status = resp.status().as_u16();
    let body_start = match status {
        206 => resp
            .headers()
            .get("content-range")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_content_range_start)
            .unwrap_or(start),
        200 => 0,
        s => return Err(HubError::Network(format!("{url}: HTTP {s}"))),
    };
    let reader = resp.into_body().into_reader();
    Ok(RangeResponse {
        status,
        start: body_start,
        reader: Box::new(reader),
    })
}

fn parse_content_range_start(v: &str) -> Option<u64> {
    // "bytes 100-199/1000"
    let rest = v.trim().strip_prefix("bytes")?.trim();
    rest.split('-').next()?.trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_range_parsing() {
        assert_eq!(parse_content_range_start("bytes 100-199/1000"), Some(100));
        assert_eq!(parse_content_range_start("bytes 0-0/*"), Some(0));
        assert_eq!(parse_content_range_start("garbage"), None);
    }

    #[test]
    fn head_keeps_the_callers_timeout() {
        use crate::testserver::{Behavior, TestServer};
        let srv = TestServer::start(vec![0; 10], Behavior::Delay(Duration::from_secs(3)));
        // Shorter than the shared agent's own 5 s.
        let t0 = std::time::Instant::now();
        let r = head(&srv.url("slow"), Duration::from_millis(400));
        assert!(r.is_err(), "{r:?}");
        assert!(t0.elapsed() < Duration::from_secs(2), "{:?}", t0.elapsed());
        // Longer than 5 s is honoured too.
        let t0 = std::time::Instant::now();
        let r = head(&srv.url("slow"), Duration::from_secs(8)).unwrap();
        assert_eq!((r.status, r.content_length), (200, Some(10)));
        assert!(t0.elapsed() >= Duration::from_secs(3));
    }

    #[test]
    fn offline_switches() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |k: &str| {
                pairs
                    .iter()
                    .find(|(n, _)| *n == k)
                    .map(|(_, v)| v.to_string())
            }
        };
        assert!(!offline_from(false, env(&[])));
        assert!(offline_from(true, env(&[])));
        assert!(offline_from(false, env(&[("NOSH_OFFLINE", "1")])));
        assert!(offline_from(false, env(&[("HF_HUB_OFFLINE", "true")])));
        assert!(!offline_from(false, env(&[("NOSH_OFFLINE", "0")])));
        assert!(!offline_from(false, env(&[("HF_HUB_OFFLINE", "")])));
    }
}
