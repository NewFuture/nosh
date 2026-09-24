//! Model registry, source selection, resumable verified downloads and import.
//!
//! All network access goes through [`net`], which refuses every request when
//! offline mode is on (`--offline`, `NOSH_OFFLINE=1`, `HF_HUB_OFFLINE=1`).

pub mod download;
pub mod hash;
pub mod lang;
pub mod net;
pub mod paths;
pub mod progress;
pub mod registry;
pub mod select;
pub mod sources;
pub mod store;
#[cfg(any(test, feature = "test-server"))]
pub mod testserver;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use progress::{BarProgress, DownloadStatus, NoProgress, Progress, SharedProgress};
pub use registry::{FileEntry, FileRole, ModelEntry, Registry};
pub use sources::{Hub, Region};
pub use store::{FileState, ResolvedModel};

#[derive(Debug, thiserror::Error)]
pub enum HubError {
    #[error("offline mode is on; network access is disabled")]
    Offline,
    #[error("download cancelled")]
    Cancelled,
    #[error("unknown model '{0}' (see `nosh model list`)")]
    UnknownModel(String),
    #[error("model {0} is not installed; run `nosh model pull` or `nosh model import`")]
    NotInstalled(String),
    #[error("registry error: {0}")]
    Registry(String),
    #[error("network error: {0}")]
    Network(String),
    #[error("all download sources failed: {0}")]
    AllSourcesFailed(String),
    #[error("SHA-256 mismatch for {file}: expected {expected}, got {actual}")]
    ChecksumMismatch {
        file: String,
        expected: String,
        actual: String,
    },
    #[error("not enough disk space: need {needed} bytes, {available} available")]
    InsufficientSpace { needed: u64, available: u64 },
    #[error("{0}")]
    Import(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// How to pick a download source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SourceSelection {
    /// Region preference + parallel speed probe.
    #[default]
    Auto,
    Fixed(Hub),
}

impl SourceSelection {
    pub fn parse(s: &str) -> Option<Self> {
        if s.eq_ignore_ascii_case("auto") {
            Some(Self::Auto)
        } else {
            Hub::parse(s).map(Self::Fixed)
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct PullOptions {
    pub selection: SourceSelection,
    pub download: download::DownloadOptions,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct DownloadState {
    best: Option<String>,
}

/// Status of one registry model in the user store.
#[derive(Debug, Clone)]
pub struct ModelStatus {
    pub entry: ModelEntry,
    pub dir: PathBuf,
    pub weights: FileState,
    pub tokenizer: FileState,
    pub found: Option<ResolvedModel>,
}

impl ModelStatus {
    pub fn installed(&self) -> bool {
        self.found.is_some()
    }
}

pub struct ModelHub {
    registry: Registry,
    user_dir: PathBuf,
    search_dirs: Vec<PathBuf>,
    state_dir: PathBuf,
}

impl Default for ModelHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ModelHub {
    pub fn new() -> Self {
        let user_dir = paths::models_dir();
        let mut search_dirs = Vec::new();
        if let Some(p) = paths::portable_models_dir() {
            search_dirs.push(p);
        }
        search_dirs.push(user_dir.clone());
        if let Some(s) = paths::system_models_dir() {
            search_dirs.push(s);
        }
        Self {
            registry: Registry::builtin(),
            user_dir,
            search_dirs,
            state_dir: paths::state_dir(),
        }
    }

    /// A hub rooted at `root` (models in `root/models`), for tests.
    pub fn with_root(root: &Path, registry: Registry) -> Self {
        let user_dir = root.join("models");
        Self {
            registry,
            search_dirs: vec![user_dir.clone()],
            user_dir,
            state_dir: root.join("state"),
        }
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    pub fn user_model_dir(&self, entry: &ModelEntry) -> PathBuf {
        self.user_dir.join(entry.dir_name())
    }

    /// Finds an installed, verified model (portable → user → system store).
    pub fn find(&self, id: Option<&str>) -> Result<Option<ResolvedModel>, HubError> {
        let entry = self.registry.lookup(id)?;
        for base in &self.search_dirs {
            if let Some(r) = store::check_dir(&base.join(entry.dir_name()), entry)? {
                return Ok(Some(r));
            }
        }
        Ok(None)
    }

    /// The weights file in `dir`: its only `.gguf`, or with several, the one
    /// named like the requested model's registry file (the default model
    /// when none is requested). Anything else is ambiguous and an error that
    /// lists the candidates.
    fn pick_gguf(&self, dir: &Path, id: Option<&str>) -> Result<PathBuf, HubError> {
        let mut ggufs: Vec<PathBuf> = fs::read_dir(dir)?
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.is_file()
                    && p.extension()
                        .is_some_and(|x| x.eq_ignore_ascii_case("gguf"))
            })
            .collect();
        ggufs.sort();
        match ggufs.len() {
            0 => Err(HubError::Import(format!("no .gguf in {}", dir.display()))),
            1 => Ok(ggufs.remove(0)),
            _ => {
                let entry = self.registry.lookup(id)?;
                let wanted = &entry.weights().name;
                if let Some(p) = ggufs
                    .iter()
                    .find(|p| p.file_name().is_some_and(|n| n == wanted.as_str()))
                {
                    return Ok(p.clone());
                }
                let names: Vec<String> = ggufs
                    .iter()
                    .filter_map(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
                    .collect();
                Err(HubError::Import(format!(
                    "{} holds several .gguf files and none is {wanted} ({}); pass the file itself to --model-path, or --model for one of: {}",
                    dir.display(),
                    entry.id,
                    names.join(", ")
                )))
            }
        }
    }

    /// Resolves an explicit `.gguf` path (or a directory holding one).
    pub fn resolve_path(&self, path: &Path, id: Option<&str>) -> Result<ResolvedModel, HubError> {
        let (dir, weights) = if path.is_dir() {
            (path.to_path_buf(), self.pick_gguf(path, id)?)
        } else if path.is_file() {
            (
                path.parent().map(Path::to_path_buf).unwrap_or_default(),
                path.to_path_buf(),
            )
        } else {
            return Err(HubError::Import(format!(
                "{} does not exist",
                path.display()
            )));
        };
        let size = fs::metadata(&weights)?.len();
        let fname = weights
            .file_name()
            .map(|n| n.to_string_lossy().into_owned());
        let entry = self
            .registry
            .models
            .iter()
            .find(|m| m.weights().size == size && Some(&m.weights().name) == fname.as_ref())
            .cloned()
            .unwrap_or_else(|| {
                self.registry
                    .lookup(id)
                    .cloned()
                    .unwrap_or_else(|_| self.registry.default_model().clone())
            });
        let tokenizer = if dir.join("tokenizer.json").is_file() {
            dir.join("tokenizer.json")
        } else if let Some(found) = self.find(Some(&entry.id)).ok().flatten() {
            found.tokenizer
        } else {
            self.user_model_dir(&entry).join(&entry.tokenizer().name)
        };
        if !tokenizer.is_file() {
            return Err(HubError::Import(format!(
                "tokenizer.json not found next to {} (use `nosh model import --tokenizer`)",
                weights.display()
            )));
        }
        Ok(ResolvedModel {
            entry,
            dir,
            weights,
            tokenizer,
        })
    }

    pub fn status(&self, entry: &ModelEntry) -> ModelStatus {
        let dir = self.user_model_dir(entry);
        let manifest = store::read_manifest(&dir);
        let found = self.search_dirs.iter().find_map(|b| {
            store::check_dir(&b.join(entry.dir_name()), entry)
                .ok()
                .flatten()
        });
        ModelStatus {
            entry: entry.clone(),
            weights: store::file_state(&dir, entry.weights(), manifest.as_ref()),
            tokenizer: store::file_state(&dir, entry.tokenizer(), manifest.as_ref()),
            found,
            dir,
        }
    }

    fn load_state(&self) -> DownloadState {
        fs::read_to_string(self.state_dir.join("download.json"))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save_state(&self, st: &DownloadState) {
        if paths::ensure_private_dir(&self.state_dir).is_ok() {
            let _ = fs::write(
                self.state_dir.join("download.json"),
                serde_json::to_string_pretty(st).unwrap_or_default(),
            );
        }
    }

    /// Hub preference: last best source first, then the regional order.
    pub fn preferred_hubs(&self, region: Region) -> Vec<Hub> {
        let mut order: Vec<Hub> = region.hub_order().to_vec();
        if let Some(best) = self.load_state().best.as_deref().and_then(Hub::parse) {
            order.retain(|h| *h != best);
            order.insert(0, best);
        }
        order
    }

    /// Downloads (or resumes) a model and its tokenizer, verifying both.
    pub fn pull(
        &self,
        id: Option<&str>,
        opts: &PullOptions,
        progress: &dyn Progress,
    ) -> Result<ResolvedModel, HubError> {
        let entry = self.registry.lookup(id)?.clone();
        if let Some(found) = self.find(Some(&entry.id))? {
            return Ok(found);
        }
        if net::is_offline() {
            return Err(HubError::Offline);
        }
        net::clear_cancel();
        let dir = self.user_model_dir(&entry);
        paths::ensure_private_dir(&dir)?;
        let endpoints = sources::Endpoints::from_env();
        let preferred = self.preferred_hubs(sources::current_region());
        let weight_cands = sources::candidates(entry.weights(), &endpoints);

        let manifest = store::read_manifest(&dir);
        let weights_needed = !matches!(
            store::file_state(&dir, entry.weights(), manifest.as_ref()),
            FileState::Present { verified: true }
        );
        let hub_order: Vec<Hub> = match opts.selection {
            SourceSelection::Fixed(h) => {
                let mut v = preferred.clone();
                v.retain(|x| *x != h);
                v.insert(0, h);
                v
            }
            SourceSelection::Auto if weights_needed && weight_cands.len() > 1 => {
                let probes = select::probe_all(&weight_cands, Duration::from_secs(3));
                let ranked = select::rank(&weight_cands, &probes, &preferred);
                let summary = select::format_probe_summary(&probes);
                progress.note(&tr!(
                    format!("测速：{summary}"),
                    format!("speed test: {summary}")
                ));
                ranked.iter().map(|c| c.hub).collect()
            }
            SourceSelection::Auto => preferred.clone(),
        };

        let mut best_hub = None;
        for role in [FileRole::Tokenizer, FileRole::Weights] {
            let file = entry.file(role);
            let manifest = store::read_manifest(&dir);
            if let FileState::Present { verified: true } =
                store::file_state(&dir, file, manifest.as_ref())
            {
                continue;
            }
            let cands =
                select::order_by_preference(&sources::candidates(file, &endpoints), &hub_order);
            let out = download::download_file(file, &dir, &cands, &opts.download, progress)?;
            let used = out.hub.or(cands.first().map(|c| c.hub));
            let rev = cands
                .iter()
                .find(|c| Some(c.hub) == used)
                .map(|c| c.revision.clone())
                .unwrap_or_default();
            store::record_verified(
                &dir,
                &entry.id,
                file,
                used.map(|h| h.name()).unwrap_or("local"),
                &rev,
            )?;
            if role == FileRole::Weights {
                best_hub = used;
            }
        }
        if let Some(h) = best_hub {
            self.save_state(&DownloadState {
                best: Some(h.name().to_string()),
            });
        }
        self.find(Some(&entry.id))?
            .ok_or_else(|| HubError::NotInstalled(entry.id.clone()))
    }

    /// Recomputes the SHA-256 of every installed file of a model.
    pub fn verify(
        &self,
        id: Option<&str>,
        mut on_file: impl FnMut(&FileEntry, &Result<(), String>),
    ) -> Result<bool, HubError> {
        let entry = self.registry.lookup(id)?;
        let dir = self
            .search_dirs
            .iter()
            .map(|b| b.join(entry.dir_name()))
            .find(|d| d.join(&entry.weights().name).exists())
            .ok_or_else(|| HubError::NotInstalled(entry.id.clone()))?;
        let mut all_ok = true;
        for f in &entry.files {
            let path = dir.join(&f.name);
            let before = store::FileStamp::of(&path);
            let res = match hash::sha256_file(&path, |_| {}) {
                Ok(sha) if sha.eq_ignore_ascii_case(&f.sha256) => {
                    if let Some(stamp) = &before {
                        let _ = store::record_verified_as(&dir, &entry.id, f, "local", "", stamp);
                    }
                    Ok(())
                }
                Ok(sha) => Err(format!("SHA-256 mismatch (got {sha})")),
                Err(e) => Err(e.to_string()),
            };
            all_ok &= res.is_ok();
            on_file(f, &res);
        }
        Ok(all_ok)
    }

    /// Imports a local `.gguf` (identified by SHA-256) and optionally its tokenizer.
    pub fn import(
        &self,
        gguf: &Path,
        tokenizer: Option<&Path>,
        progress: &dyn Progress,
    ) -> Result<ResolvedModel, HubError> {
        progress.note(&tr!(
            format!("计算 {} 的 SHA-256…", gguf.display()),
            format!("hashing {}…", gguf.display())
        ));
        let sha = hash::sha256_file(gguf, |_| {})?;
        let (entry, file) = self
            .registry
            .find_by_sha256(&sha)
            .filter(|(_, f)| f.role == FileRole::Weights)
            .ok_or_else(|| {
                HubError::Import(format!(
                    "{} (sha256 {sha}) is not a known model in the registry",
                    gguf.display()
                ))
            })?;
        let (entry, file) = (entry.clone(), file.clone());
        let dir = self.user_model_dir(&entry);
        paths::ensure_private_dir(&dir)?;
        copy_into(gguf, &dir.join(&file.name))?;
        store::record_verified(&dir, &entry.id, &file, "import", "")?;

        let tok_entry = entry.tokenizer().clone();
        let sibling = gguf.parent().map(|p| p.join("tokenizer.json"));
        let tok_src = tokenizer
            .map(Path::to_path_buf)
            .or(sibling.filter(|p| p.is_file()));
        if let Some(src) = tok_src {
            let tsha = hash::sha256_file(&src, |_| {})?;
            if !tsha.eq_ignore_ascii_case(&tok_entry.sha256) {
                return Err(HubError::ChecksumMismatch {
                    file: src.display().to_string(),
                    expected: tok_entry.sha256,
                    actual: tsha,
                });
            }
            copy_into(&src, &dir.join(&tok_entry.name))?;
            store::record_verified(&dir, &entry.id, &tok_entry, "import", "")?;
        }
        self.find(Some(&entry.id))?.ok_or_else(|| {
            HubError::Import(format!(
                "imported weights for {}, but tokenizer.json is missing; pass --tokenizer or run `nosh model pull`",
                entry.id
            ))
        })
    }
}

fn copy_into(src: &Path, dst: &Path) -> Result<(), HubError> {
    if let (Ok(a), Ok(b)) = (fs::canonicalize(src), fs::canonicalize(dst))
        && a == b
    {
        return Ok(());
    }
    let tmp = dst.with_extension("importing");
    if fs::hard_link(src, &tmp).is_err() {
        fs::copy(src, &tmp)?;
    }
    fs::rename(&tmp, dst)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::SourceRef;
    use crate::testserver::{Behavior, TestServer, tempdir};

    fn sha(d: &[u8]) -> String {
        use sha2::Digest;
        let mut h = sha2::Sha256::new();
        h.update(d);
        hash::finalize_hex(h)
    }

    /// A registry whose single model is served by `srv` via a custom HF endpoint.
    fn test_registry(weights: &[u8], tok: &[u8]) -> Registry {
        let toml = format!(
            r#"
schema = 1
[[model]]
id = "tiny:q4"
default = true
display = "tiny"
arch = "llama"
chat_format = "minicpm5"
context_max = 128
eog_ids = [1]
license = "Apache-2.0"
sampling = {{ temperature = 1.0, top_p = 1.0, min_p = 0.0 }}
  [[model.files]]
  role = "weights"
  name = "tiny.gguf"
  size = {}
  sha256 = "{}"
  sources = [{{ hub = "hf", repo = "o/tiny", revision = "r1" }}]
  [[model.files]]
  role = "tokenizer"
  name = "tokenizer.json"
  size = {}
  sha256 = "{}"
  sources = [{{ hub = "hf", repo = "o/tiny", revision = "r1" }}]
"#,
            weights.len(),
            sha(weights),
            tok.len(),
            sha(tok)
        );
        Registry::parse(&toml).unwrap()
    }

    #[test]
    fn import_identifies_by_hash() {
        let w = b"GGUF-tiny-weights".to_vec();
        let t = b"{\"model\":{}}".to_vec();
        let root = tempdir("hub-import");
        let src = tempdir("hub-import-src");
        fs::write(src.join("anything.gguf"), &w).unwrap();
        fs::write(src.join("tokenizer.json"), &t).unwrap();
        let hub = ModelHub::with_root(&root, test_registry(&w, &t));
        let r = hub
            .import(&src.join("anything.gguf"), None, &NoProgress)
            .unwrap();
        assert_eq!(r.entry.id, "tiny:q4");
        assert_eq!(fs::read(&r.weights).unwrap(), w);
        assert!(hub.find(None).unwrap().is_some());
        let mut n = 0;
        assert!(
            hub.verify(None, |_, r| {
                assert!(r.is_ok());
                n += 1;
            })
            .unwrap()
        );
        assert_eq!(n, 2);

        fs::write(src.join("other.gguf"), b"unknown").unwrap();
        assert!(matches!(
            hub.import(&src.join("other.gguf"), None, &NoProgress),
            Err(HubError::Import(_))
        ));
    }

    #[test]
    fn model_path_directories_pick_the_requested_models_file() {
        let root = tempdir("hub-pick");
        let dir = tempdir("hub-pick-dir");
        let hub = ModelHub::with_root(&root, Registry::builtin());
        let q4 = hub.registry().lookup(None).unwrap().weights().name.clone();
        let q8 = hub
            .registry()
            .lookup(Some("minicpm5-2b:q8_0"))
            .unwrap()
            .weights()
            .name
            .clone();
        fs::write(dir.join(&q4), b"q4").unwrap();
        fs::write(dir.join(&q8), b"q8").unwrap();
        fs::write(dir.join("tokenizer.json"), b"{}").unwrap();
        // The requested model's file, and the default model's without --model.
        let r = hub.resolve_path(&dir, Some("minicpm5-2b:q8_0")).unwrap();
        assert_eq!(r.weights, dir.join(&q8));
        assert_eq!(hub.resolve_path(&dir, None).unwrap().weights, dir.join(&q4));
        // Several files, none named for the request: an error naming them.
        let err = hub
            .resolve_path(&dir, Some("minicpm5-1b:q4_k_m"))
            .unwrap_err()
            .to_string();
        assert!(err.contains(&q4) && err.contains(&q8), "{err}");
        let other = tempdir("hub-pick-other");
        fs::write(other.join("a.gguf"), b"a").unwrap();
        fs::write(other.join("b.gguf"), b"b").unwrap();
        fs::write(other.join("tokenizer.json"), b"{}").unwrap();
        let err = hub.resolve_path(&other, None).unwrap_err().to_string();
        assert!(err.contains("a.gguf, b.gguf"), "{err}");
        // A single file is unambiguous whatever its name.
        fs::remove_file(other.join("b.gguf")).unwrap();
        assert_eq!(
            hub.resolve_path(&other, None).unwrap().weights,
            other.join("a.gguf")
        );
    }

    #[test]
    fn preferred_hubs_remember_best() {
        let root = tempdir("hub-pref");
        let hub = ModelHub::with_root(&root, Registry::builtin());
        assert_eq!(
            hub.preferred_hubs(Region::MainlandChina)[0],
            Hub::ModelScope
        );
        hub.save_state(&DownloadState {
            best: Some("huggingface.co".into()),
        });
        assert_eq!(
            hub.preferred_hubs(Region::MainlandChina),
            vec![Hub::HuggingFace, Hub::ModelScope, Hub::HfMirror]
        );
    }

    #[test]
    fn source_selection_parse() {
        assert_eq!(SourceSelection::parse("auto"), Some(SourceSelection::Auto));
        assert_eq!(
            SourceSelection::parse("modelscope"),
            Some(SourceSelection::Fixed(Hub::ModelScope))
        );
        assert_eq!(SourceSelection::parse("?"), None);
    }

    #[test]
    fn candidates_skip_unknown_hubs() {
        let f = FileEntry {
            role: FileRole::Weights,
            name: "x".into(),
            size: 1,
            sha256: "0".repeat(64),
            sources: vec![SourceRef {
                hub: "gitee".into(),
                repo: "a/b".into(),
                revision: "main".into(),
            }],
        };
        assert!(sources::candidates(&f, &sources::Endpoints::default()).is_empty());
    }

    #[test]
    fn pull_via_custom_endpoint() {
        // Serve both files from one server; the test server ignores the path.
        let w: Vec<u8> = (0..3000u32).map(|i| (i % 256) as u8).collect();
        let srv = TestServer::start(w.clone(), Behavior::Normal);
        let root = tempdir("hub-pull");
        let reg = test_registry(&w, &w);
        let hub = ModelHub::with_root(&root, reg);
        let entry = hub.registry().default_model().clone();
        let dir = hub.user_model_dir(&entry);
        let ep = sources::Endpoints {
            hf: Some(srv.base()),
        };
        for f in &entry.files {
            let cands = sources::candidates(f, &ep);
            assert_eq!(cands.len(), 1);
            download::download_file(
                f,
                &dir,
                &cands,
                &download::DownloadOptions::default(),
                &NoProgress,
            )
            .unwrap();
            store::record_verified(&dir, &entry.id, f, "test", "r1").unwrap();
        }
        let found = hub.find(None).unwrap().unwrap();
        assert_eq!(fs::read(found.weights).unwrap(), w);
        let st = hub.status(&entry);
        assert!(st.installed());
        assert_eq!(st.weights, FileState::Present { verified: true });
    }
}
