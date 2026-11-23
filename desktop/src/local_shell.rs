#[cfg(not(windows))]
use crate::connections;
use crate::terminal::BackendSettings;
use std::path::{Path, PathBuf};

#[cfg(windows)]
mod windows;

#[derive(Debug)]
pub enum LocalShell {
    Bundled(PathBuf),
}

impl LocalShell {
    pub fn resolve() -> Result<Self, String> {
        std::env::current_exe()
            .map(Self::Bundled)
            .map_err(|error| error.to_string())
    }

    pub fn backend(&self, folder: &Path) -> Result<BackendSettings, String> {
        let Self::Bundled(executable) = self;
        #[cfg(windows)]
        {
            windows::backend(executable, folder)
                .map_err(|error| format!("Cannot start bundled nosh: {error}"))
        }
        #[cfg(not(windows))]
        {
            let mut settings = connections::local_backend(executable, folder);
            settings
                .args
                .insert(0, nosh_cli::EMBEDDED_SHELL_FLAG.into());
            Ok(settings)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_sessions_use_the_current_executable_and_embedded_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = std::env::current_exe().unwrap();
        let shell = LocalShell::resolve().unwrap();
        let LocalShell::Bundled(executable) = &shell;
        assert_eq!(executable, &builtin);
        let builtin_command = shell.backend(dir.path()).unwrap();
        #[cfg(not(windows))]
        assert_eq!(builtin_command.args[0], nosh_cli::EMBEDDED_SHELL_FLAG);
        #[cfg(windows)]
        assert!(
            builtin_command
                .env
                .values()
                .any(|value| value.contains(nosh_cli::EMBEDDED_SHELL_FLAG))
        );
        #[cfg(windows)]
        assert_eq!(
            builtin_command.env["NOSH_EMBEDDED_PROGRAM"],
            builtin.to_string_lossy()
        );
        #[cfg(windows)]
        assert!(
            LocalShell::Bundled(dir.path().join("missing.exe"))
                .backend(dir.path())
                .is_err()
        );
    }
}
