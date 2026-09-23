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

/// Characters that make a terminal show something other than the text:
/// control characters (except newline and tab), bidirectional overrides and
/// zero-width characters.
pub fn is_hidden(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(
            c as u32,
            0x061c | 0x200b..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069 | 0xfeff
        )
}

/// Shows hidden characters as escapes (`\r`, `\e`, `\u{202e}`), so what is
/// displayed is what would run.
pub fn visible(s: &str) -> std::borrow::Cow<'_, str> {
    if !s.chars().any(is_hidden) {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if !is_hidden(c) {
            out.push(c);
            continue;
        }
        match c {
            '\r' => out.push_str("\\r"),
            '\x1b' => out.push_str("\\e"),
            c if (c as u32) < 0x80 => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push_str(&format!("\\u{{{:04x}}}", c as u32)),
        }
    }
    std::borrow::Cow::Owned(out)
}

/// One line of command output made safe to print: text overwritten by `\r`
/// is dropped, escape sequences removed, other hidden characters shown.
pub fn safe_output_line(line: &str) -> String {
    let line = line.trim_end_matches('\r');
    let line = line.rsplit('\r').next().unwrap_or(line);
    visible(&strip_ansi(line)).into_owned()
}

#[cfg(test)]
mod tests {
    #[test]
    fn strips_escapes() {
        assert_eq!(super::strip_ansi("\x1b[1;31mYOLO\x1b[0m ok"), "YOLO ok");
    }

    #[test]
    fn hidden_characters_are_shown() {
        use super::visible;
        assert_eq!(visible("ls -la"), "ls -la");
        assert_eq!(visible("a\tb\nc"), "a\tb\nc");
        assert_eq!(visible("rm -rf ~ #\r ls"), "rm -rf ~ #\\r ls");
        assert_eq!(visible("x\x1b[2Ky"), "x\\e[2Ky");
        assert_eq!(visible("a\u{202e}b"), "a\\u{202e}b");
        assert_eq!(visible("\x07"), "\\x07");
    }
}
