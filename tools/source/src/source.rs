use crate::{
    Result,
    worktree::{
        apply_indexed_patch, git, git_output, git_text, has_commit, rename_directory, snapshot,
        text, unique, write_atomic,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
};

const PIN: &str = "patches/reedline/source.toml";
const PATCH: &str = "patches/reedline/nosh.patch";
const UPSTREAM: &str = "third_party/reedline-upstream";
const GENERATED: &str = ".nosh/reedline";

#[derive(Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    repository: String,
    revision: String,
}

#[derive(Clone, PartialEq, Eq)]
struct Inputs {
    source: Source,
    pin: Vec<u8>,
    patch: Vec<u8>,
}

impl Inputs {
    fn read(root: &Path) -> Result<Self> {
        let read = |path| -> Result<Vec<u8>> {
            Ok(fs::read_to_string(root.join(path))?
                .replace("\r\n", "\n")
                .into_bytes())
        };
        let pin = read(PIN)?;
        let source: Source = toml::from_str(std::str::from_utf8(&pin)?)?;
        validate_revision(&source.revision)?;
        if source.repository != "https://github.com/nushell/reedline.git" {
            return Err("Reedline source must be the official nushell/reedline repository".into());
        }
        Ok(Self {
            source,
            pin,
            patch: read(PATCH)?,
        })
    }

    fn ensure_current(&self, root: &Path, operation: &str) -> Result<()> {
        if *self != Self::read(root)? {
            return Err(format!(
                "pin/patch changed during {operation}; live source was not replaced"
            )
            .into());
        }
        Ok(())
    }
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn validate_revision(revision: &str) -> Result<()> {
    if revision.len() != 40
        || !revision
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
    {
        return Err("revision must be a full lowercase 40-character Git commit".into());
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
pub(crate) struct State {
    schema_version: u32,
    upstream_repository: String,
    upstream_revision: String,
    upstream_tree: String,
    patch_sha256: String,
    source_pin_sha256: String,
    prepared_tree: String,
    prepared_archive_sha256: String,
}

impl State {
    fn matches(&self, inputs: &Inputs) -> bool {
        self.schema_version == 1
            && self.upstream_repository == inputs.source.repository
            && self.upstream_revision == inputs.source.revision
            && self.source_pin_sha256 == sha256(&inputs.pin)
            && self.patch_sha256 == sha256(&inputs.patch)
    }
}

struct Replay {
    dir: PathBuf,
    tree: String,
    patch: Vec<u8>,
}

pub(crate) struct Manager {
    root: PathBuf,
    cache: PathBuf,
    cache_read_only: bool,
    offline: bool,
    _lock: File,
}

impl Manager {
    pub(crate) fn new(root: PathBuf, cache: Option<PathBuf>, offline: bool) -> Result<Self> {
        let root = dunce::canonicalize(root)?;
        if !root.join(PIN).is_file() || !root.join(PATCH).is_file() {
            return Err(format!(
                "{} is not a managed nosh source root; run from the repository/archive root or pass --root <path>",
                root.display()
            ).into());
        }
        fs::create_dir_all(root.join(".nosh"))?;
        let lock = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(root.join(".nosh/source.lock"))?;
        lock.try_lock()
            .map_err(|e| format!("another source command is running, or locking failed: {e}"))?;
        Ok(Self {
            cache_read_only: cache.is_some(),
            cache: cache.unwrap_or_else(|| root.join(".nosh/cache/reedline.git")),
            root,
            offline,
            _lock: lock,
        })
    }

    fn upstream(&self) -> Result<Option<PathBuf>> {
        let upstream = self.root.join(UPSTREAM);
        if !upstream.join(".git").exists() {
            return Ok(None);
        }
        if !git_text(
            &upstream,
            &["status", "--porcelain", "--untracked-files=all"],
        )?
        .is_empty()
        {
            return Err(
                "upstream submodule is dirty; preserve its edits and edit .nosh/reedline instead"
                    .into(),
            );
        }
        Ok(Some(upstream))
    }

    fn inputs(&self) -> Result<Inputs> {
        let inputs = Inputs::read(&self.root)?;
        if self.root.join(".git").exists() {
            let entry = git_text(&self.root, &["ls-files", "--stage", "--", UPSTREAM])?;
            let fields: Vec<_> = entry.split_whitespace().collect();
            if fields.len() != 4
                || fields[0] != "160000"
                || fields[1] != inputs.source.revision
                || fields[2] != "0"
            {
                return Err("Reedline gitlink and source.toml disagree (or gitlink is unmerged). Update them together; for a Windows worktree run source commands with Windows Git.".into());
            }
        }
        if let Some(upstream) = self.upstream()?
            && git_text(&upstream, &["rev-parse", "HEAD"])? != inputs.source.revision
        {
            return Err("upstream checkout differs from its pin; run git submodule update --init before prepare".into());
        }
        Ok(inputs)
    }

    fn objects(&self, source: &Source) -> Result<PathBuf> {
        if let Some(upstream) = self.upstream()?
            && has_commit(&upstream, &source.revision)?
        {
            return Ok(upstream);
        }
        if self.cache.exists() {
            if !self.cache.join(".git").exists()
                && !(self.cache.join("HEAD").is_file() && self.cache.join("objects").is_dir())
            {
                return Err(format!(
                    "source cache is not a standalone Git repository: {}",
                    self.cache.display()
                )
                .into());
            }
            if has_commit(&self.cache, &source.revision)? {
                return Ok(self.cache.clone());
            }
        }
        if self.cache_read_only {
            return Err(format!(
                "supplied cache {} lacks revision {}; it is read-only. Supply the correct objects or omit --cache to use the private online cache.",
                self.cache.display(), source.revision
            ).into());
        }
        if self.offline {
            return Err(format!(
                "offline: Reedline {} is absent from the submodule and cache {}; initialize it online or supply --cache <bare-repository>. No unpatched fallback.",
                source.revision, self.cache.display()
            ).into());
        }
        fs::create_dir_all(&self.cache)?;
        git(&self.cache, &["init", "--bare", "--quiet"])?;
        git(
            &self.cache,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--depth=1",
                &source.repository,
                &source.revision,
            ],
        )?;
        git(
            &self.cache,
            &[
                "update-ref",
                &format!("refs/nosh/{}", source.revision),
                &source.revision,
            ],
        )?;
        Ok(self.cache.clone())
    }

    fn apply(&self, source: &Source, patch: &[u8]) -> Result<PathBuf> {
        let objects = self.objects(source)?;
        let staging = unique(&self.root.join(".nosh"), "staging");
        fs::create_dir(&staging)?;
        git(&staging, &["init", "--quiet"])?;
        git(&staging, &["config", "core.autocrlf", "false"])?;
        git(
            &staging,
            &[
                "fetch",
                "--quiet",
                "--no-tags",
                "--depth=1",
                text(&objects)?,
                &source.revision,
            ],
        )?;
        git(
            &staging,
            &["checkout", "--quiet", "--detach", &source.revision],
        )?;
        if !patch.is_empty() {
            let file = staging.join(".git/nosh-apply.patch");
            fs::write(&file, patch)?;
            if let Err(error) =
                apply_indexed_patch(&staging, &file, |args| git_output(&staging, args))
            {
                return Err(format!(
                    "{error}\nCandidate retained at {}. Live source and patch were not replaced.",
                    staging.display()
                )
                .into());
            }
        }
        Ok(staging)
    }

    fn replay(&self, source: &Source, edited: &Path) -> Result<Replay> {
        let tree = snapshot(edited)?;
        let patch = git(
            edited,
            &["diff", "--binary", "--full-index", &source.revision, &tree],
        )?;
        let dir = self.apply(source, &patch)?;
        if snapshot(&dir)? != tree {
            return Err("export replay differs from edited source; live source and original patch were not replaced".into());
        }
        Ok(Replay { dir, tree, patch })
    }

    fn state(&self) -> Result<State> {
        let path = self.root.join(GENERATED).join(".git/nosh/state.json");
        let bytes = fs::read(&path).map_err(|e| format!(
            "missing/invalid prepared state at {}: {e}; run prepare, do not delete unexported edits",
            path.display()
        ))?;
        Ok(serde_json::from_slice(&bytes)?)
    }

    fn clean(&self, state: &State) -> Result<()> {
        if snapshot(&self.root.join(GENERATED))? != state.prepared_tree {
            return Err("unexported Reedline edits (including new/deleted files); run export first. Nothing was overwritten.".into());
        }
        Ok(())
    }

    fn checked(&self) -> Result<(Inputs, State)> {
        let inputs = self.inputs()?;
        let state = self.state()?;
        if !state.matches(&inputs) {
            return Err("prepared source is stale; run prepare (local edits are protected)".into());
        }
        self.clean(&state)?;
        Ok((inputs, state))
    }

    fn record(&self, dir: &Path, inputs: &Inputs, tree: String) -> Result<()> {
        let source = &inputs.source;
        let state = State {
            schema_version: 1,
            upstream_repository: source.repository.clone(),
            upstream_revision: source.revision.clone(),
            upstream_tree: git_text(
                dir,
                &["rev-parse", &format!("{}^{{tree}}", source.revision)],
            )?,
            patch_sha256: sha256(&inputs.patch),
            source_pin_sha256: sha256(&inputs.pin),
            prepared_archive_sha256: sha256(&git(
                dir,
                &[
                    "archive",
                    "--format=tar",
                    "--mtime=1970-01-01T00:00:00Z",
                    &tree,
                ],
            )?),
            prepared_tree: tree,
        };
        let metadata = dir.join(".git/nosh");
        fs::create_dir_all(&metadata)?;
        write_atomic(&metadata.join("source.toml"), &inputs.pin)?;
        write_atomic(&metadata.join("nosh.patch"), &inputs.patch)?;
        write_atomic(
            &metadata.join("state.json"),
            &serde_json::to_vec_pretty(&state)?,
        )?;
        Ok(())
    }

    fn install(&self, staging: &Path) -> Result<()> {
        let live = self.root.join(GENERATED);
        let previous = self.root.join(".nosh/previous-reedline");
        if previous.exists() {
            return Err("previous interrupted preparation retained .nosh/previous-reedline; inspect and preserve it before retrying".into());
        }
        let had_live = live.exists();
        if had_live {
            rename_directory(&live, &previous)?;
        }
        if let Err(error) = rename_directory(staging, &live) {
            if had_live {
                rename_directory(&previous, &live)
                    .map_err(|restore| format!("{error}; rollback also failed: {restore}"))?;
            }
            return Err(error);
        }
        if had_live {
            fs::remove_dir_all(&previous).map_err(|error| {
                format!(
                    "prepared source installed, but could not clean {}: {error}",
                    previous.display()
                )
            })?;
        }
        Ok(())
    }

    pub(crate) fn prepare(&self) -> Result<()> {
        let inputs = self.inputs()?;
        if self.root.join(GENERATED).exists() {
            let state = self.state()?;
            self.clean(&state)?;
            if state.matches(&inputs) {
                println!("Reedline is already prepared; no files changed.");
                return Ok(());
            }
        }
        let staging = self.apply(&inputs.source, &inputs.patch)?;
        inputs.ensure_current(&self.root, "preparation")?;
        self.record(&staging, &inputs, snapshot(&staging)?)?;
        if self.root.join(GENERATED).exists() {
            self.clean(&self.state()?)?;
        }
        self.install(&staging)?;
        println!(
            "Prepared patched Reedline at {}",
            self.root.join(GENERATED).display()
        );
        Ok(())
    }

    pub(crate) fn check(&self) -> Result<State> {
        Ok(self.checked()?.1)
    }

    pub(crate) fn export(&self) -> Result<()> {
        let mut inputs = self.inputs()?;
        if !self.state()?.matches(&inputs) {
            return Err("pin/patch changed since preparation; preserve local edits and reconcile before export".into());
        }
        let live = self.root.join(GENERATED);
        let replay = self.replay(&inputs.source, &live)?;
        inputs.ensure_current(&self.root, "export")?;
        write_atomic(&self.root.join(PATCH), &replay.patch)?;
        inputs.patch = replay.patch;
        self.record(&live, &inputs, replay.tree)?;
        fs::remove_dir_all(replay.dir)?;
        println!(
            "Exported and replay-verified patches/reedline/nosh.patch; commit it with nosh changes."
        );
        Ok(())
    }

    pub(crate) fn upgrade(&self, revision: &str, resolved: Option<&Path>) -> Result<()> {
        validate_revision(revision)?;
        let (inputs, _) = self.checked()?;
        if !self.root.join(".git").exists() {
            return Err("upgrade requires a Git checkout, not a source archive".into());
        }
        let mut next = inputs.clone();
        next.source.revision = revision.to_owned();
        let candidate = if let Some(dir) = resolved {
            let dir = dunce::canonicalize(dir)?;
            if !dir.starts_with(self.root.join(".nosh"))
                || dir == self.root.join(GENERATED)
                || git_text(&dir, &["rev-parse", "HEAD"])? != revision
            {
                return Err(
                    "--resolved must be a retained .nosh candidate at the requested revision"
                        .into(),
                );
            }
            if !git_text(&dir, &["ls-files", "--unmerged"])?.is_empty() {
                return Err("candidate still has unresolved Git conflicts".into());
            }
            dir
        } else {
            self.apply(&next.source, &inputs.patch)?
        };
        let replay = self.replay(&next.source, &candidate)?;
        if self.checked()?.0 != inputs {
            return Err("pin/patch changed during upgrade; live source was not replaced".into());
        }
        if let Some(upstream) = self.upstream()? {
            git(
                &upstream,
                &[
                    "fetch",
                    "--quiet",
                    "--no-tags",
                    text(&replay.dir)?,
                    revision,
                ],
            )?;
            git(
                &upstream,
                &[
                    "checkout",
                    "--quiet",
                    "--no-overwrite-ignore",
                    "--detach",
                    revision,
                ],
            )?;
        }
        next.pin = toml::to_string(&next.source)?.into_bytes();
        next.patch = replay.patch;
        write_atomic(&self.root.join(PIN), &next.pin)?;
        write_atomic(&self.root.join(PATCH), &next.patch)?;
        git(
            &self.root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{revision},{UPSTREAM}"),
            ],
        )?;
        self.record(&replay.dir, &next, replay.tree)?;
        self.install(&replay.dir)?;
        println!(
            "Upgraded source and staged gitlink. Candidate retained at {}. Review the pin/patch/gitlink; align the root exact Reedline version and Cargo.lock, then run regressions. Any initialized upstream checkout now matches the new clean pin.",
            candidate.display()
        );
        Ok(())
    }
}
