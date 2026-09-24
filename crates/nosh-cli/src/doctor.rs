//! `nosh doctor`: CPU, memory, model, download sources, offline state, config.

use std::time::{Duration, Instant};

use nosh_hub::sources::{Endpoints, candidates};
use nosh_hub::{ModelEntry, ModelHub, net, tr};
use nosh_shell::style;

use crate::config::Config;
use crate::engine::{self, EngineSetup};

/// Free memory wanted on top of a model's declared minimum, for the shell
/// and the rest of the system.
const MEMORY_HEADROOM_MB: u64 = 512;

/// A warning when `available` bytes are not enough to run `model`.
fn memory_warning(model: &ModelEntry, available: u64) -> Option<String> {
    let needed = (model.min_memory_mb + MEMORY_HEADROOM_MB) * 1024 * 1024;
    (available < needed).then(|| {
        format!(
            "{} needs about {:.1} GB",
            model.display,
            needed as f64 / 1e9
        )
    })
}

fn ok(label: &str, msg: &str) {
    eprintln!("{} {label:<10} {msg}", style::green("✔"));
}

fn warn(label: &str, msg: &str) {
    eprintln!("{} {label:<10} {msg}", style::yellow("!"));
}

fn bad(label: &str, msg: &str) {
    eprintln!("{} {label:<10} {msg}", style::red("✗"));
}

fn meminfo(key: &str) -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    text.lines()
        .find_map(|l| l.strip_prefix(key))
        .and_then(|v| v.trim().trim_end_matches("kB").trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
}

fn cpu_model() -> String {
    std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find_map(|l| l.strip_prefix("model name"))
                .map(|v| v.trim_start_matches([' ', '\t', ':']).trim().to_string())
        })
        .unwrap_or_else(|| std::env::consts::ARCH.to_string())
}

/// The features candle picks its kernels by, and whether inference is fast
/// enough: candle's x86 kernels need AVX2 and FMA; every aarch64 CPU has NEON.
fn cpu_features() -> (Vec<&'static str>, bool) {
    let f = nosh_llm::cpu::features();
    let usable = !cfg!(target_arch = "x86_64") || (f.contains(&"avx2") && f.contains(&"fma"));
    (f, usable)
}

pub fn run(cfg: &Config, setup: &EngineSetup) -> i32 {
    let mut problems = 0;
    eprintln!(
        "nosh {} ({} {})",
        env!("CARGO_PKG_VERSION"),
        std::env::consts::OS,
        std::env::consts::ARCH
    );

    let (feats, usable) = cpu_features();
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let line = format!("{} · {cores} threads · {}", cpu_model(), feats.join(" "));
    if usable {
        ok("cpu", &line);
    } else {
        warn(
            "cpu",
            &format!("{line} · no AVX2/FMA: inference will be slow"),
        );
    }

    let hub = ModelHub::new();
    let entry = hub.registry().lookup(setup.model_id.as_deref()).cloned();
    let located = engine::locate(setup);
    // The model memory is checked for: the installed one, else the selected
    // (or default) registry entry.
    let model: Option<ModelEntry> = match &located {
        Ok(Some(r)) => Some(r.entry.clone()),
        _ => entry
            .as_ref()
            .ok()
            .cloned()
            .or_else(|| hub.registry().lookup(None).ok().cloned()),
    };

    match (meminfo("MemTotal:"), meminfo("MemAvailable:")) {
        (Some(t), Some(a)) => {
            let line = format!(
                "{:.1} GB total · {:.1} GB available",
                t as f64 / 1e9,
                a as f64 / 1e9
            );
            match model.as_ref().and_then(|m| memory_warning(m, a)) {
                Some(w) => warn("memory", &format!("{line} · {w}")),
                None => ok("memory", &line),
            }
        }
        _ => warn("memory", "unknown"),
    }

    match located {
        Ok(Some(r)) => ok(
            "model",
            &format!("{} · {}", r.entry.display, r.weights.display()),
        ),
        Ok(None) => {
            problems += 1;
            let name = entry
                .as_ref()
                .map(|e| e.display.clone())
                .unwrap_or_default();
            bad(
                "model",
                &tr!(
                    format!("{name} 未安装 · 运行 `nosh model pull`"),
                    format!("{name} not installed · run `nosh model pull`")
                ),
            );
        }
        Err(e) => {
            problems += 1;
            bad("model", &e.to_string());
        }
    }

    if net::is_offline() {
        ok(
            "offline",
            tr!("离线模式：不会访问网络", "offline mode: no network access"),
        );
    } else if let Ok(entry) = &entry {
        ok(
            "offline",
            tr!(
                "未开启（模型就绪后无需联网）",
                "off (no network needed once the model is installed)"
            ),
        );
        let file = entry.tokenizer();
        for c in candidates(file, &Endpoints::from_env()) {
            let start = Instant::now();
            match net::head(&c.url, Duration::from_secs(5)) {
                Ok(h) if h.status < 400 => ok(
                    c.hub.name(),
                    &format!("HTTP {} · {} ms", h.status, start.elapsed().as_millis()),
                ),
                Ok(h) => warn(c.hub.name(), &format!("HTTP {}", h.status)),
                Err(e) => warn(c.hub.name(), &e.to_string()),
            }
        }
    }

    match &cfg.path {
        Some(p) if p.exists() => {
            if cfg.warnings.is_empty() {
                ok("config", &p.display().to_string());
            } else {
                warn(
                    "config",
                    &format!("{} · {} warning(s)", p.display(), cfg.warnings.len()),
                );
                cfg.print_warnings();
            }
        }
        Some(p) => ok(
            "config",
            &format!("{} (not present; defaults)", p.display()),
        ),
        None => {}
    }
    ok("mode", cfg.approval.as_str());
    if problems == 0 { 0 } else { 1 }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_warning_follows_the_selected_model() {
        let reg = nosh_hub::Registry::builtin();
        let q4 = reg.lookup(None).unwrap();
        let q8 = reg.lookup(Some("minicpm5-2b:q8_0")).unwrap();
        let small = reg.lookup(Some("minicpm5-1b:q4_k_m")).unwrap();
        let gb = |x: f64| (x * 1e9) as u64;
        // The Q8_0 model declares more than the default Q4_K_M one.
        assert_eq!(memory_warning(q4, gb(3.5)), None);
        let w = memory_warning(q8, gb(3.5)).unwrap();
        assert!(w.contains(&q8.display), "{w}");
        // The 1B model does not get the 2B warning.
        assert_eq!(memory_warning(small, gb(2.5)), None);
        assert!(memory_warning(q4, gb(2.5)).is_some());
    }
}
