#[cfg(not(windows))]
use crate::connections;
use crate::terminal::BackendSettings;
use std::path::Path;

#[cfg(windows)]
mod windows;

pub fn backend(executable: &Path, folder: &Path) -> Result<BackendSettings, String> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn local_sessions_use_the_current_executable_and_embedded_dispatch() {
        let dir = tempfile::tempdir().unwrap();
        let builtin = std::env::current_exe().unwrap();
        let builtin_command = backend(&builtin, dir.path()).unwrap();
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
        assert!(backend(&dir.path().join("missing.exe"), dir.path()).is_err());
    }
}
