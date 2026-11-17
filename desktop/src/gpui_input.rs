use std::ops::Range;

use alacritty_terminal::term::TermMode;
use eframe::egui;
use gpui::KeyDownEvent;
use nosh_desktop::terminal::keyboard_input;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Surface {
    #[default]
    Terminal,
    Board,
    Connection,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseTarget {
    Tab(u64),
    Window,
}

pub struct ClosePrompt {
    pub target: CloseTarget,
    pub confirm_selected: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionButton {
    Cancel,
    SaveAndConnect,
    Connect,
}

impl ConnectionButton {
    pub fn action(self) -> Action {
        match self {
            Self::Cancel => Action::ReturnToTerminal,
            Self::SaveAndConnect => Action::SaveAndConnect,
            Self::Connect => Action::Connect,
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Write(Vec<u8>),
    Copy(String),
    CopyTerminal,
    Paste,
    NewLocal,
    NewConnection,
    ToggleConnections,
    SwitchTab(bool),
    RequestClose,
    ConfirmClose,
    CancelClose,
    Connect,
    SaveAndConnect,
    ReturnToTerminal,
    Zoom(i8),
}

#[derive(Default)]
pub struct LineEditor {
    pub text: String,
    pub cursor: usize,
    pub anchor: usize,
    pub marked: Option<Range<usize>>,
}

impl LineEditor {
    pub fn selection(&self) -> Range<usize> {
        self.anchor.min(self.cursor)..self.anchor.max(self.cursor)
    }

    pub fn utf16_selection(&self) -> Range<usize> {
        let selection = self.selection();
        self.text[..selection.start].encode_utf16().count()
            ..self.text[..selection.end].encode_utf16().count()
    }

    fn byte_offset(&self, utf16: usize) -> usize {
        let mut offset = 0;
        for (byte, ch) in self.text.char_indices() {
            if offset + ch.len_utf16() > utf16 {
                return byte;
            }
            offset += ch.len_utf16();
        }
        self.text.len()
    }

    pub fn byte_range(&self, utf16: Range<usize>) -> Range<usize> {
        let start = self.byte_offset(utf16.start);
        let end = self.byte_offset(utf16.end);
        start.min(end)..start.max(end)
    }

    pub fn replace(&mut self, range: Option<Range<usize>>, text: &str) -> Result<(), String> {
        let range = range
            .map(|range| self.byte_range(range))
            .or_else(|| self.marked.clone())
            .unwrap_or_else(|| self.selection());
        let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
        if self.text.len() - range.len() + text.len() > 4096 {
            return Err("Connection fields are limited to 4096 UTF-8 bytes.".into());
        }
        self.text.replace_range(range.clone(), &text);
        self.cursor = range.start + text.len();
        self.anchor = self.cursor;
        self.marked = None;
        Ok(())
    }

    pub fn mark(&mut self, range: Option<Range<usize>>, text: &str) -> Result<(), String> {
        self.replace(range, text)?;
        if !text.is_empty() {
            let len = text
                .chars()
                .filter(|ch| !ch.is_control())
                .map(char::len_utf8)
                .sum::<usize>();
            self.marked = Some(self.cursor - len..self.cursor);
        }
        Ok(())
    }

    pub fn cancel_mark(&mut self) {
        if let Some(range) = self.marked.take() {
            self.text.replace_range(range.clone(), "");
            self.cursor = range.start;
            self.anchor = self.cursor;
        }
    }

    pub fn move_cursor(&mut self, offset: usize, extend: bool) {
        self.cursor = offset.min(self.text.len());
        while !self.text.is_char_boundary(self.cursor) {
            self.cursor -= 1;
        }
        if !extend {
            self.anchor = self.cursor;
        }
        self.marked = None;
    }

    fn left(&self) -> usize {
        self.text[..self.cursor]
            .char_indices()
            .last()
            .map_or(0, |(offset, _)| offset)
    }

    fn right(&self) -> usize {
        self.cursor
            + self.text[self.cursor..]
                .chars()
                .next()
                .map_or(0, char::len_utf8)
    }

    fn delete(&mut self, backward: bool) {
        if self.selection().is_empty() {
            self.anchor = if backward { self.left() } else { self.right() };
        }
        let range = self.selection();
        self.text.replace_range(range.clone(), "");
        self.cursor = range.start;
        self.anchor = self.cursor;
        self.marked = None;
    }
}

#[derive(Default)]
pub struct InputState {
    pub surface: Surface,
    pub closing: Option<ClosePrompt>,
    pub fields: [LineEditor; 6],
    pub field: usize,
    pub form_button: Option<ConnectionButton>,
    pub terminal_composition: String,
}

impl InputState {
    pub fn terminal_enabled(&self) -> bool {
        self.closing.is_none() && self.surface == Surface::Terminal
    }

    pub fn field_enabled(&self) -> bool {
        self.connection_enabled() && self.form_button.is_none()
    }

    pub fn connection_enabled(&self) -> bool {
        self.closing.is_none() && self.surface == Surface::Connection
    }

    pub fn set_surface(&mut self, surface: Surface) {
        self.cancel_composition();
        self.surface = surface;
        self.form_button = None;
    }

    pub fn begin_close(&mut self, target: CloseTarget) {
        self.cancel_composition();
        self.closing = Some(ClosePrompt {
            target,
            confirm_selected: false,
        });
    }

    pub fn cancel_composition(&mut self) {
        self.terminal_composition.clear();
        for field in &mut self.fields {
            field.cancel_mark();
        }
    }

    pub fn commit(&mut self, range: Option<Range<usize>>, text: &str) -> Result<Action, String> {
        if self.field_enabled() {
            self.fields[self.field].replace(range, text)?;
        } else if self.terminal_enabled() {
            self.terminal_composition.clear();
            let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
            return Ok(if text.is_empty() {
                Action::None
            } else {
                Action::Write(text.into_bytes())
            });
        }
        Ok(Action::None)
    }

    pub fn paste(&mut self, text: &str, mode: TermMode) -> Result<Action, String> {
        if self.field_enabled() {
            self.fields[self.field].replace(None, text)?;
        } else if self.terminal_enabled() {
            return Ok(keyboard_input(
                &egui::Event::Paste(text.into()),
                mode,
                egui::Modifiers::NONE,
            )
            .map_or(Action::None, Action::Write));
        }
        Ok(Action::None)
    }

    pub fn mark(&mut self, range: Option<Range<usize>>, text: &str) -> Result<(), String> {
        if self.field_enabled() {
            self.fields[self.field].mark(range, text)?;
        } else if self.terminal_enabled() {
            self.terminal_composition = text.to_owned();
        }
        Ok(())
    }

    pub fn key(&mut self, event: &KeyDownEvent, mode: TermMode) -> Action {
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if let Some(prompt) = &mut self.closing {
            return match key {
                "escape" => Action::CancelClose,
                "enter" if prompt.confirm_selected => Action::ConfirmClose,
                "enter" => Action::CancelClose,
                "tab" | "left" | "right" => {
                    prompt.confirm_selected = !prompt.confirm_selected;
                    Action::None
                }
                _ => Action::None,
            };
        }
        if self.surface == Surface::Connection {
            if key == "escape" {
                return Action::ReturnToTerminal;
            }
            if key == "tab" {
                self.fields[self.field].marked = None;
                let order = [1, 0, 2, 3, 4, 5];
                let position = match self.form_button {
                    Some(ConnectionButton::Cancel) => 6,
                    Some(ConnectionButton::SaveAndConnect) => 7,
                    Some(ConnectionButton::Connect) => 8,
                    None => order
                        .iter()
                        .position(|&index| index == self.field)
                        .expect("connection field index"),
                };
                let position = (position + if m.shift { 8 } else { 1 }) % 9;
                self.form_button = match position {
                    6 => Some(ConnectionButton::Cancel),
                    7 => Some(ConnectionButton::SaveAndConnect),
                    8 => Some(ConnectionButton::Connect),
                    _ => {
                        self.field = order[position];
                        None
                    }
                };
                return Action::None;
            }
            if let Some(button) = self.form_button {
                return match key {
                    "enter" if m.control => Action::SaveAndConnect,
                    "enter" | "space" | " " => button.action(),
                    _ => Action::None,
                };
            }
            let editor = &mut self.fields[self.field];
            return match key {
                "enter" if editor.marked.is_none() => {
                    if m.control {
                        Action::SaveAndConnect
                    } else {
                        Action::Connect
                    }
                }
                "a" if m.control => {
                    editor.anchor = 0;
                    editor.cursor = editor.text.len();
                    Action::None
                }
                "c" | "x" if m.control && !editor.selection().is_empty() => {
                    let text = editor.text[editor.selection()].to_owned();
                    if key == "x" {
                        editor.delete(false);
                    }
                    Action::Copy(text)
                }
                "v" if m.control => Action::Paste,
                "backspace" => {
                    editor.delete(true);
                    Action::None
                }
                "delete" => {
                    editor.delete(false);
                    Action::None
                }
                "left" | "right" | "home" | "end" => {
                    let offset = match key {
                        "left" if !m.shift && !editor.selection().is_empty() => {
                            editor.selection().start
                        }
                        "right" if !m.shift && !editor.selection().is_empty() => {
                            editor.selection().end
                        }
                        "left" => editor.left(),
                        "right" => editor.right(),
                        "home" => 0,
                        _ => editor.text.len(),
                    };
                    editor.move_cursor(offset, m.shift);
                    Action::None
                }
                _ => Action::None,
            };
        }
        if m.control && m.shift {
            match key {
                "t" => return Action::NewLocal,
                "n" => return Action::NewConnection,
                "b" => return Action::ToggleConnections,
                "w" => return Action::RequestClose,
                "c" if self.terminal_enabled() => return Action::CopyTerminal,
                "v" if self.terminal_enabled() => return Action::Paste,
                _ => {}
            }
        }
        if m.control && key == "tab" {
            return Action::SwitchTab(m.shift);
        }
        if m.control && matches!(key, "+" | "=" | "-" | "0") {
            return Action::Zoom(match key {
                "-" => -1,
                "0" => 0,
                _ => 1,
            });
        }
        if self.terminal_enabled() {
            terminal_key(event, mode).map_or(Action::None, Action::Write)
        } else if key == "escape" {
            Action::ReturnToTerminal
        } else {
            Action::None
        }
    }
}

fn terminal_key(event: &KeyDownEvent, mode: TermMode) -> Option<Vec<u8>> {
    let m = event.keystroke.modifiers;
    let name = match event.keystroke.key.as_str() {
        "enter" => "Enter",
        "escape" => "Escape",
        "backspace" => "Backspace",
        "tab" => "Tab",
        "up" => "ArrowUp",
        "down" => "ArrowDown",
        "left" => "ArrowLeft",
        "right" => "ArrowRight",
        "home" => "Home",
        "end" => "End",
        "pageup" => "PageUp",
        "pagedown" => "PageDown",
        "delete" => "Delete",
        "insert" => "Insert",
        other => other,
    };
    let modifiers = egui::Modifiers {
        alt: m.alt,
        ctrl: m.control,
        shift: m.shift,
        mac_cmd: m.platform,
        command: m.platform || m.control,
    };
    if let Some(key) =
        egui::Key::from_name(name).or_else(|| egui::Key::from_name(&name.to_uppercase()))
        && (name.len() != 1 || m.control)
    {
        return keyboard_input(
            &egui::Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: event.is_held,
                modifiers,
            },
            mode,
            modifiers,
        );
    }
    if m.alt
        && !m.control
        && let Some(text) = &event.keystroke.key_char
    {
        return Some(format!("\x1b{text}").into_bytes());
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(value: &str) -> KeyDownEvent {
        KeyDownEvent {
            keystroke: gpui::Keystroke::parse(value).unwrap(),
            is_held: false,
        }
    }

    #[test]
    fn close_confirmation_blocks_every_input_path_and_defaults_to_keep() {
        let mut state = InputState::default();
        state.mark(None, "pending IME").unwrap();
        state.begin_close(CloseTarget::Tab(42));
        for value in [
            "ctrl-c",
            "ctrl-shift-v",
            "ctrl-shift-t",
            "ctrl-shift-n",
            "ctrl-shift-b",
            "a",
            "up",
        ] {
            assert_eq!(state.key(&key(value), TermMode::empty()), Action::None);
        }
        assert_eq!(
            state
                .paste("echo MUST_NOT_RUN\r", TermMode::BRACKETED_PASTE)
                .unwrap(),
            Action::None
        );
        assert_eq!(
            state.commit(None, "echo MUST_NOT_RUN\r").unwrap(),
            Action::None
        );
        state.mark(None, "new composition").unwrap();
        assert!(state.terminal_composition.is_empty());
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::CancelClose
        );
        assert_eq!(
            state.key(&key("escape"), TermMode::empty()),
            Action::CancelClose
        );
        assert_eq!(state.key(&key("tab"), TermMode::empty()), Action::None);
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::ConfirmClose
        );
        assert_eq!(
            state.key(&key("escape"), TermMode::empty()),
            Action::CancelClose
        );
        assert_eq!(state.closing.unwrap().target, CloseTarget::Tab(42));
    }

    #[test]
    fn connection_editor_never_falls_through_to_terminal_and_edits_unicode_ranges() {
        let mut state = InputState::default();
        state.set_surface(Surface::Connection);
        assert_eq!(state.commit(None, "a中z").unwrap(), Action::None);
        state.key(&key("left"), TermMode::empty());
        state.key(&key("backspace"), TermMode::empty());
        state.commit(None, "文").unwrap();
        assert_eq!(state.fields[0].text, "a文z");
        state.key(&key("home"), TermMode::empty());
        state.key(&key("delete"), TermMode::empty());
        assert_eq!(state.fields[0].text, "文z");
        for value in [
            "ctrl-c",
            "ctrl-shift-t",
            "ctrl-shift-n",
            "ctrl-shift-b",
            "ctrl-tab",
            "up",
            "f1",
        ] {
            assert!(!matches!(
                state.key(&key(value), TermMode::empty()),
                Action::Write(_)
            ));
        }
        state.field = 0;
        state.commit(Some(0..1), "😀").unwrap();
        assert_eq!(state.fields[0].text, "😀z");
        state.mark(Some(0..2), "候").unwrap();
        state.mark(None, "候補").unwrap();
        state.commit(None, "中").unwrap();
        assert_eq!(state.fields[0].text, "中z");
        assert!(state.fields[0].marked.is_none());
        assert_eq!(
            state.paste("X\r\n", TermMode::empty()).unwrap(),
            Action::None
        );
        assert_eq!(state.fields[0].text, "中Xz");
        state.field = 1;
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(
            state.field, 0,
            "Tab follows the visible host/name/user/port/key/jump order"
        );
        state.key(&key("shift-tab"), TermMode::empty());
        assert_eq!(state.field, 1);
        assert_eq!(state.key(&key("enter"), TermMode::empty()), Action::Connect);
        assert_eq!(
            state.key(&key("ctrl-enter"), TermMode::empty()),
            Action::SaveAndConnect
        );
        state.mark(None, "候").unwrap();
        assert_eq!(
            state.key(&key("ctrl-enter"), TermMode::empty()),
            Action::None
        );
    }

    #[test]
    fn editor_limit_preserves_old_selection_and_text() {
        let mut editor = LineEditor::default();
        editor.replace(None, "original").unwrap();
        editor.anchor = 0;
        assert!(editor.replace(None, &"x".repeat(4097)).is_err());
        assert_eq!(editor.text, "original");
        assert_eq!(editor.selection(), 0..8);
    }

    #[test]
    fn connection_focus_includes_buttons_and_blocks_text_until_a_field_is_focused() {
        let mut state = InputState::default();
        state.set_surface(Surface::Connection);
        state.field = 1;
        state.commit(None, "original-host").unwrap();
        for field in [0, 2, 3, 4, 5] {
            assert_eq!(state.key(&key("tab"), TermMode::empty()), Action::None);
            assert_eq!(state.field, field);
            assert!(state.field_enabled());
        }
        for (button, action) in [
            (ConnectionButton::Cancel, Action::ReturnToTerminal),
            (ConnectionButton::SaveAndConnect, Action::SaveAndConnect),
            (ConnectionButton::Connect, Action::Connect),
        ] {
            state.key(&key("tab"), TermMode::empty());
            assert_eq!(state.form_button, Some(button));
            assert!(!state.field_enabled());
            assert!(state.connection_enabled());
            assert_eq!(state.commit(None, "MUST_NOT_EDIT").unwrap(), Action::None);
            assert_eq!(
                state.paste("MUST_NOT_PASTE", TermMode::empty()).unwrap(),
                Action::None
            );
            state.mark(None, "MUST_NOT_COMPOSE").unwrap();
            assert_eq!(state.key(&key("space"), TermMode::empty()), action);
            assert_eq!(state.key(&key("enter"), TermMode::empty()), action);
            assert_eq!(
                state.key(&key("ctrl-enter"), TermMode::empty()),
                Action::SaveAndConnect
            );
        }
        assert!(state.fields[5].text.is_empty());
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(state.field, 1);
        assert!(state.field_enabled());
        assert_eq!(state.fields[1].text, "original-host");
        for button in [
            ConnectionButton::Connect,
            ConnectionButton::SaveAndConnect,
            ConnectionButton::Cancel,
        ] {
            state.key(&key("shift-tab"), TermMode::empty());
            assert_eq!(state.form_button, Some(button));
        }
        state.key(&key("shift-tab"), TermMode::empty());
        assert!(state.field_enabled());
        assert_eq!(state.field, 5);
        state.form_button = Some(ConnectionButton::Connect);
        state.begin_close(CloseTarget::Window);
        assert_eq!(state.key(&key("space"), TermMode::empty()), Action::None);
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::CancelClose
        );
    }

    #[test]
    fn terminal_keeps_interrupt_cursor_modes_bracketed_paste_and_native_text() {
        let mut state = InputState::default();
        assert_eq!(
            state.key(&key("ctrl-shift-n"), TermMode::empty()),
            Action::NewConnection
        );
        assert_eq!(
            state.key(&key("ctrl-shift-b"), TermMode::empty()),
            Action::ToggleConnections
        );
        assert_eq!(
            state.key(&key("ctrl-b"), TermMode::empty()),
            Action::Write(vec![2])
        );
        assert_eq!(
            state.key(&key("ctrl-tab"), TermMode::empty()),
            Action::SwitchTab(false)
        );
        assert_eq!(
            state.key(&key("ctrl-shift-tab"), TermMode::empty()),
            Action::SwitchTab(true)
        );
        assert_eq!(
            state.key(&key("ctrl-c"), TermMode::empty()),
            Action::Write(vec![3])
        );
        assert_eq!(
            state.key(&key("up"), TermMode::APP_CURSOR),
            Action::Write(b"\x1bOA".to_vec())
        );
        assert_eq!(
            state.key(&key("shift-up"), TermMode::empty()),
            Action::Write(b"\x1b[1;2A".to_vec())
        );
        assert_eq!(state.key(&key("a"), TermMode::empty()), Action::None);
        assert_eq!(
            state.commit(None, "中文").unwrap(),
            Action::Write("中文".as_bytes().to_vec())
        );
        state.begin_close(CloseTarget::Window);
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::CancelClose
        );
        state.closing = None;
        assert_eq!(state.commit(None, "\r\n\x1b").unwrap(), Action::None);
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::Write(b"\r".to_vec())
        );
        assert_eq!(
            state
                .paste("echo one\necho two", TermMode::BRACKETED_PASTE)
                .unwrap(),
            Action::Write(b"\x1b[200~echo one\necho two\x1b[201~".to_vec())
        );
        state.set_surface(Surface::Board);
        assert_eq!(state.commit(None, "hidden text").unwrap(), Action::None);
        assert_eq!(state.key(&key("enter"), TermMode::empty()), Action::None);
    }
}
