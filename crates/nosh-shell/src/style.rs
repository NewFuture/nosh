//! Safe terminal text, column-aware layout and optional ANSI styling.

use std::borrow::Cow;

pub use nosh_hub::terminal::{Terminal, stderr, stdout};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn enabled() -> bool {
    stderr().color
}

pub fn glyph<'a>(unicode: &'a str, ascii: &'a str) -> &'a str {
    stderr().glyph(unicode, ascii)
}

fn paint(code: &str, s: &str) -> String {
    stderr().paint(code, s)
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
    struct Text(String);
    impl vte::Perform for Text {
        fn print(&mut self, c: char) {
            self.0.push(c);
        }
        fn execute(&mut self, b: u8) {
            self.0.push(char::from(b));
        }
    }
    let mut text = Text(String::with_capacity(s.len()));
    // No OSC payload is needed: links, titles and other terminal commands
    // are discarded, including when their payload is very large.
    vte::Parser::<0>::new_with_size().advance(&mut text, s.as_bytes());
    text.0
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
pub fn visible(s: &str) -> Cow<'_, str> {
    escape_hidden(s, is_hidden)
}

/// Prose may use joiners (emoji, Indic and Arabic scripts). Commands still
/// use `visible`, and bidi controls remain visible in both.
pub fn visible_text(s: &str) -> Cow<'_, str> {
    escape_hidden(s, |c| is_hidden(c) && !matches!(c, '\u{200c}' | '\u{200d}'))
}

fn escape_hidden(s: &str, hidden: impl Fn(char) -> bool) -> Cow<'_, str> {
    if !s.chars().any(&hidden) {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if !hidden(c) {
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
    Cow::Owned(out)
}

/// One line of command output made safe to print: text overwritten by `\r`
/// is dropped, escape sequences removed, other hidden characters shown.
pub fn safe_output_line(line: &str) -> String {
    let line = strip_ansi(line);
    let line = line.trim_end_matches('\r');
    let line = line.rsplit('\r').next().unwrap_or(line);
    visible_text(line).into_owned()
}

pub fn width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

/// Fits plain, single-line text to terminal columns without splitting a
/// grapheme. Tabs use the terminal's usual eight-column stops.
pub fn clip_line(s: &str, columns: usize, offset: usize, ellipsis: &str) -> String {
    let mut expanded = String::with_capacity(s.len());
    let mut column = offset;
    for g in s.graphemes(true) {
        if g == "\t" {
            let spaces = 8 - column % 8;
            expanded.extend(std::iter::repeat_n(' ', spaces));
            column += spaces;
        } else {
            expanded.push_str(g);
            column += width(g);
        }
    }
    if width(&expanded) <= columns {
        return expanded;
    }
    let marker: String = ellipsis
        .graphemes(true)
        .scan(0, |used, g| {
            *used += width(g);
            (*used <= columns).then_some(g)
        })
        .collect();
    let budget = columns.saturating_sub(width(&marker));
    let mut used = 0;
    let mut end = 0;
    for (i, g) in expanded.grapheme_indices(true) {
        used += width(g);
        if used > budget {
            break;
        }
        end = i + g.len();
    }
    expanded.truncate(end);
    expanded.push_str(&marker);
    expanded
}

const MAX_OUTPUT_LINE: usize = 4096;

/// Streaming ANSI removal and bounded line buffering, one instance per
/// stdout/stderr stream. Parser state survives chunk and line boundaries.
#[derive(Default)]
pub struct OutputBuffer {
    parser: vte::Parser<0>,
    lines: OutputLines,
}

#[derive(Default)]
struct OutputLines {
    line: String,
    complete: Vec<String>,
    carriage_return: bool,
    truncated: bool,
}

impl OutputLines {
    fn take_line(&mut self) -> String {
        let mut line = std::mem::take(&mut self.line);
        if self.truncated {
            // The last retained grapheme may continue beyond the byte cap.
            if let Some((i, _)) = line.grapheme_indices(true).next_back() {
                line.truncate(i);
            }
            line.push_str("...");
        }
        self.truncated = false;
        self.carriage_return = false;
        visible_text(&line).into_owned()
    }

    fn push(&mut self, c: char) {
        if self.carriage_return {
            self.line.clear();
            self.truncated = false;
            self.carriage_return = false;
        }
        if !self.truncated && self.line.len() + c.len_utf8() <= MAX_OUTPUT_LINE {
            self.line.push(c);
        } else {
            self.truncated = true;
        }
    }
}

impl vte::Perform for OutputLines {
    fn print(&mut self, c: char) {
        self.push(c);
    }

    fn execute(&mut self, b: u8) {
        match b {
            b'\r' => self.carriage_return = true,
            b'\n' => {
                let line = self.take_line();
                self.complete.push(line);
            }
            _ => self.push(char::from(b)),
        }
    }
}

impl OutputBuffer {
    pub fn push(&mut self, s: &str) -> Vec<String> {
        self.parser.advance(&mut self.lines, s.as_bytes());
        std::mem::take(&mut self.lines.complete)
    }

    pub fn finish(&mut self) -> Option<String> {
        (!self.lines.line.is_empty() || self.lines.truncated).then(|| self.lines.take_line())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn handles_terminal_escape_families_without_eating_text() {
        for s in [
            "\x1b[1@label",
            "\x1b[1~label",
            "\x1b]8;;https://example.invalid\x07label\x1b]8;;\x07",
            "\x1b]8;;https://example.invalid\x1b\\label\x1b]8;;\x1b\\",
            "\x1bPignored\x1b\\label",
            "\x1b_ignored\x1b\\label",
            "\x1b^ignored\x1b\\label",
            "\x1b(Blabel",
        ] {
            assert_eq!(strip_ansi(s), "label", "{s:?}");
        }
        assert_eq!(strip_ansi("a\tb\nc\r"), "a\tb\nc\r");
        assert_eq!(strip_ansi("label\x1b[31"), "label");
        assert_eq!(strip_ansi("label\x1b]unfinished"), "label");
        assert_eq!(
            safe_output_line("10%\r20%\r\n".trim_end_matches('\n')),
            "20%"
        );
    }

    #[test]
    fn prose_preserves_joiners_but_not_hidden_commands() {
        let emoji = "\u{1f469}\u{200d}\u{1f4bb}";
        assert_eq!(visible_text(emoji), emoji);
        assert!(visible(emoji).contains("\\u{200d}"));
        assert_eq!(
            visible_text("a\u{202e}\u{200b}b\x1b"),
            "a\\u{202e}\\u{200b}b\\e"
        );
    }

    #[test]
    fn clipping_counts_columns_and_keeps_whole_graphemes() {
        for text in [
            "abc".repeat(40),
            "\u{4e2d}".repeat(50),
            "\u{20bb7}".repeat(50),
            "\u{1f469}\u{200d}\u{1f4bb}".repeat(50),
            "e\u{301}".repeat(100),
            "\u{1f1e8}\u{1f1f3}".repeat(50),
            "x\t".repeat(30),
        ] {
            for columns in 0..110 {
                let result = clip_line(&text, columns, 4, "...");
                assert!(width(&result) <= columns, "{columns}: {result:?}");
                if !text.contains('\t') {
                    let kept = result.strip_suffix("...").unwrap_or(&result);
                    if !kept.is_empty() && !kept.chars().all(|c| c == '.') {
                        assert!(
                            text.grapheme_indices(true).any(|(i, _)| i == kept.len())
                                || kept.len() == text.len()
                        );
                    }
                }
            }
        }
        assert_eq!(clip_line("a\tb", 20, 4, "..."), "a   b");
        assert_eq!(clip_line("e\u{301}x", 1, 0, ""), "e\u{301}");
        assert_eq!(
            clip_line("\u{1f469}\u{200d}\u{1f4bb}x", 2, 0, ""),
            "\u{1f469}\u{200d}\u{1f4bb}"
        );
    }

    #[test]
    fn command_output_is_independent_of_chunk_boundaries() {
        let input = "\x1b[31m\u{4f60}\u{597d}\x1b[0m\r\n\
                     old\rnew\r\n\x1b]8;;url\npayload\x1b\\link\x1b]8;;\x07\n\
                     \u{1f469}\u{200d}\u{1f4bb}\tend";
        let expected = vec![
            "\u{4f60}\u{597d}",
            "new",
            "link",
            "\u{1f469}\u{200d}\u{1f4bb}\tend",
        ];
        for split in input.char_indices().map(|(i, _)| i).chain([input.len()]) {
            let mut buffer = OutputBuffer::default();
            let mut lines = buffer.push(&input[..split]);
            lines.extend(buffer.push(&input[split..]));
            lines.extend(buffer.finish());
            assert_eq!(lines, expected, "split {split}");
        }
    }

    #[test]
    fn long_lines_stay_bounded_and_carriage_return_can_replace_them() {
        let mut buffer = OutputBuffer::default();
        buffer.push(&"a".repeat(MAX_OUTPUT_LINE + 20));
        assert!(buffer.lines.line.len() <= MAX_OUTPUT_LINE);
        assert_eq!(buffer.push("\rshort\r\n"), ["short"]);
        buffer.push(&"e\u{301}".repeat(MAX_OUTPUT_LINE));
        let line = buffer.finish().unwrap();
        assert!(line.len() <= MAX_OUTPUT_LINE + 3);
        assert!(line.ends_with("..."));
        let input = format!("\x1b]0;{}\x07ok\n", "payload".repeat(5000));
        assert_eq!(buffer.push(&input), ["ok"]);
    }
}
