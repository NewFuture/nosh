use std::ffi::OsString;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use crate::terminal::BackendSettings;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Connection {
    pub name: String,
    pub host: String,
    pub user: String,
    pub port: Option<u16>,
    pub identity: String,
    pub jump: String,
}

impl Connection {
    pub fn validate(&self) -> Result<(), String> {
        if self.name.trim().is_empty() || self.name.len() > 80 {
            return Err("Connection name must contain 1-80 bytes.".into());
        }
        validate_atom(&self.host, "Host or SSH config alias", b".-_:[]%")?;
        if !self.user.is_empty() {
            validate_atom(&self.user, "Username", b".-_")?;
        }
        if self.port == Some(0) {
            return Err("Port must be between 1 and 65535.".into());
        }
        if !self.jump.is_empty() {
            validate_atom(&self.jump, "Jump host", b".-_:[]%@,")?;
        }
        if self.identity.chars().any(char::is_control) || self.identity.len() > 4096 {
            return Err("Invalid identity file path.".into());
        }
        Ok(())
    }

    pub fn args(&self) -> Result<Vec<String>, String> {
        self.validate()?;
        // Keep host authentication interactive, even if a user's config disables it.
        // Agent forwarding and LocalCommand aren't desktop capabilities.
        let mut args = vec![
            "-tt".into(),
            "-o".into(),
            "StrictHostKeyChecking=ask".into(),
            "-o".into(),
            "ForwardAgent=no".into(),
            "-o".into(),
            "PermitLocalCommand=no".into(),
            "-o".into(),
            "ClearAllForwardings=yes".into(),
            "-o".into(),
            "RemoteCommand=none".into(),
            "-o".into(),
            "ConnectTimeout=15".into(),
            "-o".into(),
            "ServerAliveInterval=30".into(),
            "-o".into(),
            "ServerAliveCountMax=3".into(),
        ];
        if !self.user.is_empty() {
            args.extend(["-l".into(), self.user.clone()]);
        }
        if let Some(port) = self.port {
            args.extend(["-p".into(), port.to_string()]);
        }
        if !self.identity.is_empty() {
            args.extend(["-i".into(), self.identity.clone()]);
        }
        if !self.jump.is_empty() {
            args.extend(["-J".into(), self.jump.clone()]);
        }
        args.extend(["--".into(), self.host.clone()]);
        Ok(args)
    }

    pub fn target(&self) -> String {
        let user = if self.user.is_empty() {
            String::new()
        } else {
            format!("{}@", self.user)
        };
        let port = self.port.map(|p| format!(":{p}")).unwrap_or_default();
        format!("{user}{}{port}", self.host)
    }
}

fn validate_atom(value: &str, name: &str, punctuation: &[u8]) -> Result<(), String> {
    if value.is_empty()
        || value.len() > 255
        || value.starts_with('-')
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || punctuation.contains(&b))
    {
        return Err(format!("{name} contains invalid characters."));
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub version: u32,
    pub dark: bool,
    pub font_size: f32,
    pub connections: Vec<Connection>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: 1,
            dark: false,
            font_size: 14.0,
            connections: Vec::new(),
        }
    }
}

impl Settings {
    pub fn load(path: &Path) -> Result<Self, String> {
        let file = match fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(format!("Read {}: {error}", path.display())),
        };
        let mut bytes = Vec::new();
        file.take(1_048_577)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > 1_048_576 {
            return Err("Desktop settings exceed 1 MiB.".into());
        }
        let settings: Self = serde_json::from_slice(&bytes)
            .map_err(|error| format!("Invalid settings {}: {error}", path.display()))?;
        settings.validate()?;
        Ok(settings)
    }

    fn validate(&self) -> Result<(), String> {
        if self.version != 1 || !(10.0..=24.0).contains(&self.font_size) {
            return Err("Unsupported desktop settings version or font size.".into());
        }
        if self.connections.len() > 100 {
            return Err("At most 100 saved connections are supported.".into());
        }
        for connection in &self.connections {
            connection.validate()?;
        }
        Ok(())
    }

    pub fn save(&self, path: &Path) -> Result<(), String> {
        self.validate()?;
        let parent = path.parent().ok_or("Settings path has no parent.")?;
        fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let mut file =
            tempfile::NamedTempFile::new_in(parent).map_err(|error| error.to_string())?;
        serde_json::to_writer_pretty(&mut file, self).map_err(|error| error.to_string())?;
        file.flush().map_err(|error| error.to_string())?;
        file.as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        file.persist(path).map_err(|error| error.to_string())?;
        Ok(())
    }
}

pub fn find_nosh(explicit: Option<PathBuf>) -> Result<PathBuf, String> {
    if let Some(path) = explicit.or_else(|| std::env::var_os("NOSH_BIN").map(PathBuf::from)) {
        return fs::canonicalize(&path)
            .map_err(|error| format!("Invalid nosh executable {}: {error}", path.display()));
    }
    let exe = if cfg!(windows) { "nosh.exe" } else { "nosh" };
    if let Ok(current) = std::env::current_exe()
        && let Some(parent) = current.parent()
        && parent.join(exe).is_file()
    {
        return Ok(parent.join(exe));
    }
    if let Some(path) = find_on_path(exe) {
        return Ok(path);
    }
    Err("nosh was not found. Build nosh-cli beside this executable or use --nosh PATH.".into())
}

fn find_on_path(executable: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default())
        .filter(|folder| folder.is_absolute())
        .map(|folder| folder.join(executable))
        .find(|path| path.is_file())
}

pub fn local_backend(nosh: &Path, folder: &Path) -> BackendSettings {
    BackendSettings {
        shell: nosh.to_string_lossy().into_owned(),
        args: vec!["--offline".into(), "--no-download".into(), "-i".into()],
        working_directory: Some(folder.to_owned()),
        ..Default::default()
    }
}

pub fn ssh_backend(connection: &Connection) -> Result<BackendSettings, String> {
    let args = connection.args()?;
    let ssh = find_on_path(if cfg!(windows) { "ssh.exe" } else { "ssh" })
        .ok_or("OpenSSH was not found. Install it and add its directory to PATH.")?;
    Ok(BackendSettings {
        shell: ssh.to_string_lossy().into_owned(),
        args,
        working_directory: None,
        ..Default::default()
    })
}

pub fn suggestion_args(description: &str) -> Vec<OsString> {
    ["--offline", "--no-download", "-s", "--", description]
        .into_iter()
        .map(OsString::from)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn connection() -> Connection {
        Connection {
            name: "dev".into(),
            host: "dev-alias".into(),
            ..Default::default()
        }
    }

    #[test]
    fn ssh_respects_config_defaults_and_keeps_all_input_as_arguments() {
        let mut connection = connection();
        let args = connection.args().unwrap();
        assert_eq!(&args[args.len() - 2..], ["--", "dev-alias"]);
        assert!(!args.iter().any(|arg| arg == "-p" || arg == "-l"));
        assert!(args.contains(&"StrictHostKeyChecking=ask".into()));
        connection.host = "::1".into();
        connection.user = "deploy".into();
        connection.port = Some(2222);
        connection.identity = r"C:\keys\my key".into();
        connection.jump = "user@jump.example:2222".into();
        let args = connection.args().unwrap();
        assert!(args.windows(2).any(|a| a == ["-i", r"C:\keys\my key"]));
        assert!(
            args.windows(2)
                .any(|a| a == ["-J", "user@jump.example:2222"])
        );
    }

    #[test]
    fn ssh_rejects_options_shell_metacharacters_and_control_characters() {
        for host in [
            "-oProxyCommand=calc",
            "host;touch file",
            "host\nother",
            "u@host",
            "",
        ] {
            let mut connection = connection();
            connection.host = host.into();
            assert!(connection.args().is_err(), "{host:?}");
        }
        let mut connection = connection();
        connection.jump = "-oProxyCommand=calc".into();
        assert!(connection.args().is_err());
        connection.jump.clear();
        connection.port = Some(0);
        assert!(connection.args().is_err());
    }

    #[test]
    fn settings_persist_connections_and_reject_corruption_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("desktop.json");
        let settings = Settings {
            connections: vec![connection()],
            dark: true,
            ..Default::default()
        };
        settings.save(&path).unwrap();
        assert_eq!(
            Settings::load(&path).unwrap().connections,
            settings.connections
        );
        let corrupt = b"{broken";
        fs::write(&path, corrupt).unwrap();
        assert!(Settings::load(&path).is_err());
        assert_eq!(fs::read(&path).unwrap(), corrupt);
        fs::write(&path, r#"{"version":2}"#).unwrap();
        assert!(Settings::load(&path).is_err());
    }

    #[test]
    fn descriptions_starting_with_flags_cannot_change_nosh_mode() {
        let args = suggestion_args("--yolo; whoami");
        assert_eq!(args[3], "--");
        assert_eq!(args[4], "--yolo; whoami");
    }
}
