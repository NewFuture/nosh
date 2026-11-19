use crate::connections;
use crate::terminal::BackendSettings;
use std::path::{Path, PathBuf};

#[cfg(windows)]
mod windows;

#[derive(Debug)]
pub enum LocalShell {
    Bundled(PathBuf),
    External(PathBuf),
}

impl LocalShell {
    pub fn resolve(explicit: Option<PathBuf>) -> Result<Self, String> {
        let current = std::env::current_exe().map_err(|error| error.to_string())?;
        Self::resolve_at(
            current,
            explicit.or_else(|| std::env::var_os("NOSH_BIN").map(Into::into)),
        )
    }

    fn resolve_at(current: PathBuf, external: Option<PathBuf>) -> Result<Self, String> {
        let Some(external) = external else {
            return Ok(Self::Bundled(current));
        };
        let external = connections::find_nosh(Some(external))?;
        if !external.is_file() {
            return Err(format!(
                "Invalid nosh executable {}: not a file",
                external.display()
            ));
        }
        if std::fs::canonicalize(&current).ok().as_ref() == Some(&external) {
            Ok(Self::Bundled(external))
        } else {
            Ok(Self::External(external))
        }
    }

    pub fn backend(&self, folder: &Path) -> Result<BackendSettings, String> {
        let (Self::Bundled(executable) | Self::External(executable)) = self;
        #[cfg(windows)]
        if matches!(self, Self::Bundled(_)) {
            return windows::backend(executable, folder)
                .map_err(|error| format!("Cannot start bundled nosh: {error}"));
        }
        let mut settings = connections::local_backend(executable, folder);
        if matches!(self, Self::Bundled(_)) {
            settings
                .args
                .insert(0, nosh_cli::EMBEDDED_SHELL_FLAG.into());
        }
        Ok(settings)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_is_default_but_explicit_overrides_never_silently_fall_back() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = dir.path().join("desktop.exe");
        let external = dir.path().join("nosh.exe");
        std::fs::write(&builtin, b"desktop").unwrap();
        std::fs::write(&external, b"external").unwrap();
        let builtin_command = LocalShell::resolve_at(builtin.clone(), None)
            .unwrap()
            .backend(dir.path())
            .unwrap();
        #[cfg(not(windows))]
        assert_eq!(builtin_command.args[0], nosh_cli::EMBEDDED_SHELL_FLAG);
        #[cfg(windows)]
        assert!(
            builtin_command
                .env
                .values()
                .any(|value| value.contains(nosh_cli::EMBEDDED_SHELL_FLAG))
        );
        let external_command = LocalShell::resolve_at(builtin.clone(), Some(external))
            .unwrap()
            .backend(dir.path())
            .unwrap();
        assert_eq!(external_command.args[0], "--offline");
        assert!(LocalShell::resolve_at(builtin.clone(), Some(dir.path().join("absent"))).is_err());
        assert!(LocalShell::resolve_at(builtin.clone(), Some(dir.path().to_owned())).is_err());
        assert!(matches!(
            LocalShell::resolve_at(builtin.clone(), Some(builtin)),
            Ok(LocalShell::Bundled(_))
        ));
        #[cfg(windows)]
        assert!(
            LocalShell::Bundled(dir.path().join("missing.exe"))
                .backend(dir.path())
                .is_err()
        );
    }
}
