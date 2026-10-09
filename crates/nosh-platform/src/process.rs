//! Optional host process observations, not inference-engine state.

/// Resident set size of this process in MB (Linux), current and peak.
pub fn rss_mb() -> Option<(f64, f64)> {
    let s = std::fs::read_to_string("/proc/self/status").ok()?;
    let field = |name: &str| -> Option<f64> {
        s.lines()
            .find(|l| l.starts_with(name))?
            .split_whitespace()
            .nth(1)?
            .parse::<f64>()
            .ok()
            .map(|kb| kb / 1024.0)
    };
    Some((field("VmRSS:")?, field("VmHWM:")?))
}
