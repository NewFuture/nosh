//! On-disk model stores and `manifest.json`.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::HubError;
use crate::hash;
use crate::registry::{FileEntry, FileRole, ModelEntry};

/// Version 2 records a [`FileStamp`] per file; older manifests only kept the
/// modification time in seconds and are not trusted (files are re-hashed once).
pub const MANIFEST_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Manifest {
    #[serde(default)]
    pub version: u32,
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
    /// Identity of the file when its hash was verified.
    #[serde(default)]
    pub stamp: Option<FileStamp>,
}

/// What identifies one version of a file without reading it: size and
/// nanosecond modification time, plus on Unix the device/inode and the
/// status-change time (which tools cannot set, unlike mtime). Replacing or
/// rewriting the file changes at least one of them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub size: u64,
    pub mtime_ns: u64,
    /// Unix ctime; elsewhere the creation time (0 when unavailable).
    pub ctime_ns: u64,
    /// Unix device and inode numbers (0 elsewhere).
    pub dev: u64,
    pub ino: u64,
}

impl FileStamp {
    pub fn of(path: &Path) -> Option<Self> {
        let m = fs::metadata(path).ok()?;
        let ns = |t: SystemTime| {
            t.duration_since(UNIX_EPOCH)
                .map_or(0, |d| u64::try_from(d.as_nanos()).unwrap_or(u64::MAX))
        };
        let mut s = FileStamp {
            size: m.len(),
            mtime_ns: ns(m.modified().ok()?),
            ..FileStamp::default()
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            s.ctime_ns = u64::try_from(m.ctime())
                .map_or(0, |secs| secs.saturating_mul(1_000_000_000))
                .saturating_add(u64::try_from(m.ctime_nsec()).unwrap_or(0));
            s.dev = m.dev();
            s.ino = m.ino();
        }
        #[cfg(not(unix))]
        {
            s.ctime_ns = m.created().map_or(0, ns);
        }
        Some(s)
    }
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

/// Records that `file` in `dir` has been verified, as it is now (use after
/// the verified bytes were moved into place).
pub fn record_verified(
    dir: &Path,
    id: &str,
    file: &FileEntry,
    source: &str,
    revision: &str,
) -> Result<(), HubError> {
    let path = dir.join(&file.name);
    let stamp = FileStamp::of(&path).ok_or_else(|| {
        HubError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} disappeared before it was recorded", path.display()),
        ))
    })?;
    record_verified_as(dir, id, file, source, revision, &stamp).map(|_| ())
}

/// Records that `file` was verified while it had `stamp` (taken before
/// hashing). Records nothing and returns `false` if the file changed since.
pub fn record_verified_as(
    dir: &Path,
    id: &str,
    file: &FileEntry,
    source: &str,
    revision: &str,
    stamp: &FileStamp,
) -> Result<bool, HubError> {
    if FileStamp::of(&dir.join(&file.name)).as_ref() != Some(stamp) {
        return Ok(false);
    }
    let mut m = read_manifest(dir)
        .filter(|m| m.version == MANIFEST_VERSION)
        .unwrap_or_default();
    m.version = MANIFEST_VERSION;
    m.id = id.to_string();
    m.files.retain(|f| f.name != file.name);
    m.files.push(ManifestFile {
        name: file.name.clone(),
        size: file.size,
        sha256: file.sha256.clone(),
        source: source.to_string(),
        revision: revision.to_string(),
        stamp: Some(stamp.clone()),
    });
    write_manifest(dir, &m)?;
    Ok(true)
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
            let verified = manifest
                .filter(|man| man.version == MANIFEST_VERSION)
                .is_some_and(|man| {
                    let now = FileStamp::of(&path);
                    man.files.iter().any(|f| {
                        f.name == file.name
                            && f.sha256.eq_ignore_ascii_case(&file.sha256)
                            && f.size == file.size
                            && f.stamp.is_some()
                            && f.stamp == now
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
                let path = dir.join(&f.name);
                // Stamp first: a change while hashing then leaves the file unrecorded.
                let before = FileStamp::of(&path);
                let sha = hash::sha256_file(&path, |_| {})?;
                if !sha.eq_ignore_ascii_case(&f.sha256) {
                    return Err(HubError::ChecksumMismatch {
                        file: path.display().to_string(),
                        expected: f.sha256.clone(),
                        actual: sha,
                    });
                }
                // The hash only holds if the file did not change meanwhile; a
                // read-only store merely cannot record it (`Err`).
                let unchanged = before.is_some_and(|stamp| {
                    record_verified_as(dir, &entry.id, f, "local", "", &stamp).unwrap_or(true)
                });
                if !unchanged {
                    return Err(HubError::Changed(path.display().to_string()));
                }
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

    fn verified(dir: &Path, e: &ModelEntry) -> bool {
        file_state(dir, e.weights(), read_manifest(dir).as_ref())
            == FileState::Present { verified: true }
    }

    /// Coarse kernel clocks can give two writes a few ms apart the same ctime.
    fn tick() {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    #[test]
    fn same_size_replacement_with_restored_mtime_is_not_trusted() {
        let (e, dir) = tiny_entry("store-replace");
        check_dir(&dir, &e).unwrap().unwrap();
        assert!(verified(&dir, &e));
        let path = dir.join(&e.weights().name);
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        tick();
        // Rewritten in place (same inode), same size, mtime put back to the
        // recorded nanosecond: only the change time gives it away.
        fs::write(&path, b"weights-bytez").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        assert_eq!(fs::metadata(&path).unwrap().modified().unwrap(), mtime);
        assert!(!verified(&dir, &e));
        assert!(matches!(
            check_dir(&dir, &e),
            Err(HubError::ChecksumMismatch { .. })
        ));
        // Replaced by another file (new inode) with the right bytes: re-hashed, then trusted.
        tick();
        let tmp = dir.join("replacement");
        fs::write(&tmp, b"weights-bytes").unwrap();
        fs::rename(&tmp, &path).unwrap();
        assert!(!verified(&dir, &e));
        check_dir(&dir, &e).unwrap().unwrap();
        assert!(verified(&dir, &e));
    }

    #[test]
    fn version_1_manifests_force_one_reverification() {
        let (e, dir) = tiny_entry("store-v1");
        let secs = fs::metadata(dir.join(&e.weights().name))
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let v1 = serde_json::json!({
            "id": e.id,
            "files": e.files.iter().map(|f| serde_json::json!({
                "name": f.name, "size": f.size, "sha256": f.sha256,
                "source": "hf", "revision": "main", "mtime": secs,
            })).collect::<Vec<_>>(),
        });
        fs::write(dir.join(MANIFEST), v1.to_string()).unwrap();
        let m = read_manifest(&dir).unwrap();
        assert_eq!(m.version, 0);
        assert!(!verified(&dir, &e), "a v1 entry vouches for nothing");
        // A tampered file hiding behind a v1 entry is caught by the re-hash.
        fs::write(dir.join(&e.weights().name), b"weights-bytez").unwrap();
        assert!(check_dir(&dir, &e).is_err());
        fs::write(dir.join(&e.weights().name), b"weights-bytes").unwrap();
        check_dir(&dir, &e).unwrap().unwrap();
        let m = read_manifest(&dir).unwrap();
        assert_eq!(m.version, MANIFEST_VERSION);
        assert!(m.files.iter().all(|f| f.stamp.is_some()));
        assert!(verified(&dir, &e));
    }

    #[test]
    fn nothing_is_recorded_if_the_file_changed_while_hashing() {
        let (e, dir) = tiny_entry("store-race");
        let path = dir.join(&e.weights().name);
        let before = FileStamp::of(&path).unwrap();
        tick();
        fs::write(&path, b"weights-bytez").unwrap();
        let recorded = record_verified_as(&dir, &e.id, e.weights(), "local", "", &before).unwrap();
        assert!(!recorded);
        assert!(read_manifest(&dir).is_none());
    }
}
