//! Metadata-only file identity for cache invalidation.

use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
}
