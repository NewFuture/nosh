//! On-disk model stores and `manifest.json`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::HubError;
use crate::hash;
use crate::registry::{FileEntry, FileRole, ModelEntry};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    pub id: String,
    pub files: Vec<ManifestFile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestFile {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    #[serde(default)]
    pub source: String,
    #[serde(default)]
    pub revision: String,
    /// Modification time (seconds) when the hash was verified.
    pub mtime: u64,
}

pub const MANIFEST: &str = "manifest.json";

pub fn read_manifest(dir: &Path) -> Option<Manifest> {
    let s = fs::read_to_string(dir.join(MANIFEST)).ok()?;
    serde_json::from_str(&s).ok()
}

pub fn write_manifest(dir: &Path, m: &Manifest) -> Result<(), HubError> {
    let tmp = dir.join(format!("{MANIFEST}.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(m).expect("serializable"))?;
    fs::rename(tmp, dir.join(MANIFEST))?;
    Ok(())
}

fn mtime_secs(path: &Path) -> Option<u64> {
    fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|d| d.as_secs())
}

/// Records that `file` in `dir` has been verified.
pub fn record_verified(
    dir: &Path,
    id: &str,
    file: &FileEntry,
    source: &str,
    revision: &str,
) -> Result<(), HubError> {
    let mut m = read_manifest(dir).unwrap_or_else(|| Manifest {
        id: id.to_string(),
        files: vec![],
    });
    m.id = id.to_string();
    m.files.retain(|f| f.name != file.name);
    m.files.push(ManifestFile {
        name: file.name.clone(),
        size: file.size,
        sha256: file.sha256.clone(),
        source: source.to_string(),
        revision: revision.to_string(),
        mtime: mtime_secs(&dir.join(&file.name)).unwrap_or(0),
    });
    write_manifest(dir, &m)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileState {
    /// Present with the right size; `verified` when the manifest vouches for the hash.
    Present {
        verified: bool,
    },
    Partial(u64),
    WrongSize(u64),
    Missing,
}

impl FileState {
    pub fn is_present(&self) -> bool {
        matches!(self, FileState::Present { .. })
    }
}

pub fn file_state(dir: &Path, file: &FileEntry, manifest: Option<&Manifest>) -> FileState {
    let path = dir.join(&file.name);
    match fs::metadata(&path) {
        Ok(m) if m.len() == file.size => {
            let verified = manifest.is_some_and(|man| {
                man.files.iter().any(|f| {
                    f.name == file.name
                        && f.sha256.eq_ignore_ascii_case(&file.sha256)
                        && f.size == file.size
                        && Some(f.mtime) == mtime_secs(&path)
                })
            });
            FileState::Present { verified }
        }
        Ok(m) => FileState::WrongSize(m.len()),
        Err(_) => match fs::metadata(dir.join(format!("{}.partial", file.name))) {
            Ok(m) => FileState::Partial(m.len()),
            Err(_) => FileState::Missing,
        },
    }
}

/// Where a model's files were found.
#[derive(Debug, Clone)]
pub struct ResolvedModel {
    pub entry: ModelEntry,
    pub dir: PathBuf,
    pub weights: PathBuf,
    pub tokenizer: PathBuf,
}

/// Checks a candidate directory; verifies hashes that the manifest does not
/// already vouch for (and records them when the directory is writable).
pub fn check_dir(dir: &Path, entry: &ModelEntry) -> Result<Option<ResolvedModel>, HubError> {
    if !dir.is_dir() {
        return Ok(None);
    }
    let manifest = read_manifest(dir);
    for role in [FileRole::Tokenizer, FileRole::Weights] {
        let f = entry.file(role);
        match file_state(dir, f, manifest.as_ref()) {
            FileState::Present { verified: true } => {}
            FileState::Present { verified: false } => {
                let sha = hash::sha256_file(&dir.join(&f.name), |_| {})?;
                if !sha.eq_ignore_ascii_case(&f.sha256) {
                    return Err(HubError::ChecksumMismatch {
                        file: dir.join(&f.name).display().to_string(),
                        expected: f.sha256.clone(),
                        actual: sha,
                    });
                }
                let _ = record_verified(dir, &entry.id, f, "local", "");
            }
            _ => return Ok(None),
        }
    }
    Ok(Some(ResolvedModel {
        entry: entry.clone(),
        dir: dir.to_path_buf(),
        weights: dir.join(&entry.weights().name),
        tokenizer: dir.join(&entry.tokenizer().name),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Registry, SourceRef};

    fn tiny_entry(dir_tag: &str) -> (ModelEntry, PathBuf) {
        let mut e = Registry::builtin().default_model().clone();
        let w = b"weights-bytes".to_vec();
        let t = b"{\"tok\":1}".to_vec();
        let sha = |d: &[u8]| {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(d);
            hash::finalize_hex(h)
        };
        for f in &mut e.files {
            let data = if f.role == FileRole::Weights { &w } else { &t };
            f.size = data.len() as u64;
            f.sha256 = sha(data);
            f.sources = vec![SourceRef {
                hub: "hf".into(),
                repo: "a/b".into(),
                revision: "main".into(),
            }];
        }
        let dir = crate::testserver::tempdir(dir_tag);
        fs::write(dir.join(&e.weights().name), &w).unwrap();
        fs::write(dir.join(&e.tokenizer().name), &t).unwrap();
        (e, dir)
    }

    #[test]
    fn check_dir_verifies_and_records() {
        let (e, dir) = tiny_entry("store-ok");
        let r = check_dir(&dir, &e).unwrap().unwrap();
        assert_eq!(r.weights, dir.join(&e.weights().name));
        let m = read_manifest(&dir).unwrap();
        assert_eq!(m.files.len(), 2);
        assert_eq!(
            file_state(&dir, e.weights(), Some(&m)),
            FileState::Present { verified: true }
        );
    }

    #[test]
    fn check_dir_rejects_tampered_file() {
        let (e, dir) = tiny_entry("store-bad");
        fs::write(dir.join(&e.weights().name), b"weights-bytez").unwrap();
        assert!(matches!(
            check_dir(&dir, &e),
            Err(HubError::ChecksumMismatch { .. })
        ));
    }

    #[test]
    fn missing_and_partial_states() {
        let (e, dir) = tiny_entry("store-partial");
        fs::remove_file(dir.join(&e.weights().name)).unwrap();
        assert_eq!(file_state(&dir, e.weights(), None), FileState::Missing);
        fs::write(dir.join(format!("{}.partial", e.weights().name)), b"wei").unwrap();
        assert_eq!(file_state(&dir, e.weights(), None), FileState::Partial(3));
        assert!(check_dir(&dir, &e).unwrap().is_none());
    }
}
