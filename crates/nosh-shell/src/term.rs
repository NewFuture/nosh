//! Terminal input outside the line editor (guard prompt, approval cards).
//! Keys come from the controlling terminal even when stdin is a pipe.

use std::borrow::Cow;
use std::io::{IsTerminal, Write};

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Whether a controlling terminal can be used for prompts.
pub fn available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .is_ok()
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

/// Reads one key press in raw mode; `None` without a terminal.
pub fn read_key() -> Option<KeyEvent> {
    crossterm::terminal::enable_raw_mode().ok()?;
    let key = loop {
        match event::read() {
            Ok(Event::Key(k)) if k.kind != KeyEventKind::Release => break Some(k),
            Ok(_) => {}
            Err(_) => break None,
        }
    };
    let _ = crossterm::terminal::disable_raw_mode();
    key
}

/// Reads a short line with echo on stderr, starting from `initial`.
/// `None` on Esc, Ctrl-C or Ctrl-D.
pub fn read_text(initial: &str) -> Option<String> {
    let mut err = std::io::stderr();
    let mut buf: Vec<char> = initial.chars().collect();
    let _ = write!(err, "{initial}");
    let _ = err.flush();
    crossterm::terminal::enable_raw_mode().ok()?;
    let res = loop {
        let ev = match event::read() {
            Ok(ev) => ev,
            Err(_) => break None,
        };
        match ev {
            Event::Paste(s) => {
                let s: String = s.chars().filter(|c| !c.is_control()).collect();
                let _ = write!(err, "{s}");
                buf.extend(s.chars());
            }
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                match k.code {
                    KeyCode::Enter => break Some(buf.iter().collect()),
                    KeyCode::Esc => break None,
                    KeyCode::Char('c' | 'd') if ctrl => break None,
                    KeyCode::Char('u') if ctrl => {
                        for c in buf.drain(..).rev() {
                            erase(&mut err, c);
                        }
                    }
                    KeyCode::Backspace => {
                        if let Some(c) = buf.pop() {
                            erase(&mut err, c);
                        }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        buf.push(c);
                        let _ = write!(err, "{c}");
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        let _ = err.flush();
    };
    let _ = crossterm::terminal::disable_raw_mode();
    let _ = writeln!(err);
    res
}

fn erase(w: &mut impl Write, c: char) {
    let wide = matches!(c as u32, 0x1100..=0x115f | 0x2e80..=0xa4cf | 0xac00..=0xd7a3 | 0xf900..=0xfaff | 0xfe30..=0xfe4f | 0xff00..=0xff60 | 0xffe0..=0xffe6);
    let _ = if wide {
        write!(w, "\x08\x08  \x08\x08")
    } else {
        write!(w, "\x08 \x08")
    };
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
    if std::io::stdout().is_terminal() && std::io::stdin().is_terminal() {
        let mut ed = reedline::Reedline::create();
        ed.run_edit_commands(&[reedline::EditCommand::InsertString(initial.to_string())]);
        match ed.read_line(&PlainPrompt(prompt.to_string())) {
            Ok(reedline::Signal::Success(s)) => Some(s),
            _ => None,
        }
    } else {
        eprint!("{prompt}");
        read_text(initial)
    }
}
