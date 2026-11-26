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
    Find,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditorTarget {
    Connection(usize),
    Find,
}

impl EditorTarget {
    pub fn slot(self) -> usize {
        match self {
            Self::Connection(index) => index,
            Self::Find => 6,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CloseTarget {
    Tab(u64),
    Window,
    Connection(usize),
}

pub struct ClosePrompt {
    pub target: CloseTarget,
    pub confirm_selected: bool,
}

pub const MAX_PASTE_BYTES: usize = 1024 * 1024;
const QUICK_PASTE_BYTES: usize = 1024;

pub struct PasteReview {
    pub target: u64,
    pub text: String,
    pub lines: usize,
    pub confirm_selected: bool,
}

impl PasteReview {
    pub fn new(target: u64, text: String) -> Result<Self, String> {
        if text.len() > MAX_PASTE_BYTES {
            return Err("Paste exceeds the 1 MiB limit. Nothing was sent.".into());
        }
        let mut lines = 1;
        let mut chars = text.chars().peekable();
        while let Some(ch) = chars.next() {
            if ch == '\r' {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                }
                lines += 1;
            } else if ch == '\n' {
                lines += 1;
            }
        }
        Ok(Self {
            target,
            text,
            lines,
            confirm_selected: false,
        })
    }

    pub fn needs_confirmation(&self) -> bool {
        needs_paste_confirmation(&self.text)
    }

    pub fn preview(&self) -> String {
        let mut preview = String::new();
        let mut consumed = 0;
        let mut lines = 1;
        for ch in self.text.chars().take(2000) {
            if lines > 10 {
                break;
            }
            consumed += ch.len_utf8();
            match ch {
                '\r' => preview.push_str("\\r"),
                '\n' => {
                    preview.push('\n');
                    lines += 1;
                }
                '\t' => preview.push_str("\\t"),
                ch if ch.is_control() => preview.extend(ch.escape_default()),
                ch => preview.push(ch),
            }
        }
        if consumed < self.text.len() {
            preview.push_str("\n[Preview clipped; confirmation sends the full clipboard text.]");
        }
        preview
    }

    pub fn allows_target(&self, id: u64, running: bool) -> bool {
        self.target == id && running
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConnectionButton {
    Cancel,
    Save,
    Connect,
    Advanced,
    Browse,
    Delete,
}

impl ConnectionButton {
    pub fn action(self) -> Action {
        match self {
            Self::Cancel => Action::ReturnToTerminal,
            Self::Save => Action::SaveConnection,
            Self::Connect => Action::Connect,
            Self::Advanced => Action::ToggleAdvanced,
            Self::Browse => Action::BrowseIdentity,
            Self::Delete => Action::DeleteConnection,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    None,
    Write(Vec<u8>),
    Copy(String),
    CopyTerminal,
    Paste,
    PasteText(String),
    ConfirmPaste,
    CancelPaste,
    OpenFind,
    CloseFind,
    MoveFind(bool),
    ActivateTab(u64),
    NewLocal,
    NewConnection,
    ToggleConnections,
    SwitchTab(bool),
    RequestClose,
    ConfirmClose,
    CancelClose,
    Connect,
    SaveAndConnect,
    SaveConnection,
    ToggleAdvanced,
    BrowseIdentity,
    DeleteConnection,
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
        self.utf16_range(self.selection())
    }

    pub fn utf16_range(&self, range: Range<usize>) -> Range<usize> {
        self.text[..range.start].encode_utf16().count()
            ..self.text[..range.end].encode_utf16().count()
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
            return Err("Text fields are limited to 4096 UTF-8 bytes.".into());
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

    fn key(&mut self, event: &KeyDownEvent) -> Action {
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        match key {
            "a" if m.control => {
                self.anchor = 0;
                self.cursor = self.text.len();
            }
            "c" | "x" if m.control && !self.selection().is_empty() => {
                let text = self.text[self.selection()].to_owned();
                if key == "x" {
                    self.delete(false);
                }
                return Action::Copy(text);
            }
            "v" if m.control => return Action::Paste,
            "backspace" => self.delete(true),
            "delete" => self.delete(false),
            "left" | "right" | "home" | "end" => {
                let offset = match key {
                    "left" if !m.shift && !self.selection().is_empty() => self.selection().start,
                    "right" if !m.shift && !self.selection().is_empty() => self.selection().end,
                    "left" => self.left(),
                    "right" => self.right(),
                    "home" => 0,
                    _ => self.text.len(),
                };
                self.move_cursor(offset, m.shift);
            }
            _ => {}
        }
        Action::None
    }
}

#[derive(Default)]
pub struct InputState {
    pub surface: Surface,
    pub closing: Option<ClosePrompt>,
    pub pasting: Option<PasteReview>,
    pub fields: [LineEditor; 6],
    pub field: usize,
    pub form_button: Option<ConnectionButton>,
    pub terminal_composition: String,
    pub advanced: bool,
    pub editing: bool,
    pub find: LineEditor,
    pub find_focus: usize,
}

impl InputState {
    pub fn terminal_enabled(&self) -> bool {
        self.closing.is_none() && self.pasting.is_none() && self.surface == Surface::Terminal
    }

    pub fn field_enabled(&self) -> bool {
        self.connection_enabled() && self.form_button.is_none()
    }

    pub fn connection_enabled(&self) -> bool {
        self.closing.is_none() && self.pasting.is_none() && self.surface == Surface::Connection
    }

    pub fn background_enabled(&self) -> bool {
        self.closing.is_none()
            && self.pasting.is_none()
            && matches!(
                self.surface,
                Surface::Terminal | Surface::Board | Surface::Find
            )
    }

    pub fn active_editor(&self) -> Option<EditorTarget> {
        if self.closing.is_some() || self.pasting.is_some() {
            None
        } else if self.surface == Surface::Find && self.find_focus == 0 {
            Some(EditorTarget::Find)
        } else if self.field_enabled() {
            Some(EditorTarget::Connection(self.field))
        } else {
            None
        }
    }

    pub fn editor(&self, target: EditorTarget) -> &LineEditor {
        match target {
            EditorTarget::Connection(index) => &self.fields[index],
            EditorTarget::Find => &self.find,
        }
    }

    pub fn editor_mut(&mut self, target: EditorTarget) -> &mut LineEditor {
        match target {
            EditorTarget::Connection(index) => &mut self.fields[index],
            EditorTarget::Find => &mut self.find,
        }
    }

    pub fn set_surface(&mut self, surface: Surface) {
        self.cancel_composition();
        self.surface = surface;
        self.form_button = None;
        self.find_focus = 0;
    }

    pub fn begin_close(&mut self, target: CloseTarget) {
        self.cancel_composition();
        self.pasting = None;
        self.closing = Some(ClosePrompt {
            target,
            confirm_selected: false,
        });
    }

    pub fn cancel_composition(&mut self) {
        self.terminal_composition.clear();
        self.find.cancel_mark();
        for field in &mut self.fields {
            field.cancel_mark();
        }
    }

    pub fn commit(&mut self, range: Option<Range<usize>>, text: &str) -> Result<Action, String> {
        if let Some(target) = self.active_editor() {
            self.editor_mut(target).replace(range, text)?;
        } else if self.terminal_enabled() {
            self.terminal_composition.clear();
            if needs_paste_confirmation(text) {
                if text.len() > MAX_PASTE_BYTES {
                    return Err("Paste exceeds the 1 MiB limit. Nothing was sent.".into());
                }
                return Ok(Action::PasteText(text.to_owned()));
            }
            let text: String = text.chars().filter(|ch| !ch.is_control()).collect();
            return Ok(if text.is_empty() {
                Action::None
            } else {
                Action::Write(text.into_bytes())
            });
        }
        Ok(Action::None)
    }

    pub fn paste(&mut self, text: &str) -> Result<Action, String> {
        if let Some(target) = self.active_editor() {
            self.editor_mut(target).replace(None, text)?;
        } else if self.terminal_enabled() {
            if text.len() > MAX_PASTE_BYTES {
                return Err("Paste exceeds the 1 MiB limit. Nothing was sent.".into());
            }
            return Ok(Action::PasteText(text.to_owned()));
        }
        Ok(Action::None)
    }

    pub fn mark(&mut self, range: Option<Range<usize>>, text: &str) -> Result<(), String> {
        if let Some(target) = self.active_editor() {
            self.editor_mut(target).mark(range, text)?;
        } else if self.terminal_enabled() {
            self.terminal_composition = text.to_owned();
        }
        Ok(())
    }

    pub fn key(&mut self, event: &KeyDownEvent, mode: TermMode) -> Action {
        let key = event.keystroke.key.as_str();
        let m = event.keystroke.modifiers;
        if let Some(prompt) = &mut self.closing {
            return confirmation_key(
                event,
                &mut prompt.confirm_selected,
                Action::ConfirmClose,
                Action::CancelClose,
            );
        }
        if let Some(prompt) = &mut self.pasting {
            return confirmation_key(
                event,
                &mut prompt.confirm_selected,
                Action::ConfirmPaste,
                Action::CancelPaste,
            );
        }
        if self.surface == Surface::Connection {
            if key == "escape" {
                return Action::ReturnToTerminal;
            }
            if key == "tab" {
                self.fields[self.field].marked = None;
                let mut order = vec![Ok(1), Ok(2), Ok(3), Ok(0), Err(ConnectionButton::Advanced)];
                if self.advanced {
                    order.extend([Ok(4), Err(ConnectionButton::Browse), Ok(5)]);
                }
                if self.editing {
                    order.push(Err(ConnectionButton::Delete));
                }
                order.extend([
                    Err(ConnectionButton::Cancel),
                    Err(ConnectionButton::Save),
                    Err(ConnectionButton::Connect),
                ]);
                let current = self.form_button.map_or(Ok(self.field), Err);
                let position = order.iter().position(|&item| item == current).unwrap_or(0);
                let next =
                    order[(position + if m.shift { order.len() - 1 } else { 1 }) % order.len()];
                self.form_button = next.err();
                if let Ok(field) = next {
                    self.field = field;
                }
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
            return if key == "enter" && editor.marked.is_none() {
                if m.control {
                    Action::SaveAndConnect
                } else {
                    Action::Connect
                }
            } else {
                editor.key(event)
            };
        }
        if self.surface == Surface::Find && self.find.marked.is_some() {
            if key == "escape" {
                self.find.cancel_mark();
            }
            return self.find.key(event);
        }
        if m.control && m.shift {
            match key {
                "t" => return Action::NewLocal,
                "n" => return Action::NewConnection,
                "f" => return Action::OpenFind,
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
        if self.surface == Surface::Find {
            return match key {
                "escape" => Action::CloseFind,
                "tab" => {
                    self.find.marked = None;
                    self.find_focus = (self.find_focus + if m.shift { 3 } else { 1 }) % 4;
                    Action::None
                }
                "enter" | "space" | " " if self.find_focus == 3 => Action::CloseFind,
                "enter" | "space" | " " if self.find_focus == 1 => Action::MoveFind(true),
                "enter" | "space" | " " if self.find_focus == 2 => Action::MoveFind(false),
                "enter" if self.find.marked.is_none() => Action::MoveFind(m.shift),
                _ if self.find_focus == 0 => self.find.key(event),
                _ => Action::None,
            };
        }
        if key == "insert" && m.shift && self.terminal_enabled() {
            return Action::Paste;
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

fn needs_paste_confirmation(text: &str) -> bool {
    text.len() > QUICK_PASTE_BYTES || text.chars().any(|ch| ch.is_control() && ch != '\t')
}

fn confirmation_key(
    event: &KeyDownEvent,
    selected: &mut bool,
    confirm: Action,
    cancel: Action,
) -> Action {
    let modifiers = event.keystroke.modifiers;
    match event.keystroke.key.as_str() {
        "escape" => cancel,
        "enter" if *selected => confirm,
        "enter" => cancel,
        "tab" | "left" | "right" if !modifiers.control && !modifiers.alt && !modifiers.platform => {
            *selected = !*selected;
            Action::None
        }
        _ => Action::None,
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
    fn terminal_paste_review_has_explicit_boundaries_and_stable_target() {
        let quick = PasteReview::new(42, "x".repeat(1024)).unwrap();
        assert!(!quick.needs_confirmation());
        let large = PasteReview::new(42, "x".repeat(1025)).unwrap();
        assert!(large.needs_confirmation());
        for text in [
            "one\n",
            "one\r",
            "one\r\ntwo",
            "one\ntwo",
            "\u{1b}[201~",
            "\u{03}",
        ] {
            let review = PasteReview::new(42, text.into()).unwrap();
            assert!(review.needs_confirmation(), "{text:?}");
            assert!(review.allows_target(42, true));
            assert!(!review.allows_target(43, true));
            assert!(!review.allows_target(42, false));
        }
        assert_eq!(PasteReview::new(42, "a\r\nb\rc\n".into()).unwrap().lines, 4);
        let review = PasteReview::new(42, "中".repeat(MAX_PASTE_BYTES / 3)).unwrap();
        assert!(review.preview().len() < 9000);
        assert!(PasteReview::new(42, "x".repeat(MAX_PASTE_BYTES)).is_ok());
        assert!(PasteReview::new(42, "x".repeat(MAX_PASTE_BYTES + 1)).is_err());
        assert!(
            PasteReview::new(42, "\x1bhello".into())
                .unwrap()
                .preview()
                .contains("\\u{1b}")
        );
    }

    #[test]
    fn pending_paste_is_cancel_by_default_and_blocks_all_other_inputs() {
        let mut state = InputState {
            pasting: Some(PasteReview::new(42, "echo one\necho two".into()).unwrap()),
            ..Default::default()
        };
        assert!(!state.background_enabled());
        assert!(!state.terminal_enabled());
        assert_eq!(state.active_editor(), None);
        for value in [
            "ctrl-tab",
            "ctrl-shift-v",
            "ctrl-shift-t",
            "ctrl-shift-f",
            "up",
            "ctrl-c",
        ] {
            assert_eq!(state.key(&key(value), TermMode::empty()), Action::None);
        }
        assert_eq!(state.commit(None, "MUST_NOT_RUN").unwrap(), Action::None);
        assert_eq!(state.paste("OTHER").unwrap(), Action::None);
        state.mark(None, "候補").unwrap();
        assert!(state.terminal_composition.is_empty());
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::CancelPaste
        );
        state.key(&key("right"), TermMode::empty());
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::ConfirmPaste
        );
        assert_eq!(
            state.key(&key("escape"), TermMode::empty()),
            Action::CancelPaste
        );
        state.begin_close(CloseTarget::Window);
        assert!(
            state.pasting.is_none(),
            "closing must discard rather than defer a paste"
        );
    }

    #[test]
    fn find_editor_navigation_and_ime_are_isolated_from_shell_and_other_editors() {
        let mut state = InputState::default();
        assert_eq!(
            state.key(&key("ctrl-shift-f"), TermMode::empty()),
            Action::OpenFind
        );
        state.set_surface(Surface::Find);
        assert!(state.background_enabled());
        state.commit(None, "中😀").unwrap();
        state.commit(Some(1..3), "文").unwrap();
        assert_eq!(state.find.text, "中文");
        assert_eq!(state.paste("needle").unwrap(), Action::None);
        assert_eq!(state.find.text, "中文needle");
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::MoveFind(false)
        );
        assert_eq!(
            state.key(&key("shift-enter"), TermMode::empty()),
            Action::MoveFind(true)
        );
        state.mark(None, "候").unwrap();
        assert_eq!(state.key(&key("enter"), TermMode::empty()), Action::None);
        assert_eq!(state.key(&key("ctrl-tab"), TermMode::empty()), Action::None);
        state.key(&key("escape"), TermMode::empty());
        assert_eq!(
            state.key(&key("ctrl-tab"), TermMode::empty()),
            Action::SwitchTab(false)
        );
        for expected in [
            Action::MoveFind(true),
            Action::MoveFind(false),
            Action::CloseFind,
        ] {
            state.key(&key("tab"), TermMode::empty());
            assert_eq!(state.commit(None, "MUST_NOT_EDIT").unwrap(), Action::None);
            assert_eq!(state.key(&key("enter"), TermMode::empty()), expected);
        }
        assert_eq!(
            state.key(&key("escape"), TermMode::empty()),
            Action::CloseFind
        );
        assert!(state.fields.iter().all(|field| field.text.is_empty()));
        state.set_surface(Surface::Terminal);
        assert_eq!(
            state.key(&key("shift-insert"), TermMode::empty()),
            Action::Paste
        );
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
        assert_eq!(state.paste("echo MUST_NOT_RUN\r").unwrap(), Action::None);
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
        state.form_button = None;
        state.commit(Some(0..1), "😀").unwrap();
        assert_eq!(state.fields[0].text, "😀z");
        state.mark(Some(0..2), "候").unwrap();
        state.mark(None, "候補").unwrap();
        state.commit(None, "中").unwrap();
        assert_eq!(state.fields[0].text, "中z");
        assert!(state.fields[0].marked.is_none());
        assert_eq!(state.paste("X\r\n").unwrap(), Action::None);
        assert_eq!(state.fields[0].text, "中Xz");
        state.field = 1;
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(
            state.field, 2,
            "Tab follows destination first, then profile and access"
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
        for field in [2, 3, 0] {
            assert_eq!(state.key(&key("tab"), TermMode::empty()), Action::None);
            assert_eq!(state.field, field);
            assert!(state.field_enabled());
        }
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(state.form_button, Some(ConnectionButton::Advanced));
        assert_eq!(
            state.key(&key("space"), TermMode::empty()),
            Action::ToggleAdvanced
        );
        state.advanced = true;
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(state.field, 4);
        assert!(state.field_enabled());
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(state.form_button, Some(ConnectionButton::Browse));
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::BrowseIdentity
        );
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(state.field, 5);
        assert!(state.field_enabled());
        for (button, action) in [
            (ConnectionButton::Cancel, Action::ReturnToTerminal),
            (ConnectionButton::Save, Action::SaveConnection),
            (ConnectionButton::Connect, Action::Connect),
        ] {
            state.key(&key("tab"), TermMode::empty());
            assert_eq!(state.form_button, Some(button));
            assert!(!state.field_enabled());
            assert!(state.connection_enabled());
            assert_eq!(state.commit(None, "MUST_NOT_EDIT").unwrap(), Action::None);
            assert_eq!(state.paste("MUST_NOT_PASTE").unwrap(), Action::None);
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
            ConnectionButton::Save,
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
        assert_eq!(
            state.commit(None, "\r\n\x1b").unwrap(),
            Action::PasteText("\r\n\x1b".into())
        );
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::Write(b"\r".to_vec())
        );
        assert_eq!(
            state.paste("echo one\necho two").unwrap(),
            Action::PasteText("echo one\necho two".into())
        );
        state.set_surface(Surface::Board);
        assert_eq!(state.commit(None, "hidden text").unwrap(), Action::None);
        assert_eq!(state.key(&key("enter"), TermMode::empty()), Action::None);
    }

    #[test]
    fn collapsed_connection_fields_are_skipped_and_save_is_network_independent() {
        let mut state = InputState::default();
        state.set_surface(Surface::Connection);
        state.field = 1;
        state.fields[4].replace(None, "existing-key").unwrap();
        for field in [2, 3, 0] {
            state.key(&key("tab"), TermMode::empty());
            assert_eq!(state.field, field);
        }
        for button in [
            ConnectionButton::Advanced,
            ConnectionButton::Cancel,
            ConnectionButton::Save,
        ] {
            state.key(&key("tab"), TermMode::empty());
            assert_eq!(state.form_button, Some(button));
        }
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::SaveConnection
        );
        assert_eq!(
            state.fields[4].text, "existing-key",
            "collapsing access options must not discard them"
        );
        state.editing = true;
        state.form_button = Some(ConnectionButton::Advanced);
        state.key(&key("tab"), TermMode::empty());
        assert_eq!(state.form_button, Some(ConnectionButton::Delete));
        state.begin_close(CloseTarget::Connection(3));
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::CancelClose
        );
        state.key(&key("right"), TermMode::empty());
        assert_eq!(
            state.key(&key("enter"), TermMode::empty()),
            Action::ConfirmClose
        );
    }
}
