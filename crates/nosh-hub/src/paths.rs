//! Well-known nosh directories.

use std::path::PathBuf;

fn nosh_home() -> Option<PathBuf> {
    std::env::var_os("NOSH_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

/// `<data_dir>/nosh` (Linux: `~/.local/share/nosh`), or `$NOSH_HOME`.
pub fn data_dir() -> PathBuf {
    nosh_home().unwrap_or_else(|| {
        dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nosh")
    })
}

/// `<config_dir>/nosh` (Linux: `~/.config/nosh`), or `$NOSH_HOME`.
pub fn config_dir() -> PathBuf {
    nosh_home().unwrap_or_else(|| {
        dirs::config_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("nosh")
    })
}

pub fn config_file() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn models_dir() -> PathBuf {
    data_dir().join("models")
}

pub fn state_dir() -> PathBuf {
    data_dir().join("state")
}

/// Portable mode: `<exe dir>/models`.
pub fn portable_models_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.join("models")))
}

/// Shared system model store for multi-user hosts.
pub fn system_models_dir() -> Option<PathBuf> {
    if cfg!(unix) {
        Some(PathBuf::from("/usr/share/nosh/models"))
    } else {
        None
    }
}

/// Creates `dir` (and parents) with mode 0700 on Unix.
pub fn ensure_private_dir(dir: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}
