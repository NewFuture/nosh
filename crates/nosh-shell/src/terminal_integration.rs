//! Common, display-only OSC 7/133 integration. Never an authorization source.

use std::cell::Cell;
use std::io::{IsTerminal, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

use crate::EmbeddedShell;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Mode {
    #[default]
    Auto,
    Off,
    On,
}

impl Mode {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            "on" => Some(Self::On),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct Session {
    enabled: Cell<bool>,
    activity: Cell<bool>,
    host: String,
}

impl Session {
    pub fn detect(shell: &EmbeddedShell, mode: Mode) -> Self {
        let supported_output = shell.is_interactive()
            && std::io::stdin().is_terminal()
            && crate::style::stdout().ansi
            && crate::style::stderr().ansi;
        let multiplexed = shell.var("TMUX").is_some() || shell.var("STY").is_some();
        let external = [
            "__vsc_prompt_cmd",
            "__vsc_precmd",
            "__wezterm_precmd",
            "__wezterm_semantic_precmd",
            "__wezterm_osc7",
            "wezterm_precmd",
            "iterm2_precmd",
        ]
        .iter()
        .any(|name| shell.has_function(name));
        let enabled = enabled(
            mode,
            supported_output,
            shell.var("TERM_PROGRAM").as_deref(),
            multiplexed,
            external,
        );
        if mode == Mode::On && external {
            eprintln!("nosh: terminal integration already loaded; not adding duplicate markers");
        }
        let host = if enabled {
            let mut buffer = [0u8; 256];
            // SAFETY: gethostname writes only into the supplied buffer.
            if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
                eprintln!(
                    "nosh: terminal cwd reporting unavailable: {}",
                    std::io::Error::last_os_error()
                );
                return Self::default();
            }
            let length = buffer
                .iter()
                .position(|byte| *byte == 0)
                .unwrap_or(buffer.len());
            encode(&buffer[..length], false)
        } else {
            String::new()
        };
        Self {
            enabled: Cell::new(enabled),
            activity: Cell::new(false),
            host,
        }
    }

    pub fn enabled(&self) -> bool {
        self.enabled.get()
    }

    fn write(&self, text: &str) {
        if !self.enabled() {
            return;
        }
        if let Err(error) = std::io::stdout().flush() {
            self.enabled.set(false);
            eprintln!("nosh: terminal integration disabled: {error}");
            return;
        }
        // Reedline paints on stderr; keep all nosh-owned markers on that
        // stream while independently requiring stdout to be a terminal.
        let mut output = std::io::stderr().lock();
        if let Err(error) = output
            .write_all(text.as_bytes())
            .and_then(|()| output.flush())
        {
            self.enabled.set(false);
            eprintln!("nosh: terminal integration disabled: {error}");
        }
    }

    pub fn cwd(&self, cwd: &Path) {
        if self.enabled() {
            self.write(&cwd_marker(&self.host, cwd));
        }
    }

    pub fn begin(&self) -> Region<'_> {
        self.activity.set(true);
        self.write("\x1b]133;C\x1b\\");
        Region {
            session: self,
            finished: false,
        }
    }

    pub fn cancel_input(&self) {
        self.write(&finished_marker(None));
    }

    pub fn finish_input(&self) {
        if !self.activity.replace(false) {
            self.cancel_input();
        }
    }
}

fn enabled(
    mode: Mode,
    output: bool,
    host: Option<&str>,
    multiplexed: bool,
    external: bool,
) -> bool {
    output
        && !external
        && match mode {
            Mode::Off => false,
            Mode::On => true,
            Mode::Auto => !multiplexed && matches!(host, Some("WezTerm" | "vscode")),
        }
}

pub(crate) struct Region<'a> {
    session: &'a Session,
    finished: bool,
}

impl Region<'_> {
    pub fn finish(mut self, code: i32) {
        self.session.write(&finished_marker(Some(code)));
        self.finished = true;
    }
}

impl Drop for Region<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.session.cancel_input();
        }
    }
}

fn finished_marker(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("\x1b]133;D;{code}\x1b\\"),
        None => "\x1b]133;D\x1b\\".into(),
    }
}

fn cwd_marker(host: &str, cwd: &Path) -> String {
    format!(
        "\x1b]7;file://{host}{}\x1b\\",
        encode(cwd.as_os_str().as_bytes(), true)
    )
}

fn encode(bytes: &[u8], path: bool) -> String {
    use std::fmt::Write;
    let mut result = String::new();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (path && byte == b'/')
        {
            result.push(char::from(byte));
        } else {
            write!(result, "%{byte:02X}").expect("writing to String");
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn conservative_auto_and_explicit_overrides() {
        for host in [Some("WezTerm"), Some("vscode")] {
            assert!(enabled(Mode::Auto, true, host, false, false));
            assert!(!enabled(Mode::Auto, true, host, true, false));
            assert!(!enabled(Mode::Auto, true, host, false, true));
        }
        for host in [None, Some(""), Some("unknown")] {
            assert!(!enabled(Mode::Auto, true, host, false, false));
            assert!(enabled(Mode::On, true, host, false, false));
        }
        for mode in [Mode::Auto, Mode::Off, Mode::On] {
            assert!(!enabled(mode, false, Some("WezTerm"), false, false));
        }
        assert!(!enabled(Mode::Off, true, Some("WezTerm"), false, false));
    }

    #[test]
    fn uri_encodes_control_unicode_and_non_utf8_without_sequence_injection() {
        let path = OsString::from_vec(b"/a b/\xe4\xb8\xad/%;#?\x1b\x07\n\xff".to_vec());
        assert_eq!(
            cwd_marker("host", Path::new(&path)),
            "\x1b]7;file://host/a%20b/%E4%B8%AD/%25%3B%23%3F%1B%07%0A%FF\x1b\\"
        );
        assert_eq!(finished_marker(Some(17)), "\x1b]133;D;17\x1b\\");
        assert_eq!(finished_marker(None), "\x1b]133;D\x1b\\");
    }
}
