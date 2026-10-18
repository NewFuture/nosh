use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
#[cfg(windows)]
use std::os::windows::fs::OpenOptionsExt;
use std::path::Path;

const MAX_CONFIG: u64 = 1024 * 1024;

pub fn read_text(path: &Path) -> Result<String, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(String::new()),
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    read(file).map_err(|error| format!("{}: {error}", path.display()))
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
    identity: Option<Identity>,
}

#[cfg(unix)]
type Identity = (u64, u64, u32, u32);
#[cfg(windows)]
type Identity = nosh_platform::fs::FileIdentity;

fn source(path: &Path) -> Result<Source, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let file = match options.open(path) {
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
    #[cfg(unix)]
    let metadata = file.metadata().map_err(|e| e.to_string())?;
    // Replacing a foreign/shared file must not change ownership or hard-link semantics.
    #[cfg(unix)]
    if metadata.nlink() != 1 || metadata.uid() != unsafe { libc::geteuid() } {
        return Err("configuration must be an owned file with one hard link".into());
    }
    #[cfg(unix)]
    let identity = (
        metadata.dev(),
        metadata.ino(),
        metadata.mode(),
        metadata.gid(),
    );
    #[cfg(windows)]
    let identity = {
        let identity = nosh_platform::fs::file_identity(&file).map_err(|e| e.to_string())?;
        if identity.links != 1
            || !nosh_platform::fs::owned_by_current_user(&file).map_err(|e| e.to_string())?
        {
            return Err("configuration must be an owned file with one hard link".into());
        }
        identity
    };
    Ok(Source {
        text: read(file)?,
        identity: Some(identity),
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
    let mut directory = fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    directory.mode(0o700);
    directory.create(parent).map_err(|e| e.to_string())?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    options
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    #[cfg(windows)]
    options.custom_flags(windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OPEN_REPARSE_POINT);
    let lock = options
        .open(parent.join(".config.lock"))
        .map_err(|e| format!("cannot open configuration lock: {e}"))?;
    if !lock.metadata().map_err(|e| e.to_string())?.is_file() {
        return Err("configuration lock must be a regular file".into());
    }
    #[cfg(windows)]
    if nosh_platform::fs::file_identity(&lock)
        .map_err(|e| e.to_string())?
        .links
        != 1
        || !nosh_platform::fs::owned_by_current_user(&lock).map_err(|e| e.to_string())?
    {
        return Err("configuration lock must be an owned file with one hard link".into());
    }
    lock.try_lock()
        .map_err(|e| format!("configuration is busy or cannot be locked: {e}"))?;
    let original = source(path)?;
    #[cfg(unix)]
    let read_only = original
        .identity
        .is_some_and(|(_, _, mode, _)| mode & 0o222 == 0);
    #[cfg(windows)]
    let read_only = original.identity.is_some_and(|identity| {
        identity.attributes & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_READONLY != 0
    });
    if read_only {
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
    #[cfg(unix)]
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
    #[cfg(windows)]
    if original.identity.is_some() {
        let temporary = temporary.into_temp_path();
        nosh_platform::fs::replace_file(&temporary, path)
            .map_err(|error| format!("configuration was not saved: {error}"))?;
        return Ok(result);
    }
    temporary
        .persist(path)
        .map_err(|e| format!("configuration was not saved: {}", e.error))?;
    Ok(result)
}
