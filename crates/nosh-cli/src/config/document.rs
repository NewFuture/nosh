use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

const MAX_CONFIG: u64 = 1024 * 1024;

pub fn read_text(path: &Path) -> Result<String, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    read(file)
}

fn read(file: fs::File) -> Result<String, String> {
    if !file.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("configuration must be a regular file".into());
    }
    let mut text = String::new();
    file.take(MAX_CONFIG + 1)
        .read_to_string(&mut text)
        .map_err(|e| e.to_string())?;
    if text.len() as u64 > MAX_CONFIG {
        return Err("configuration exceeds 1 MiB".into());
    }
    Ok(text)
}

#[derive(PartialEq, Eq)]
struct Source {
    text: String,
    identity: Option<(u64, u64, u32, u32)>,
}

fn source(path: &Path) -> Result<Source, String> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if fs::symlink_metadata(path).is_ok() {
                return Err("configuration is a symlink; edit its target explicitly".into());
            }
            return Ok(Source {
                text: String::new(),
                identity: None,
            });
        }
        Err(e) => return Err(format!("cannot open configuration {}: {e}", path.display())),
    };
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    // Replacing a foreign/shared file must not change ownership or hard-link semantics.
    if metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("configuration must be an owned file with one hard link".into());
    }
    Ok(Source {
        text: read(file)?,
        identity: Some((
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.gid(),
        )),
    })
}

pub fn update<T>(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T, String>,
) -> Result<T, String> {
    update_checked(path, change, || {})
}

pub fn update_checked<T>(
    path: &Path,
    change: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<T, String>,
    before_commit: impl FnOnce(),
) -> Result<T, String> {
    let parent = path.parent().ok_or("configuration path has no parent")?;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .map_err(|e| e.to_string())?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(parent.join(".config.lock"))
        .map_err(|e| format!("cannot open configuration lock: {e}"))?;
    if !lock.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("configuration lock must be a regular file".into());
    }
    lock.try_lock()
        .map_err(|e| format!("configuration is busy or cannot be locked: {e}"))?;
    let original = source(path)?;
    if original
        .identity
        .is_some_and(|(_, _, mode, _)| mode & 0o222 == 0)
    {
        return Err("configuration is read-only; changes were not saved".into());
    }
    let mut document = original
        .text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|e| format!("invalid configuration: {e}"))?;
    let result = change(&mut document)?;
    let updated = document.to_string();
    if updated == original.text {
        return Ok(result);
    }
    if updated.len() as u64 > MAX_CONFIG {
        return Err("configuration exceeds 1 MiB".into());
    }
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    if let Some((_, _, mode, group)) = original.identity {
        use std::os::fd::AsRawFd;
        if temporary
            .as_file()
            .metadata()
            .map_err(|e| e.to_string())?
            .gid()
            != group
            && unsafe { libc::fchown(temporary.as_file().as_raw_fd(), !0, group) } != 0
        {
            return Err(format!(
                "cannot preserve configuration group: {}",
                std::io::Error::last_os_error()
            ));
        }
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode & 0o777))
            .map_err(|e| e.to_string())?;
    }
    temporary
        .write_all(updated.as_bytes())
        .map_err(|e| e.to_string())?;
    temporary.as_file().sync_all().map_err(|e| e.to_string())?;
    before_commit();
    if source(path)? != original {
        return Err("configuration changed while saving; unchanged, retry the command".into());
    }
    temporary
        .persist(path)
        .map_err(|e| format!("configuration was not saved: {}", e.error))?;
    Ok(result)
}
