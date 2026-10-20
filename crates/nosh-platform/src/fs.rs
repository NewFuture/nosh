//! Metadata-only file identity for cache invalidation.

#[cfg(any(not(windows), test))]
use std::fs;
use std::path::Path;
#[cfg(not(windows))]
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{FileIdentity, file_identity, owned_by_current_user, replace_file};

/// What identifies one version of a file without reading it: size and
/// nanosecond modification time, file identity and status-change time on
/// Unix/Windows. Creation time alone cannot identify an in-place rewrite.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileStamp {
    pub size: u64,
    pub mtime_ns: u64,
    /// Unix ctime or Windows ChangeTime; elsewhere creation time (0 when unavailable).
    pub ctime_ns: u64,
    /// Unix device/inode or Windows volume/file index (0 elsewhere).
    pub dev: u64,
    pub ino: u64,
}

impl FileStamp {
    pub fn of(path: &Path) -> Option<Self> {
        #[cfg(windows)]
        {
            windows::file_stamp(path).ok()
        }
        #[cfg(not(windows))]
        {
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps_preserve_the_manifest_schema() {
        let stamp = FileStamp {
            size: 42,
            mtime_ns: 123,
            ctime_ns: 456,
            dev: 7,
            ino: 8,
        };
        let value = serde_json::json!({
            "size": 42, "mtime_ns": 123, "ctime_ns": 456, "dev": 7, "ino": 8
        });
        assert_eq!(serde_json::to_value(&stamp).unwrap(), value);
        assert_eq!(serde_json::from_value::<FileStamp>(value).unwrap(), stamp);
    }

    #[test]
    fn stamps_follow_file_changes_and_missing_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("observed");
        assert_eq!(FileStamp::of(&path), None);
        fs::write(&path, b"before").unwrap();
        let before = FileStamp::of(&path).unwrap();
        assert_eq!(FileStamp::of(&path), Some(before.clone()));
        fs::write(&path, b"after changes").unwrap();
        let after = FileStamp::of(&path).unwrap();
        assert_ne!(before, after);
        assert_eq!(after.size, 13);
        fs::remove_file(&path).unwrap();
        assert_eq!(FileStamp::of(&path), None);
    }

    #[test]
    #[cfg(windows)]
    fn windows_stamps_detect_same_size_rewrites_with_restored_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model-part");
        fs::write(&path, b"before").unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        let before = FileStamp::of(&path).unwrap();
        assert_ne!(before.ino, 0);
        std::thread::sleep(std::time::Duration::from_millis(50));
        fs::write(&path, b"after!").unwrap();
        fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(mtime)
            .unwrap();
        let after = FileStamp::of(&path).unwrap();
        assert_eq!(
            (before.size, before.mtime_ns, before.dev, before.ino),
            (after.size, after.mtime_ns, after.dev, after.ino)
        );
        assert_ne!(before.ctime_ns, after.ctime_ns);
    }
}
