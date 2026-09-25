//! Output capabilities, shared by progress bars and the shell UI.

use std::io::IsTerminal;
use std::sync::OnceLock;

#[derive(Debug, Clone, Copy)]
pub struct Terminal {
    pub tty: bool,
    pub ansi: bool,
    pub color: bool,
    pub unicode: bool,
}

impl Terminal {
    fn detect(tty: bool) -> Self {
        let term = std::env::var("TERM").ok();
        let locale = ["LC_ALL", "LC_CTYPE", "LANG"]
            .iter()
            .find_map(|k| std::env::var(k).ok().filter(|v| !v.is_empty()));
        let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty())
            || std::env::var_os("CLICOLOR").is_some_and(|v| v == "0");
        Self::with_env(tty, term.as_deref(), locale.as_deref(), no_color)
    }

    fn with_env(tty: bool, term: Option<&str>, locale: Option<&str>, no_color: bool) -> Self {
        let ansi = tty && term.is_some_and(|t| !matches!(t, "" | "dumb" | "unknown"));
        let utf8 = locale.is_none_or(|l| l.to_ascii_lowercase().replace('-', "").contains("utf8"));
        Self {
            tty,
            ansi,
            color: ansi && !no_color,
            unicode: ansi && utf8 && !matches!(term, Some("linux" | "vt100" | "vt220")),
        }
    }

    pub fn glyph<'a>(self, unicode: &'a str, ascii: &'a str) -> &'a str {
        if self.unicode { unicode } else { ascii }
    }

    pub fn paint(self, code: &str, s: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

pub fn stdout() -> Terminal {
    static TERM: OnceLock<Terminal> = OnceLock::new();
    *TERM.get_or_init(|| Terminal::detect(std::io::stdout().is_terminal()))
}

pub fn stderr() -> Terminal {
    static TERM: OnceLock<Terminal> = OnceLock::new();
    *TERM.get_or_init(|| Terminal::detect(std::io::stderr().is_terminal()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_follow_the_destination_and_environment() {
        for term in [
            "xterm-256color",
            "screen",
            "screen-256color",
            "tmux-256color",
            "rxvt-unicode",
        ] {
            let t = Terminal::with_env(true, Some(term), Some("zh_CN.UTF-8"), false);
            assert!(t.ansi && t.color && t.unicode, "{term}");
            let t = Terminal::with_env(false, Some(term), Some("C.UTF-8"), false);
            assert!(!t.ansi && !t.color && !t.unicode);
        }
        for term in [None, Some(""), Some("dumb"), Some("unknown")] {
            let t = Terminal::with_env(true, term, Some("C.UTF-8"), false);
            assert!(!t.ansi && !t.color && !t.unicode);
        }
        let t = Terminal::with_env(true, Some("xterm"), Some("en_US.utf8"), true);
        assert!(
            t.ansi && t.unicode && !t.color,
            "NO_COLOR does not disable cursor control"
        );
        for locale in ["C", "POSIX", "zh_CN.GB18030", "en_US.ISO-8859-1"] {
            let t = Terminal::with_env(true, Some("xterm"), Some(locale), false);
            assert!(t.ansi && !t.unicode, "{locale}");
        }
        for term in ["linux", "vt100", "vt220"] {
            assert!(!Terminal::with_env(true, Some(term), Some("C.UTF-8"), false).unicode);
        }
    }
}
