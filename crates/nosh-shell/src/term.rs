//! Terminal input outside the line editor (guard prompt, approval cards).
//! Keys come from the controlling terminal even when stdin is a pipe.

use std::borrow::Cow;
use std::io::{self, IsTerminal, Write};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers,
};
use reedline::Signal;
use unicode_segmentation::UnicodeSegmentation;

use crate::style;

/// Whether a controlling terminal can be used for prompts.
pub fn available() -> bool {
    std::io::stderr().is_terminal()
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/tty")
            .is_ok()
}

/// Query the actual output descriptor, not stdout or an unrelated `/dev/tty`.
pub fn stderr_columns() -> Option<usize> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: ioctl writes a winsize into valid storage; it is read only on success.
    let result = unsafe { libc::ioctl(libc::STDERR_FILENO, libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: the successful ioctl initialized size.
    let columns = usize::from(unsafe { size.assume_init() }.ws_col);
    (columns > 0).then_some(columns)
}

/// Discards pending typeahead so buffered keystrokes cannot answer a prompt.
pub fn flush_input() {
    use std::os::fd::AsRawFd;
    if let Ok(tty) = std::fs::OpenOptions::new().read(true).open("/dev/tty") {
        // SAFETY: valid open fd for the duration of the call.
        unsafe {
            libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH);
        }
    }
}

pub fn is_ctrl(k: &KeyEvent, c: char) -> bool {
    k.modifiers.contains(KeyModifiers::CONTROL) && k.code == KeyCode::Char(c)
}

struct RawInput {
    restore: bool,
    paste: bool,
}

impl RawInput {
    fn new(paste: bool) -> io::Result<Self> {
        let restore = !crossterm::terminal::is_raw_mode_enabled()?;
        if restore {
            crossterm::terminal::enable_raw_mode()?;
        }
        let mut input = Self {
            restore,
            paste: false,
        };
        if paste {
            crossterm::execute!(io::stderr(), EnableBracketedPaste)?;
            input.paste = true;
        }
        Ok(input)
    }
}

impl Drop for RawInput {
    fn drop(&mut self) {
        if self.paste
            && let Err(e) = crossterm::execute!(io::stderr(), DisableBracketedPaste)
        {
            eprintln!("nosh: cannot restore terminal paste mode: {e}");
        }
        if self.restore
            && let Err(e) = crossterm::terminal::disable_raw_mode()
        {
            eprintln!("nosh: cannot restore terminal input mode: {e}");
        }
    }
}

/// Reads one key press in raw mode; `None` without a terminal.
pub fn read_key() -> Option<KeyEvent> {
    let read = || -> io::Result<KeyEvent> {
        if !available() {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "no visible terminal for input",
            ));
        }
        let _raw = RawInput::new(false)?;
        loop {
            match event::read()? {
                Event::Key(k) if k.kind != KeyEventKind::Release => return Ok(k),
                _ => {}
            }
        }
    };
    match read() {
        Ok(key) => Some(key),
        Err(e) => {
            eprintln!("nosh: cannot read terminal input: {e}");
            None
        }
    }
}

/// Reads a short line on stderr, starting from `initial`.
/// `None` on Esc, Ctrl-C or Ctrl-D.
pub fn read_text(prompt: &str, initial: &str) -> Option<String> {
    text_result(read_short_line(prompt, initial, false))
}

fn text_result(result: io::Result<Signal>) -> Option<String> {
    match result {
        Ok(Signal::Success(text)) => Some(text),
        Ok(_) => None,
        Err(e) => {
            eprintln!("nosh: cannot read terminal input: {e}");
            None
        }
    }
}

pub(crate) fn read_plain_line(prompt: &str, initial: &str) -> io::Result<Signal> {
    read_short_line(prompt, initial, true)
}

fn read_short_line(prompt: &str, initial: &str, command: bool) -> io::Result<Signal> {
    if !available() {
        return Err(io::Error::new(
            io::ErrorKind::NotConnected,
            "no visible terminal for input",
        ));
    }
    let ansi = style::stderr().ansi;
    let _raw = RawInput::new(ansi)?;
    let mut err = io::stderr().lock();
    let mut buf = initial.to_string();
    let mut line = InputLine {
        prompt: if ansi {
            prompt.to_string()
        } else {
            style::strip_ansi(prompt)
        },
        columns: stderr_columns().unwrap_or(80),
        previous_width: 0,
        ansi,
        command,
    };
    line.start(&mut err)?;
    line.draw(&mut err, &buf)?;
    let result = loop {
        match event::read()? {
            Event::Paste(s) => {
                buf.extend(s.chars().filter(|c| !c.is_control()));
            }
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Enter => break Signal::Success(buf),
                    KeyCode::Esc => break Signal::CtrlC,
                    KeyCode::Char('c') if ctrl => break Signal::CtrlC,
                    KeyCode::Char('d') if ctrl => break Signal::CtrlD,
                    KeyCode::Char('u') if ctrl => buf.clear(),
                    KeyCode::Backspace => pop_grapheme(&mut buf),
                    KeyCode::Char(c) if !ctrl => {
                        buf.push(c);
                    }
                    _ => {}
                }
            }
            Event::Resize(columns, _) => {
                // Reflow differs between emulators. Start a fresh editing row
                // instead of guessing how many old rows the terminal reflowed.
                write!(err, "\r\n")?;
                line.columns = stderr_columns().unwrap_or(usize::from(columns).max(1));
                line.previous_width = 0;
                line.start(&mut err)?;
            }
            _ => {}
        }
        line.draw(&mut err, &buf)?;
    };
    write!(err, "\r\n")?;
    err.flush()?;
    Ok(result)
}

fn pop_grapheme(buf: &mut String) {
    if let Some((i, _)) = buf.grapheme_indices(true).next_back() {
        buf.truncate(i);
    }
}

fn input_tail(buf: &str, columns: usize, command: bool) -> String {
    let text = if command {
        style::visible(buf)
    } else {
        style::visible_text(buf)
    };
    let text = text.replace('\n', "\\n").replace('\t', "\\t");
    if style::width(&text) <= columns {
        return text;
    }
    let marker = style::clip_line(style::glyph("…", "..."), columns, 0, "");
    let budget = columns.saturating_sub(style::width(&marker));
    let mut used = 0;
    let mut start = text.len();
    for (i, g) in text.grapheme_indices(true).rev() {
        used += style::width(g);
        if used > budget {
            break;
        }
        start = i;
    }
    format!("{marker}{}", &text[start..])
}

struct InputLine {
    prompt: String,
    columns: usize,
    previous_width: usize,
    ansi: bool,
    command: bool,
}

impl InputLine {
    fn start(&mut self, out: &mut impl Write) -> io::Result<()> {
        if self.prompt.contains('\n')
            || style::width(&style::strip_ansi(&self.prompt)) + 8 >= self.columns
        {
            write!(out, "{}\r\n", self.prompt.trim_end().replace('\n', "\r\n"))?;
            self.prompt.clear();
        }
        Ok(())
    }

    fn draw(&mut self, out: &mut impl Write, buf: &str) -> io::Result<()> {
        let prompt_width = style::width(&style::strip_ansi(&self.prompt));
        let room = self.columns.saturating_sub(prompt_width + 1);
        let text = input_tail(buf, room, self.command);
        let width = prompt_width + style::width(&text);
        if self.ansi {
            write!(out, "\r\x1b[K{}{text}", self.prompt)?;
        } else {
            let padding = " ".repeat(self.previous_width.saturating_sub(width));
            write!(
                out,
                "\r{}{text}{padding}\r{}{text}",
                self.prompt, self.prompt
            )?;
        }
        self.previous_width = width;
        out.flush()
    }
}

struct PlainPrompt(String);

impl reedline::Prompt for PlainPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        self.0.as_str().into()
    }
    fn render_prompt_right(&self) -> Cow<'_, str> {
        "".into()
    }
    fn render_prompt_indicator(&self, _: reedline::PromptEditMode) -> Cow<'_, str> {
        "".into()
    }
    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        "".into()
    }
    fn render_prompt_history_search_indicator(
        &self,
        _: reedline::PromptHistorySearch,
    ) -> Cow<'_, str> {
        "".into()
    }
}

/// Lets the user edit `initial`; `None` if cancelled.
pub fn edit_line(prompt: &str, initial: &str) -> Option<String> {
    if style::stdout().ansi
        && std::io::stdin().is_terminal()
        && !initial.chars().any(style::is_hidden)
    {
        let mut ed = reedline::Reedline::create().with_ansi_colors(style::stdout().color);
        ed.run_edit_commands(&[reedline::EditCommand::InsertString(initial.to_string())]);
        text_result(ed.read_line(&PlainPrompt(prompt.to_string())))
    } else {
        text_result(read_plain_line(prompt, initial))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backspace_removes_whole_graphemes() {
        for cluster in [
            "\u{1f600}",
            "\u{20bb7}",
            "e\u{301}",
            "\u{1f469}\u{200d}\u{1f4bb}",
            "\u{1f44d}\u{1f3fd}",
            "\u{1f1e8}\u{1f1f3}",
            "\u{2764}\u{fe0f}",
        ] {
            let mut buf = format!("a{cluster}");
            pop_grapheme(&mut buf);
            assert_eq!(buf, "a", "{cluster:?}");
        }
    }

    #[test]
    fn input_window_never_wraps_or_splits_graphemes() {
        let input = format!(
            "{}e\u{301}\u{1f469}\u{200d}\u{1f4bb}",
            "\u{4e2d}".repeat(50)
        );
        for columns in 0..100 {
            let tail = input_tail(&input, columns, false);
            assert!(style::width(&tail) <= columns);
            if tail.ends_with('\u{1f4bb}') {
                assert!(tail.ends_with("\u{1f469}\u{200d}\u{1f4bb}"));
            }
        }
        assert_eq!(input_tail("a\nb\tc\x1b", 80, true), "a\\nb\\tc\\e");
    }

    #[test]
    fn dumb_terminal_redraw_erases_the_old_input_without_ansi() {
        let mut line = InputLine {
            prompt: "> ".into(),
            columns: 20,
            previous_width: 0,
            ansi: false,
            command: false,
        };
        let mut out = Vec::new();
        line.draw(&mut out, "\u{1f600}").unwrap();
        out.clear();
        line.draw(&mut out, "").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\r>   \r> ");
    }
}
