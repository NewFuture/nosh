//! Bounded, in-memory evidence from the user's terminal, separate from agent logs.

use std::collections::VecDeque;
use std::time::Duration;

use crate::backend::UserCommand;
use crate::style;

pub const OUTPUT_BYTES: usize = 4096;
pub const METADATA_BYTES: usize = 1024;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CaptureUserOutput {
    #[default]
    Off,
    Last,
}

impl CaptureUserOutput {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "last" => Some(Self::Last),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputUnavailable {
    NoPty,
    ControlFailure,
    FullScreen,
}

impl OutputUnavailable {
    pub fn reason(self) -> &'static str {
        match self {
            Self::NoPty => "no_capture_pty",
            Self::ControlFailure => "capture_control_failure",
            Self::FullScreen => "full_screen_terminal",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputState {
    NotCaptured,
    Unavailable(OutputUnavailable),
    Captured,
}

/// One immutable request snapshot. These bounded copies are not shell history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserOutput {
    pub command_id: u64,
    pub command: String,
    pub cwd: String,
    pub command_truncated: bool,
    pub cwd_truncated: bool,
    pub exit: i32,
    pub duration: Duration,
    pub state: OutputState,
    pub terminal_source: bool,
    pub text: String,
    pub observed_bytes: Option<u64>,
    pub truncated: bool,
    pub incomplete: bool,
    pub mixed: bool,
}

impl UserOutput {
    pub(crate) fn unavailable(command: &UserCommand, state: OutputState) -> Self {
        let (line, command_truncated) = bounded_metadata(&command.line);
        let cwd_text = command.cwd.to_string_lossy();
        let (cwd, cwd_truncated) = bounded_metadata(&cwd_text);
        Self {
            command_id: command.id,
            command: line.to_string(),
            cwd: cwd.to_string(),
            command_truncated,
            cwd_truncated,
            exit: command.exit,
            duration: command.duration,
            state,
            terminal_source: false,
            text: String::new(),
            observed_bytes: None,
            truncated: false,
            incomplete: false,
            mixed: false,
        }
    }

    pub(crate) fn captured(
        command: &UserCommand,
        snapshot: CapturedOutput,
        mixed: bool,
        interrupted: bool,
    ) -> Self {
        let mut result = Self::unavailable(command, OutputState::Captured);
        result.terminal_source = true;
        result.observed_bytes = Some(snapshot.observed);
        result.truncated = snapshot.truncated;
        result.incomplete = snapshot.incomplete || interrupted;
        result.mixed = mixed;
        if snapshot.full_screen {
            result.state = OutputState::Unavailable(OutputUnavailable::FullScreen);
        } else if !mixed {
            result.text = snapshot.text;
        }
        result
    }

    pub fn has_body(&self) -> bool {
        self.state == OutputState::Captured && !self.mixed
    }
}

pub fn bounded_metadata(value: &str) -> (&str, bool) {
    let mut end = value.len().min(METADATA_BYTES);
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    (&value[..end], end != value.len())
}

#[derive(Debug, Clone, Default)]
pub struct CapturedOutput {
    pub text: String,
    pub observed: u64,
    pub truncated: bool,
    pub incomplete: bool,
    pub full_screen: bool,
}

/// Shared by terminal evidence and the existing agent pipe capture.
#[derive(Default)]
pub(crate) struct Utf8Decoder {
    pending: Vec<u8>,
}

impl Utf8Decoder {
    pub(crate) fn push(&mut self, bytes: &[u8]) -> String {
        self.pending.extend_from_slice(bytes);
        let mut valid = 0;
        while valid < self.pending.len() {
            match std::str::from_utf8(&self.pending[valid..]) {
                Ok(_) => {
                    valid = self.pending.len();
                    break;
                }
                Err(error) => {
                    valid += error.valid_up_to();
                    if let Some(bad) = error.error_len() {
                        valid += bad;
                    } else {
                        break;
                    }
                }
            }
        }
        let text = String::from_utf8_lossy(&self.pending[..valid]).into_owned();
        self.pending.drain(..valid);
        debug_assert!(self.pending.len() <= 3);
        text
    }

    pub(crate) fn finish(&mut self) -> String {
        let text = String::from_utf8_lossy(&self.pending).into_owned();
        self.pending.clear();
        text
    }
}

pub(crate) struct OutputCollector {
    utf8: Utf8Decoder,
    parser: vte::Parser<0>,
    tail: TextTail,
    observed: u64,
    plain_ascii: bool,
}

impl Default for OutputCollector {
    fn default() -> Self {
        Self {
            utf8: Utf8Decoder::default(),
            parser: vte::Parser::default(),
            tail: TextTail::default(),
            observed: 0,
            plain_ascii: true,
        }
    }
}

impl OutputCollector {
    pub(crate) fn push(&mut self, bytes: &[u8]) {
        self.observed = self.observed.saturating_add(bytes.len() as u64);
        for block in bytes.chunks(16 * 1024) {
            // A plain initial line has no parser/decoder state to carry. Keep
            // its tail in bulk rather than shifting it once per character.
            if self.plain_ascii && block.iter().all(|b| *b >= b' ' && *b <= b'~') {
                self.tail.push_ascii(block);
                continue;
            }
            self.plain_ascii = false;
            let text = self.utf8.push(block);
            self.parser.advance(&mut self.tail, text.as_bytes());
        }
    }

    pub(crate) fn finish(mut self) -> CapturedOutput {
        self.parser
            .advance(&mut self.tail, self.utf8.finish().as_bytes());
        CapturedOutput {
            text: String::from_utf8(self.tail.bytes.into_iter().collect())
                .expect("terminal tail only contains whole UTF-8 characters"),
            observed: self.observed,
            truncated: self.tail.truncated,
            incomplete: self.tail.incomplete,
            full_screen: self.tail.full_screen,
        }
    }
}

#[derive(Default)]
struct TextTail {
    bytes: VecDeque<u8>,
    line_bytes: usize,
    carriage_return: bool,
    truncated: bool,
    incomplete: bool,
    full_screen: bool,
}

impl TextTail {
    fn push_ascii(&mut self, bytes: &[u8]) {
        let keep = &bytes[bytes.len().saturating_sub(OUTPUT_BYTES)..];
        let discard = (self.bytes.len() + keep.len()).saturating_sub(OUTPUT_BYTES);
        self.truncated |= self.bytes.len() + bytes.len() > OUTPUT_BYTES;
        self.bytes.drain(..discard);
        self.bytes.extend(keep);
        self.line_bytes = self.bytes.len();
    }

    fn push(&mut self, text: &str) {
        if self.full_screen {
            return;
        }
        if self.carriage_return {
            self.bytes.truncate(self.bytes.len() - self.line_bytes);
            self.line_bytes = 0;
            self.carriage_return = false;
        }
        while self.bytes.len() + text.len() > OUTPUT_BYTES {
            let prefix = self.bytes.len() - self.line_bytes;
            let mut removed = 1usize;
            self.bytes.pop_front();
            while self.bytes.front().is_some_and(|b| b & 0xc0 == 0x80) {
                self.bytes.pop_front();
                removed += 1;
            }
            self.line_bytes -= removed.saturating_sub(prefix);
            self.truncated = true;
        }
        self.bytes.extend(text.as_bytes());
        self.line_bytes += text.len();
    }
}

impl vte::Perform for TextTail {
    fn print(&mut self, ch: char) {
        let mut encoded = [0; 4];
        self.push(&style::visible_text(ch.encode_utf8(&mut encoded)));
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            b'\r' => self.carriage_return = true,
            b'\n' => {
                self.carriage_return = false;
                self.push("\n");
                self.line_bytes = 0;
            }
            _ => self.print(char::from(byte)),
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if intermediates == b"?"
            && matches!(action, 'h' | 'l')
            && params
                .iter()
                .flatten()
                .any(|p| matches!(p, 47 | 1047 | 1049))
        {
            self.full_screen = true;
            self.bytes.clear();
            self.line_bytes = 0;
        } else if ignore || action != 'm' {
            self.incomplete = true;
        }
    }

    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {
        self.incomplete = true;
    }

    fn hook(&mut self, _: &vte::Params, _: &[u8], _: bool, _: char) {
        self.incomplete = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_is_independent_of_byte_boundaries() {
        let input = "first\n\x1b[31m\u{4f60}\u{597d}\x1b[0m\rprogress\n\
                     \x1b]0;ignored\x07last\u{1f469}\u{200d}\u{1f4bb}"
            .as_bytes();
        for split in 0..=input.len() {
            let mut collector = OutputCollector::default();
            collector.push(&input[..split]);
            collector.push(&input[split..]);
            let out = collector.finish();
            assert_eq!(out.text, "first\nprogress\nlast\u{1f469}\u{200d}\u{1f4bb}");
            assert_eq!(out.observed, input.len() as u64);
            assert!(!out.truncated);
        }
    }

    #[test]
    fn tails_and_parser_storage_stay_bounded() {
        let mut collector = OutputCollector::default();
        for _ in 0..256 {
            collector.push(&vec![b'x'; 16384]);
            assert!(collector.tail.bytes.len() <= OUTPUT_BYTES);
            assert!(collector.utf8.pending.len() <= 3);
        }
        collector.push(b"\rshort\n\x1b]0;");
        for _ in 0..256 {
            collector.push(&vec![b'z'; 16384]);
            assert!(collector.tail.bytes.len() <= OUTPUT_BYTES);
        }
        collector.push(b"\x07final\xff\xe4");
        let out = collector.finish();
        assert_eq!(out.text, "short\nfinal\u{fffd}\u{fffd}");
        assert!(out.truncated);
    }

    #[test]
    fn truncation_is_after_utf8_and_hidden_character_expansion() {
        for input in [
            "\u{4e2d}".repeat(3000),
            "\0".repeat(3000),
            "a\n".repeat(4000),
        ] {
            let mut collector = OutputCollector::default();
            collector.push(input.as_bytes());
            collector.push(b"LAST");
            let out = collector.finish();
            assert!(out.text.len() <= OUTPUT_BYTES);
            assert!(out.text.ends_with("LAST"));
            assert!(out.truncated);
        }
    }

    #[test]
    fn full_screen_and_cursor_motion_are_not_complete_text() {
        let mut collector = OutputCollector::default();
        collector.push(b"before\x1b[?1049hsecret\x1b[?1049lafter");
        let out = collector.finish();
        assert!(out.full_screen);
        assert!(out.text.is_empty());
        let mut collector = OutputCollector::default();
        collector.push(b"\x1b[Htext");
        assert!(collector.finish().incomplete);
        let empty = OutputCollector::default().finish();
        assert_eq!(empty.observed, 0);
        assert!(empty.text.is_empty());
        assert!(!empty.incomplete);
    }

    #[test]
    fn metadata_is_bounded_without_modifying_the_executable_command() {
        let command = UserCommand {
            id: 9,
            line: "\u{4e2d}".repeat(900),
            cwd: std::path::PathBuf::from("\u{4e2d}".repeat(900)),
            exit: 17,
            duration: Duration::from_millis(1),
        };
        let output = UserOutput::unavailable(&command, OutputState::NotCaptured);
        assert!(output.command.len() <= METADATA_BYTES && output.command_truncated);
        assert!(output.cwd.len() <= METADATA_BYTES && output.cwd_truncated);
        assert_eq!(command.line.len(), 2700);
        assert_eq!(output.observed_bytes, None);
    }
}
