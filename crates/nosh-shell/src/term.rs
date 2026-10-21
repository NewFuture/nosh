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
    #[cfg(unix)]
    let terminal = "/dev/tty";
    #[cfg(windows)]
    let terminal = "CONIN$";
    std::io::stderr().is_terminal()
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(terminal)
            .is_ok()
}

/// Query the actual output descriptor, not stdout or an unrelated `/dev/tty`.
pub fn stderr_columns() -> Option<usize> {
    columns(2)
}

pub fn stdout_columns() -> Option<usize> {
    columns(1)
}

#[cfg(unix)]
fn columns(fd: i32) -> Option<usize> {
    let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
    // SAFETY: ioctl writes a winsize into valid storage; it is read only on success.
    let result = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, size.as_mut_ptr()) };
    if result != 0 {
        return None;
    }
    // SAFETY: the successful ioctl initialized size.
    let columns = usize::from(unsafe { size.assume_init() }.ws_col);
    (columns > 0).then_some(columns)
}

#[cfg(windows)]
fn columns(fd: i32) -> Option<usize> {
    use windows_sys::Win32::System::Console::{
        CONSOLE_SCREEN_BUFFER_INFO, GetConsoleScreenBufferInfo, GetStdHandle, STD_ERROR_HANDLE,
        STD_OUTPUT_HANDLE,
    };
    let mut information = CONSOLE_SCREEN_BUFFER_INFO::default();
    // SAFETY: query the borrowed output handle into correctly sized storage.
    let result = unsafe {
        GetConsoleScreenBufferInfo(
            GetStdHandle(if fd == 2 {
                STD_ERROR_HANDLE
            } else {
                STD_OUTPUT_HANDLE
            }),
            &raw mut information,
        )
    };
    if result == 0 {
        return None;
    }
    usize::try_from(
        i32::from(information.srWindow.Right) - i32::from(information.srWindow.Left) + 1,
    )
    .ok()
    .filter(|columns| *columns > 0)
}

/// Discards pending typeahead so buffered keystrokes cannot answer a prompt.
#[cfg(unix)]
pub fn flush_input() -> io::Result<()> {
    use std::os::fd::AsRawFd;
    let tty = std::fs::OpenOptions::new().read(true).open("/dev/tty")?;
    crate::pty::flush_input()?;
    // SAFETY: valid open fd for the duration of the call.
    if unsafe { libc::tcflush(tty.as_raw_fd(), libc::TCIFLUSH) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(windows)]
pub fn flush_input() -> io::Result<()> {
    use std::os::windows::io::AsRawHandle;
    let input = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("CONIN$")?;
    // SAFETY: input owns a console input handle for this call.
    if unsafe {
        windows_sys::Win32::System::Console::FlushConsoleInputBuffer(input.as_raw_handle())
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
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
    text_result(
        read_short_line(prompt, initial, false, None, None, None).map(|(signal, ..)| signal),
    )
}

/// Selects a fixed choice with Up/Down or its number; Enter accepts the default.
pub fn read_choice(prompt: &str, choices: &[String], default: usize) -> io::Result<Option<usize>> {
    let initial = choices.get(default).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "default choice is out of range",
        )
    })?;
    let options = AnswerInput {
        choices,
        max_bytes: usize::MAX,
        cancelled: &|| false,
        selected: Some(default),
        choice_only: true,
    };
    match read_short_line(prompt, initial, false, None, None, Some(&options))? {
        (Signal::Success(_), _, selected) => Ok(selected),
        _ => Ok(None),
    }
}

/// Reads from the controlling terminal, never stdin. Up/Down selects a choice;
/// typing always permits a custom answer, including numeric text.
pub fn read_answer(
    prompt: &str,
    choices: &[String],
    max_bytes: usize,
    cancelled: &dyn Fn() -> bool,
) -> io::Result<Option<String>> {
    let options = AnswerInput {
        choices,
        max_bytes,
        cancelled,
        selected: None,
        choice_only: false,
    };
    match read_short_line(prompt, "", false, None, None, Some(&options))? {
        (Signal::Success(text), ..) => Ok(Some(text)),
        _ => Ok(None),
    }
}

struct AnswerInput<'a> {
    choices: &'a [String],
    max_bytes: usize,
    cancelled: &'a dyn Fn() -> bool,
    selected: Option<usize>,
    choice_only: bool,
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

pub(crate) fn read_plain_line(
    prompt: &str,
    draft: &mut String,
    assistance: Option<&crate::AssistDisplay>,
    editing: &crate::editing::Compiled,
) -> io::Result<Signal> {
    let (signal, buffer, _) =
        read_short_line(prompt, draft, true, assistance, Some(editing), None)?;
    *draft = buffer;
    Ok(signal)
}

fn read_short_line(
    prompt: &str,
    initial: &str,
    command: bool,
    assistance: Option<&crate::AssistDisplay>,
    editing: Option<&crate::editing::Compiled>,
    answer: Option<&AnswerInput<'_>>,
) -> io::Result<(Signal, String, Option<usize>)> {
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
    let choice_only = answer.is_some_and(|input| input.choice_only);
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
        choice_only,
    };
    line.start(&mut err)?;
    line.draw(&mut err, &buf)?;
    let mut shown = String::new();
    let mut selected = answer.and_then(|input| input.selected);
    let result = loop {
        if answer.is_some_and(|input| (input.cancelled)()) {
            break Signal::CtrlC;
        }
        if let Some(display) = assistance
            && buf.is_empty()
        {
            let status = display.status(editing.and_then(|maps| maps.ai_key(&maps.initial_mode())));
            if !status.is_empty() && status != shown {
                write!(err, "\r\n{status}\r\n")?;
                shown = status;
                line.previous_width = 0;
                line.draw(&mut err, &buf)?;
            }
        }
        if (assistance.is_some() || answer.is_some())
            && !event::poll(std::time::Duration::from_millis(100))?
        {
            continue;
        }
        match event::read()? {
            Event::Paste(_) if choice_only => {}
            Event::Paste(s) => {
                selected = None;
                if let Some(display) = assistance {
                    display.invalidate();
                }
                if answer.is_some() {
                    buf.push_str(&s);
                } else if command && editing.is_some() {
                    buf.push_str(&s.replace("\r\n", "\n").replace('\r', "\n"));
                } else {
                    buf.extend(s.chars().filter(|c| !c.is_control()));
                }
            }
            Event::Key(k) if k.kind != KeyEventKind::Release => {
                let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
                if let Some(action) = editing.and_then(|editing| editing.plain_action(&k)) {
                    match action {
                        reedline::ReedlineEvent::ExecuteHostCommand(command) => {
                            break Signal::HostCommand(command);
                        }
                        reedline::ReedlineEvent::Enter => break Signal::Success(buf.clone()),
                        reedline::ReedlineEvent::CtrlC => break Signal::CtrlC,
                        reedline::ReedlineEvent::Edit(commands)
                            if commands.as_slice() == [reedline::EditCommand::InsertNewline] =>
                        {
                            buf.push('\n');
                        }
                        reedline::ReedlineEvent::Edit(commands)
                            if commands.as_slice() == [reedline::EditCommand::Clear] =>
                        {
                            buf.clear();
                        }
                        _ => {
                            return Err(io::Error::other(
                                "unsupported compiled basic-input action",
                            ));
                        }
                    }
                    if let Some(display) = assistance {
                        display.invalidate();
                    }
                    line.draw(&mut err, &buf)?;
                    continue;
                }
                if let Some(display) = assistance {
                    display.invalidate();
                }
                match k.code {
                    KeyCode::Enter if editing.is_none() => break Signal::Success(buf.clone()),
                    KeyCode::Esc if editing.is_none() => break Signal::CtrlC,
                    KeyCode::Char('c') if ctrl => break Signal::CtrlC,
                    KeyCode::Char('d') if ctrl && (editing.is_none() || buf.is_empty()) => {
                        break Signal::CtrlD;
                    }
                    KeyCode::Char('u') if ctrl && editing.is_none() && !choice_only => buf.clear(),
                    KeyCode::Backspace if !choice_only => pop_grapheme(&mut buf),
                    KeyCode::Up | KeyCode::Down
                        if answer.is_some_and(|input| !input.choices.is_empty()) =>
                    {
                        let choices = answer.unwrap().choices;
                        let index = match (selected, k.code) {
                            (None, KeyCode::Up) => choices.len() - 1,
                            (None, _) => 0,
                            (Some(index), KeyCode::Up) => {
                                (index + choices.len() - 1) % choices.len()
                            }
                            (Some(index), _) => (index + 1) % choices.len(),
                        };
                        selected = Some(index);
                        buf.clone_from(&choices[index]);
                    }
                    KeyCode::Char(c) if !ctrl && choice_only => {
                        if let Some(index) = c.to_digit(10).and_then(|n| n.checked_sub(1))
                            && let Some(choice) = answer.unwrap().choices.get(index as usize)
                        {
                            selected = Some(index as usize);
                            buf.clone_from(choice);
                        }
                    }
                    KeyCode::Char(c) if !ctrl => {
                        selected = None;
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
        if answer.is_some_and(|input| buf.len() > input.max_bytes) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "answer exceeds input limit",
            ));
        }
        line.draw(&mut err, &buf)?;
    };
    write!(err, "\r\n")?;
    err.flush()?;
    Ok((result, buf, selected))
}

fn pop_grapheme(buf: &mut String) {
    if let Some((i, _)) = buf.grapheme_indices(true).next_back() {
        buf.truncate(i);
    }
}

fn input_window(buf: &str, columns: usize, command: bool, from_start: bool) -> String {
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
    if from_start {
        return style::clip_line(&text, columns, 0, &marker);
    }
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
    choice_only: bool,
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
        let text = input_window(buf, room, self.command, self.choice_only);
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
        text_result(
            read_short_line(prompt, initial, true, None, None, None).map(|(signal, ..)| signal),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_default_choices_are_rejected_before_reading_the_terminal() {
        for choices in [vec![], vec!["one".into()]] {
            let error = read_choice("", &choices, choices.len()).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

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
            let tail = input_window(&input, columns, false, false);
            assert!(style::width(&tail) <= columns);
            if tail.ends_with('\u{1f4bb}') {
                assert!(tail.ends_with("\u{1f469}\u{200d}\u{1f4bb}"));
            }
        }
        assert_eq!(input_window("a\nb\tc\x1b", 80, true, false), "a\\nb\\tc\\e");
        let choice = input_window(
            "minicpm5-2b:q4_k_m (MiniCPM5-2B Q4_K_M, 1.57 GB, Apache-2.0)",
            32,
            false,
            true,
        );
        assert!(choice.starts_with("minicpm5-2b:q4_k_m"), "{choice}");
        assert!(style::width(&choice) <= 32);
    }

    #[test]
    fn dumb_terminal_redraw_erases_the_old_input_without_ansi() {
        let mut line = InputLine {
            prompt: "> ".into(),
            columns: 20,
            previous_width: 0,
            ansi: false,
            command: false,
            choice_only: false,
        };
        let mut out = Vec::new();
        line.draw(&mut out, "\u{1f600}").unwrap();
        out.clear();
        line.draw(&mut out, "").unwrap();
        assert_eq!(String::from_utf8(out).unwrap(), "\r>   \r> ");
    }
}
