//! `nosh doctor`: CPU, memory, model, download sources, offline state, config.

use std::time::{Duration, Instant};

use nosh_hub::sources::{Endpoints, candidates};
use nosh_hub::{ModelHub, net, tr};
use nosh_shell::style;

use crate::config::Config;
use crate::engine::{self, EngineSetup};

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

#[cfg(target_arch = "x86_64")]
fn cpu_features() -> (Vec<&'static str>, bool) {
    let mut f = Vec::new();
    macro_rules! feat {
        ($($name:tt),*) => {$(
            if std::arch::is_x86_feature_detected!($name) {
                f.push($name);
            }
        )*};
    }
    feat!("avx2", "fma", "f16c", "avx512f", "avx512bw", "avx512vnni");
    let usable =
        std::arch::is_x86_feature_detected!("avx2") && std::arch::is_x86_feature_detected!("fma");
    (f, usable)
}

#[cfg(not(target_arch = "x86_64"))]
fn cpu_features() -> (Vec<&'static str>, bool) {
    (vec!["neon"], true)
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

    match (meminfo("MemTotal:"), meminfo("MemAvailable:")) {
        (Some(t), Some(a)) => {
            let line = format!(
                "{:.1} GB total · {:.1} GB available",
                t as f64 / 1e9,
                a as f64 / 1e9
            );
            if a < 3_500_000_000 {
                warn(
                    "memory",
                    &format!("{line} · the 2B model needs about 3 GB (8K context)"),
                );
            } else {
                ok("memory", &line);
            }
        }
        _ => warn("memory", "unknown"),
    }

    let hub = ModelHub::new();
    let entry = hub.registry().lookup(setup.model_id.as_deref()).cloned();
    match engine::locate(setup) {
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
