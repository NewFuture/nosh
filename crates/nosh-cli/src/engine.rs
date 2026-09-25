//! Finding, downloading (with first-run confirmation) and loading the model.

use std::path::PathBuf;

use nosh_core::LoadedEngine;
use nosh_hub::{
    BarProgress, HubError, ModelHub, PullOptions, ResolvedModel, SourceSelection, net, tr,
};
use nosh_llm::{LocalChatEngine, LocalEngineOptions};
use nosh_shell::{style, term};

#[derive(Debug, Clone)]
pub struct EngineSetup {
    pub model_id: Option<String>,
    pub model_path: Option<PathBuf>,
    pub context_length: usize,
    pub seed: Option<u64>,
    /// `--no-download` or `download.auto = "never"`.
    pub no_download: bool,
    pub selection: SourceSelection,
}

fn declined_marker() -> PathBuf {
    nosh_hub::paths::state_dir().join("download-declined")
}

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

/// Where the model is, if it is available without downloading.
pub fn locate(setup: &EngineSetup) -> Result<Option<ResolvedModel>, HubError> {
    let hub = ModelHub::new();
    let path = setup
        .model_path
        .clone()
        .or_else(|| std::env::var_os("NOSH_MODEL_PATH").map(PathBuf::from));
    match path {
        Some(p) => hub.resolve_path(&p, setup.model_id.as_deref()).map(Some),
        None => hub.find(setup.model_id.as_deref()),
    }
}

/// Asks (default yes) and downloads the model in the foreground.
/// `ask`: prompt on the terminal; otherwise announce and proceed.
pub fn download(setup: &EngineSetup, ask: bool) -> Result<ResolvedModel, String> {
    let hub = ModelHub::new();
    let entry = hub
        .registry()
        .lookup(setup.model_id.as_deref())
        .map_err(|e| e.to_string())?
        .clone();
    if setup.no_download {
        return Err(tr!(
            format!(
                "模型 {} 未安装；运行 `nosh model pull` 或 `nosh model import <gguf>`",
                entry.display
            ),
            format!(
                "model {} is not installed; run `nosh model pull` or `nosh model import <gguf>`",
                entry.display
            )
        ));
    }
    if net::is_offline() {
        return Err(tr!(
            format!(
                "模型 {} 未安装，且处于离线模式；在联网机器上 `nosh model pull` 后用 `nosh model import` 导入",
                entry.display
            ),
            format!(
                "model {} is not installed and offline mode is on; pull it elsewhere and `nosh model import` it",
                entry.display
            )
        ));
    }
    let question = tr!(
        format!(
            "首次使用需要下载模型 {}（{}，{}），是否继续？[Y/n] ",
            entry.display,
            gb(entry.total_size()),
            entry.license
        ),
        format!(
            "nosh needs to download {} ({}, {}) first. Continue? [Y/n] ",
            entry.display,
            gb(entry.total_size()),
            entry.license
        )
    );
    if ask && term::available() {
        match term::read_text(&question, "") {
            Some(a) if a.trim().is_empty() || a.trim().to_lowercase().starts_with('y') => {}
            _ => {
                let _ = nosh_hub::paths::ensure_private_dir(&nosh_hub::paths::state_dir());
                let _ = std::fs::write(declined_marker(), "");
                return Err(tr!(
                    "已取消下载；之后可以运行 `nosh model pull`",
                    "download declined; run `nosh model pull` later"
                )
                .to_string());
            }
        }
    } else {
        eprintln!(
            "{}",
            tr!(
                format!(
                    "nosh: 正在下载 {}（{}）",
                    entry.display,
                    gb(entry.total_size())
                ),
                format!(
                    "nosh: downloading {} ({})",
                    entry.display,
                    gb(entry.total_size())
                )
            )
        );
    }
    let opts = PullOptions {
        selection: setup.selection,
        ..PullOptions::default()
    };
    let r = hub
        .pull(Some(&entry.id), &opts, &BarProgress::new())
        .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_file(declined_marker());
    eprintln!(
        "{} {}",
        style::glyph("✔", "+"),
        tr!(
            "SHA-256 校验通过。之后可以完全断网使用。",
            "SHA-256 verified. nosh can now run fully offline."
        )
    );
    Ok(r)
}

/// First interactive start without a model: offer the download once.
pub fn offer_first_download(setup: &EngineSetup) {
    match first_download_needed(setup) {
        Ok(true) => {
            if let Err(e) = download(setup, true) {
                eprintln!("{}", style::dim(&format!("nosh: {e}")));
            }
        }
        Ok(false) => {}
        // A configured model that cannot be used (`--model-path` missing or
        // ambiguous) is reported; downloading another one would not help.
        Err(e) => eprintln!("{}", style::dim(&format!("nosh: {e}"))),
    }
}

/// Whether the first start should offer the download: only when no model is
/// found (not when the configured one cannot be resolved) and downloads are
/// allowed and were not declined.
fn first_download_needed(setup: &EngineSetup) -> Result<bool, HubError> {
    let missing = locate(setup)?.is_none();
    Ok(missing && !setup.no_download && !net::is_offline() && !declined_marker().exists())
}

/// Loads the engine, downloading the model first if needed.
pub fn load(setup: &EngineSetup, ask: bool) -> Result<LoadedEngine, String> {
    let resolved = match locate(setup).map_err(|e| e.to_string())? {
        Some(r) => r,
        None => download(setup, ask)?,
    };
    let terminal = style::stderr();
    if terminal.tty {
        let status = format!(
            "{} {}",
            style::glyph("…", "..."),
            tr!(
                format!("加载 {}", resolved.entry.display),
                format!("loading {}", resolved.entry.display)
            )
        );
        if terminal.ansi {
            let status = style::clip_line(
                &status,
                term::stderr_columns().unwrap_or(80).saturating_sub(1),
                0,
                "",
            );
            eprint!("{}", style::dim(&status));
        } else {
            eprintln!("{status}");
        }
    }
    let engine = LocalChatEngine::load(
        &resolved,
        LocalEngineOptions {
            context_length: setup.context_length,
            seed: setup.seed,
            ..LocalEngineOptions::default()
        },
    );
    if terminal.ansi {
        eprint!("\r\x1b[K");
    }
    let engine =
        engine.map_err(|e| format!("failed to load {}: {e}", resolved.weights.display()))?;
    let info = engine.info();
    let description = format!(
        "{} · {} · ctx {} · {} threads · loaded in {:.1}s",
        resolved.entry.display,
        resolved.weights.display(),
        info.context,
        info.threads,
        info.load_secs
    );
    let trace = std::env::var_os("NOSH_EVAL_TRACE").filter(|p| !p.is_empty());
    let metadata = serde_json::json!({
        "model": resolved.entry.id,
        "context_length": info.context,
        "threads": info.threads,
        "load_s": info.load_secs,
        "kv_dtype": format!("{:?}", info.kv_dtype),
    });
    let engine = crate::eval_trace::wrap(
        Box::new(engine),
        trace.as_deref().map(std::path::Path::new),
        metadata,
    )
    .map_err(|e| format!("evaluation trace: {e}"))?;
    Ok(LoadedEngine {
        engine,
        description,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup(model_path: PathBuf) -> EngineSetup {
        EngineSetup {
            model_id: None,
            model_path: Some(model_path),
            context_length: 8192,
            seed: None,
            no_download: false,
            selection: SourceSelection::Auto,
        }
    }

    #[test]
    fn an_unusable_model_path_is_reported_instead_of_downloading() {
        let dir = std::env::temp_dir().join(format!("nosh-engine-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // Missing.
        let err = first_download_needed(&setup(dir.join("missing.gguf"))).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
        // Ambiguous: several GGUFs, none named for the requested model.
        std::fs::write(dir.join("a.gguf"), b"a").unwrap();
        std::fs::write(dir.join("b.gguf"), b"b").unwrap();
        let err = first_download_needed(&setup(dir.clone())).unwrap_err();
        assert!(err.to_string().contains("a.gguf, b.gguf"), "{err}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
