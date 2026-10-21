//! Model selection, authorized downloads and local engine assembly.

use std::path::{Path, PathBuf};

use nosh_core::{EngineLoadError, LoadMode, LoadedEngine};
use nosh_engine::{CancelHandle, SamplingParams};
use nosh_hub::{
    BarProgress, HubError, ModelEntry, ModelHub, Progress, PullOptions, Registry, ResolvedModel,
    SharedProgress, SourceSelection, net,
};
use nosh_llm::{LocalChatEngine, LocalEngineOptions, ModelSource};
use nosh_platform::tr;
use nosh_shell::{style, term};

#[derive(Clone, Default)]
pub struct Installation {
    pub display: nosh_shell::AssistDisplay,
    pub cancel: CancelHandle,
}

#[derive(Clone)]
pub struct EngineSetup {
    pub theme: nosh_shell::theme::ThemeHandle,
    pub device: Result<nosh_llm::InferenceDevice, String>,
    pub model_id: Result<Option<String>, String>,
    pub explicit_model: bool,
    pub model_path: Option<PathBuf>,
    pub context_length: usize,
    pub seed: Option<u64>,
    /// `--no-download` or `download.auto = "never"`.
    pub no_download: bool,
    pub selection: SourceSelection,
    pub installation: Option<Installation>,
}

impl EngineSetup {
    pub(crate) fn selected_model(&self) -> Result<Option<&str>, EngineLoadError> {
        self.model_id
            .as_ref()
            .map(|id| id.as_deref())
            .map_err(|reason| EngineLoadError::NotReady(reason.clone()))
    }

    fn check_cancelled(&self) -> Result<(), EngineLoadError> {
        if self
            .installation
            .as_ref()
            .is_some_and(|installation| installation.cancel.is_cancelled())
        {
            return Err(load_error(HubError::Cancelled));
        }
        Ok(())
    }
}

fn declined_marker() -> PathBuf {
    nosh_platform::paths::state_dir().join("download-declined")
}

fn gb(bytes: u64) -> String {
    format!("{:.2} GB", bytes as f64 / 1e9)
}

/// Where the model is, if it is available without downloading.
pub fn locate(setup: &EngineSetup) -> Result<Option<ResolvedModel>, EngineLoadError> {
    let id = setup.selected_model()?;
    let hub = ModelHub::new();
    let path = setup
        .model_path
        .clone()
        .or_else(|| std::env::var_os("NOSH_MODEL_PATH").map(PathBuf::from));
    match path {
        Some(p) => hub.resolve_path(&p, id).map(Some),
        None => hub.find(id),
    }
    .map_err(load_error)
}

pub(crate) fn model_source(model: &ResolvedModel) -> ModelSource<'_> {
    let sampling = &model.entry.sampling;
    ModelSource {
        id: &model.entry.id,
        arch: &model.entry.arch,
        chat_format: &model.entry.chat_format,
        weights: &model.weights,
        tokenizer: &model.tokenizer,
        eog_ids: &model.entry.eog_ids,
        sampling: SamplingParams {
            temperature: sampling.temperature,
            top_p: sampling.top_p,
            top_k: sampling.top_k,
            min_p: sampling.min_p,
            ..SamplingParams::default()
        },
    }
}

fn load_error(error: HubError) -> EngineLoadError {
    let reason = error.to_string();
    match error {
        HubError::Cancelled => EngineLoadError::Cancelled(reason),
        HubError::NotInstalled(_) | HubError::Offline => EngineLoadError::NotReady(reason),
        _ => EngineLoadError::Failed(reason),
    }
}

fn downloadable_model(setup: &EngineSetup, hub: &ModelHub) -> Result<ModelEntry, EngineLoadError> {
    let entry = hub
        .registry()
        .lookup(setup.selected_model()?)
        .map_err(load_error)?
        .clone();
    if setup.no_download {
        return Err(EngineLoadError::NotReady(tr!(
            format!(
                "模型 {} 未安装；运行 `nosh model pull {}` 或 `nosh model import <gguf>`",
                entry.display, entry.id
            ),
            format!(
                "model {} is not installed; run `nosh model pull {}` or `nosh model import <gguf>`",
                entry.display, entry.id
            )
        )));
    }
    if net::is_offline() {
        return Err(EngineLoadError::NotReady(tr!(
            format!(
                "模型 {} 未安装，且处于离线模式；在联网机器上 `nosh model pull {}` 后用 `nosh model import` 导入",
                entry.display, entry.id
            ),
            format!(
                "model {} is not installed and offline mode is on; run `nosh model pull {}` elsewhere and `nosh model import` it",
                entry.display, entry.id
            )
        )));
    }
    Ok(entry)
}

fn remember_declined() -> Result<(), EngineLoadError> {
    nosh_platform::paths::ensure_private_dir(&nosh_platform::paths::state_dir())
        .and_then(|()| std::fs::write(declined_marker(), ""))
        .map_err(|error| format!("cannot save model installation preference: {error}").into())
}

fn pull_model(
    setup: &EngineSetup,
    hub: &ModelHub,
    entry: &ModelEntry,
    progress: &dyn Progress,
) -> Result<ResolvedModel, EngineLoadError> {
    let opts = PullOptions {
        selection: setup.selection,
        ..PullOptions::default()
    };
    let resolved = match &setup.installation {
        Some(installation) => hub.pull_with_cancel(Some(&entry.id), &opts, progress, &|| {
            installation.cancel.is_cancelled()
        }),
        None => hub.pull(Some(&entry.id), &opts, progress),
    }
    .map_err(load_error)?;
    let _ = std::fs::remove_file(declined_marker());
    Ok(resolved)
}

/// Asks (default yes) and downloads the model in the foreground.
/// `ask`: prompt on the terminal; otherwise announce and proceed.
pub fn download(setup: &EngineSetup, ask: bool) -> Result<ResolvedModel, EngineLoadError> {
    let hub = ModelHub::new();
    let entry = downloadable_model(setup, &hub)?;
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
                if let Err(error) = remember_declined() {
                    eprintln!("nosh: {error}");
                }
                return Err(EngineLoadError::Cancelled(
                    tr!(
                        format!("已取消下载；之后可以运行 `nosh model pull {}`", entry.id),
                        format!(
                            "download declined; run `nosh model pull {}` later",
                            entry.id
                        )
                    )
                    .to_string(),
                ));
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
    let r = pull_model(setup, &hub, &entry, &BarProgress::new())?;
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

fn model_choices(registry: &Registry, selected: &str, explicit: bool) -> (Vec<String>, usize) {
    let default = registry
        .models
        .iter()
        .position(|entry| entry.id == selected)
        .expect("selected model belongs to the registry");
    let mut choices = registry
        .models
        .iter()
        .map(|entry| {
            let label = if entry.id != selected {
                ""
            } else if explicit {
                tr!(" [已选择]", " [selected]")
            } else {
                tr!(" [推荐]", " [recommended]")
            };
            format!(
                "{} ({}, {}, {}){label}",
                entry.id,
                entry.display,
                gb(entry.total_size()),
                entry.license
            )
        })
        .collect::<Vec<_>>();
    choices.push(
        tr!(
            "暂不安装，直接使用 shell",
            "Skip installation; use the shell"
        )
        .into(),
    );
    (choices, default)
}

/// Select without downloading; the caller runs the authorized work alongside the REPL.
pub fn offer_first_model(setup: &mut EngineSetup, config_path: &Path) -> Option<Installation> {
    let styles = setup.theme.snapshot().styles(style::stderr());
    let installation = match select_first_model(setup, config_path) {
        Ok(selected) => selected.then(Installation::default),
        Err(error) => {
            eprintln!("{}", styles.dim(&format!("nosh: {error}")));
            None
        }
    };
    setup.installation = installation.clone();
    installation
}

fn select_first_model(
    setup: &mut EngineSetup,
    config_path: &Path,
) -> Result<bool, EngineLoadError> {
    if !first_download_needed(setup)? || !term::available() {
        return Ok(false);
    }
    let registry = Registry::builtin();
    let selected = registry
        .lookup(setup.selected_model()?)
        .map_err(load_error)?;
    let (choices, default) = model_choices(&registry, &selected.id, setup.explicit_model);
    term::flush_input().map_err(|error| format!("cannot prepare model selection: {error}"))?;
    eprintln!(
        "{}",
        tr!(
            "选择要安装的模型；确认后进入 shell，在后台自动下载并加载：",
            "Choose a model to install; enter the shell while it downloads and loads in the background:"
        )
    );
    for (index, choice) in choices.iter().enumerate() {
        eprintln!("  {}. {choice}", index + 1);
    }
    let choice = term::read_choice(
        tr!(
            "选择 [↑/↓ 或数字，Enter 确认，Esc 跳过]: ",
            "Model [Up/Down or number, Enter to select, Esc to skip]: "
        ),
        &choices,
        default,
    )
    .map_err(|error| format!("cannot select a model: {error}"))?;
    let Some(entry) = choice.and_then(|index| registry.models.get(index)) else {
        remember_declined()?;
        eprintln!(
            "{}",
            tr!(
                "nosh: 已跳过模型安装；之后可用 `nosh model pull ID` 安装。",
                "nosh: model installation skipped; use `nosh model pull ID` later."
            )
        );
        return Ok(false);
    };
    setup.model_id = Ok(Some(entry.id.clone()));
    if let Err(error) = crate::config::save_model(config_path, &entry.id) {
        eprintln!(
            "nosh: {}: {error}",
            tr!(
                "无法保存模型选择，仅本次会话使用",
                "cannot save model selection; using it for this session only"
            )
        );
    }
    Ok(true)
}

/// Whether the first start should offer the download: only when no model is
/// found (not when the configured one cannot be resolved) and downloads are
/// allowed and were not declined.
fn first_download_needed(setup: &EngineSetup) -> Result<bool, EngineLoadError> {
    let missing = locate(setup)?.is_none();
    Ok(missing && !setup.no_download && !net::is_offline() && !declined_marker().exists())
}

/// Ordinary background assistance uses installed models only.
pub fn load(setup: &EngineSetup, mode: LoadMode) -> Result<LoadedEngine, EngineLoadError> {
    setup.selected_model()?;
    setup.check_cancelled()?;
    let (engine, description, metadata) = if let Some(path) = std::env::var_os("NOSH_EVAL_WORKER") {
        crate::eval_worker::connect(std::path::Path::new(&path), setup)?
    } else {
        let (engine, description, metadata) = load_local(setup, mode)?;
        (
            Box::new(engine) as Box<dyn nosh_engine::ChatEngine>,
            description,
            metadata,
        )
    };
    let trace = std::env::var_os("NOSH_EVAL_TRACE").filter(|p| !p.is_empty());
    let engine =
        crate::eval_trace::wrap(engine, trace.as_deref().map(std::path::Path::new), metadata)
            .map_err(|e| format!("evaluation trace: {e}"))?;
    Ok(LoadedEngine {
        engine,
        description,
    })
}

struct ModelProgress<'a> {
    shared: SharedProgress,
    display: &'a nosh_shell::AssistDisplay,
    model: &'a str,
}

impl ModelProgress<'_> {
    fn publish(&self) {
        let status = self.shared.state.lock().unwrap().clone();
        self.display.set_model_status(Some(tr!(
            format!(
                "AI: 下载 {}% {} ({})",
                status.percent(),
                self.model,
                status.file
            ),
            format!(
                "AI: downloading {}% {} ({})",
                status.percent(),
                self.model,
                status.file
            )
        )));
    }
}

impl Progress for ModelProgress<'_> {
    fn start(&self, name: &str, total: u64, already: u64) {
        self.shared.start(name, total, already);
        self.publish();
    }

    fn advance(&self, pos: u64) {
        self.shared.advance(pos);
        self.publish();
    }

    fn note(&self, message: &str) {
        self.shared.note(message);
        self.display.set_model_status(Some(tr!(
            format!("AI: 下载 {}: {message}", self.model),
            format!("AI: downloading {}: {message}", self.model)
        )));
    }

    fn finish(&self, ok: bool) {
        self.shared.finish(ok);
        self.display.set_model_status(Some(if ok {
            tr!(
                format!("AI: {} 校验通过", self.model),
                format!("AI: {} verified", self.model)
            )
        } else {
            tr!(
                format!("AI: {} 下载已停止", self.model),
                format!("AI: {} download stopped", self.model)
            )
        }));
    }
}

fn download_selected(
    setup: &EngineSetup,
    mode: LoadMode,
    installation: &Installation,
) -> Result<ResolvedModel, EngineLoadError> {
    let hub = ModelHub::new();
    let entry = downloadable_model(setup, &hub)?;
    if mode == LoadMode::Foreground {
        return pull_model(setup, &hub, &entry, &BarProgress::new());
    }
    let progress = ModelProgress {
        shared: SharedProgress::default(),
        display: &installation.display,
        model: &entry.id,
    };
    progress.note(tr!("正在准备下载", "preparing download"));
    pull_model(setup, &hub, &entry, &progress)
}

pub(crate) fn load_local(
    setup: &EngineSetup,
    mode: LoadMode,
) -> Result<(LocalChatEngine, String, serde_json::Value), EngineLoadError> {
    setup.check_cancelled()?;
    let device = setup.device.clone()?;
    let background = mode == LoadMode::Background;
    let resolved = match (locate(setup)?, &setup.installation) {
        (Some(r), _) => r,
        (None, Some(installation)) => download_selected(setup, mode, installation)?,
        (None, None) if background => {
            let hub = ModelHub::new();
            let entry = hub
                .registry()
                .lookup(setup.selected_model()?)
                .map_err(load_error)?;
            return Err(load_error(HubError::NotInstalled(entry.id.clone())));
        }
        (None, None) => download(setup, true)?,
    };
    setup.check_cancelled()?;
    if let Some(installation) = &setup.installation {
        installation.display.set_model_status(Some(tr!(
            format!("AI: 加载 {}", resolved.entry.id),
            format!("AI: loading {}", resolved.entry.id)
        )));
    }
    let terminal = style::stderr();
    if terminal.tty && !background {
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
            eprint!("{}", setup.theme.snapshot().styles(terminal).dim(&status));
        } else {
            eprintln!("{status}");
        }
    }
    let engine = LocalChatEngine::load(
        model_source(&resolved),
        LocalEngineOptions {
            device,
            context_length: setup.context_length,
            seed: setup.seed,
            ..LocalEngineOptions::default()
        },
    );
    if terminal.ansi && !background {
        eprint!("\r\x1b[K");
    }
    let engine =
        engine.map_err(|e| format!("failed to load {}: {e}", resolved.weights.display()))?;
    setup.check_cancelled()?;
    let info = engine.info();
    if !background && info.device_selection.requested == nosh_llm::InferenceDevice::Auto {
        eprintln!(
            "nosh: device auto -> {}: {}",
            info.device, info.device_selection.reason
        );
    }
    let description = format!(
        "{} · {} · {} ({}: {}) · ctx {} · {} threads · loaded in {:.1}s",
        resolved.entry.display,
        resolved.weights.display(),
        info.device,
        info.device_selection.requested,
        info.device_selection.reason,
        info.context,
        info.threads,
        info.load_secs
    );
    let metadata = serde_json::json!({
        "model": resolved.entry.id,
        "arch": info.arch,
        "chat_format": info.chat_format,
        "recurrent_state_dtype": info.recurrent_state_dtype,
        "device": info.device.to_string(),
        "device_requested": info.device_selection.requested.to_string(),
        "device_reason": info.device_selection.reason,
        "cuda_required_bytes": info.device_selection.required_cuda_bytes,
        "cuda_free_bytes": info.device_selection.free_cuda_bytes,
        "cuda_compiled": cfg!(feature = "cuda"),
        "context_length": info.context,
        "threads": info.threads,
        "load_s": info.load_secs,
        "device_init_s": info.device_init_secs,
        "model_init_s": info.model_init_secs,
        "kv_dtype": format!("{:?}", info.kv_dtype),
    });
    Ok((engine, description, metadata))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_source_preserves_registry_metadata_and_sampling_defaults() {
        let mut entry = nosh_hub::Registry::builtin().default_model().clone();
        entry.sampling.temperature = 0.4;
        entry.sampling.top_p = 0.8;
        entry.sampling.top_k = 12;
        entry.sampling.min_p = 0.1;
        let model = ResolvedModel {
            entry,
            dir: "model".into(),
            weights: "weights.gguf".into(),
            tokenizer: "tokenizer.json".into(),
        };
        let source = model_source(&model);
        assert_eq!(source.id, model.entry.id);
        assert_eq!(source.arch, model.entry.arch);
        assert_eq!(source.chat_format, model.entry.chat_format);
        assert_eq!(source.weights, model.weights);
        assert_eq!(source.tokenizer, model.tokenizer);
        assert_eq!(source.eog_ids, model.entry.eog_ids);
        assert_eq!(
            source.sampling,
            SamplingParams {
                temperature: 0.4,
                top_p: 0.8,
                top_k: 12,
                min_p: 0.1,
                ..SamplingParams::default()
            }
        );
    }

    fn setup(model_path: PathBuf) -> EngineSetup {
        EngineSetup {
            theme: Default::default(),
            device: Ok(nosh_llm::InferenceDevice::Cpu),
            model_id: Ok(None),
            explicit_model: true,
            model_path: Some(model_path),
            context_length: 8192,
            seed: None,
            no_download: false,
            selection: SourceSelection::Auto,
            installation: None,
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

    #[test]
    fn resource_disabled_setup_cannot_locate_download_or_load_a_model() {
        let reason = "fixture: available resources below 3.5 GiB";
        let mut setup = setup("must-not-be-opened.gguf".into());
        setup.model_id = Err(reason.into());
        let expected = EngineLoadError::NotReady(reason.into());
        assert_eq!(locate(&setup).unwrap_err(), expected);
        assert_eq!(first_download_needed(&setup).unwrap_err(), expected);
        assert_eq!(download(&setup, false).unwrap_err(), expected);
        assert_eq!(
            load_local(&setup, LoadMode::Foreground)
                .err()
                .expect("local loader must honor the gate"),
            expected
        );
        for installation in [None, Some(Installation::default())] {
            setup.installation = installation;
            for mode in [LoadMode::Foreground, LoadMode::Background] {
                let error = load(&setup, mode)
                    .err()
                    .expect("resource gate must run before any model or worker");
                assert_eq!(error, expected);
            }
        }
    }

    #[test]
    fn picker_preselects_the_resource_recommendation_and_lists_all_models() {
        let registry = Registry::builtin();
        for recommended in [
            "minicpm5-2b:q8_0",
            "minicpm5-2b:q4_k_m",
            "qwen3.5-0.8b:q8_0",
        ] {
            let (choices, default) = model_choices(&registry, recommended, false);
            assert_eq!(choices.len(), registry.models.len() + 1);
            assert_eq!(registry.models[default].id, recommended);
            assert!(choices[default].contains(tr!("推荐", "recommended")));
            assert_eq!(
                choices
                    .iter()
                    .filter(|label| label.contains(tr!("推荐", "recommended")))
                    .count(),
                1
            );
            for (choice, entry) in choices.iter().zip(&registry.models) {
                assert!(choice.contains(&entry.id));
                assert!(choice.contains(&gb(entry.total_size())));
                assert!(choice.contains(&entry.license));
            }
        }
        let requested = "minicpm5-1b:q4_k_m";
        let (choices, default) = model_choices(&registry, requested, true);
        assert_eq!(registry.models[default].id, requested);
        assert!(choices[default].contains(tr!("已选择", "selected")));
        assert!(
            !choices
                .iter()
                .any(|label| label.contains(tr!("推荐", "recommended")))
        );
    }

    #[test]
    fn selected_download_progress_survives_edits_and_records_each_file() {
        let display = nosh_shell::AssistDisplay::default();
        let progress = ModelProgress {
            shared: SharedProgress::default(),
            display: &display,
            model: "fixture:q4",
        };
        progress.start("weights.gguf", 1000, 250);
        assert!(display.model_status().unwrap().contains("25%"));
        progress.advance(800);
        display.invalidate();
        let status = display.model_status().unwrap();
        assert!(
            status.contains("80%") && status.contains("weights.gguf"),
            "{status}"
        );
        progress.note("fixture source changed");
        assert!(
            display
                .model_status()
                .unwrap()
                .contains("fixture source changed")
        );
        progress.finish(false);
        assert!(progress.shared.state.lock().unwrap().failed);
        progress.start("tokenizer.json", 100, 0);
        assert!(display.model_status().unwrap().contains("0%"));
        assert!(!progress.shared.state.lock().unwrap().failed);
        progress.advance(100);
        assert!(display.model_status().unwrap().contains("100%"));
        progress.finish(true);
        let snapshot = progress.shared.state.lock().unwrap().clone();
        assert!(snapshot.done && !snapshot.failed);
        assert_eq!(snapshot.percent(), 100);
    }

    #[test]
    fn selecting_a_model_does_not_override_disabled_downloads_or_cancellation() {
        let mut setup = setup("must-not-be-opened.gguf".into());
        setup.no_download = true;
        assert!(matches!(
            download_selected(&setup, LoadMode::Background, &Installation::default(),),
            Err(EngineLoadError::NotReady(_))
        ));
        let installation = Installation::default();
        installation.cancel.cancel();
        setup.installation = Some(installation);
        assert!(matches!(
            load(&setup, LoadMode::Background),
            Err(EngineLoadError::Cancelled(_))
        ));
    }
}
#[test]
fn loader_error_categories_do_not_depend_on_error_text() {
    assert_eq!(
        load_error(HubError::Cancelled),
        EngineLoadError::Cancelled("download cancelled".into())
    );
    assert!(matches!(
        load_error(HubError::NotInstalled("fixture".into())),
        EngineLoadError::NotReady(_)
    ));
    assert!(matches!(
        load_error(HubError::UnknownModel("fixture".into())),
        EngineLoadError::Failed(_)
    ));
    assert_eq!(EngineLoadError::Cancelled("stop".into()).exit_code(), 130);
    assert_eq!(EngineLoadError::NotReady("missing".into()).exit_code(), 2);
    assert_eq!(EngineLoadError::Failed("bad".into()).exit_code(), 2);
}
