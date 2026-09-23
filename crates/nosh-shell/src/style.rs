//! Minimal ANSI styling for nosh's own output (off with `NO_COLOR` or when
//! stderr is not a terminal).

use std::io::IsTerminal;
use std::sync::OnceLock;

pub fn enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty()) && std::io::stderr().is_terminal()
    })
}

fn paint(code: &str, s: &str) -> String {
    if enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}

pub fn bold(s: &str) -> String {
    paint("1", s)
}

pub fn red(s: &str) -> String {
    paint("31", s)
}

pub fn red_bold(s: &str) -> String {
    paint("1;31", s)
}

pub fn green(s: &str) -> String {
    paint("32", s)
}

pub fn yellow(s: &str) -> String {
    paint("33", s)
}

pub fn cyan(s: &str) -> String {
    paint("36", s)
}

pub fn blue_bold(s: &str) -> String {
    paint("1;34", s)
}

/// Removes ANSI escape sequences (for width calculations and logs).
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for c in chars.by_ref() {
                    if c.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_escapes() {
        assert_eq!(super::strip_ansi("\x1b[1;31mYOLO\x1b[0m ok"), "YOLO ok");
    }
}
