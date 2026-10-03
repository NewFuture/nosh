//! Startup configuration compiled into mode- and input-owner-specific bindings.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::num::NonZeroUsize;
use std::str::FromStr;
use std::sync::Arc;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
use reedline::{
    EditCommand, EditContext, EditMode, Emacs, EventStatus, Keybindings, PromptEditMode,
    PromptInteraction, PromptViMode, ReedlineEvent, ReedlineRawEvent, Vi,
};
use serde::Deserialize;

pub(crate) const COMPLETION_AI_COMMAND: &str = "__nosh_completion_ai__";
pub(crate) const FOCUS_NOTICE: &str = "__nosh_edit_focus__";
pub(crate) const VI_LIMIT_NOTICE: &str = "__nosh_vi_limit__";
pub(crate) const EDITOR_NOTICE: &str = "__nosh_editor_unavailable__";
pub(crate) const PLAIN_NOTICE: &str = "__nosh_plain_unavailable__";
pub const MAX_VI_REPETITIONS: usize = 1024;
pub const MAX_VI_SEQUENCE: usize = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Auto,
    Emacs,
    Vi,
}

impl FromStr for Mode {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "auto" => Ok(Self::Auto),
            "emacs" => Ok(Self::Emacs),
            "vi" => Ok(Self::Vi),
            _ => Err("shell.edit_mode: expected auto | emacs | vi".into()),
        }
    }
}

impl Mode {
    pub fn resolve(self, visual: Option<&str>, editor: Option<&str>) -> Self {
        match self {
            Self::Auto
                if [visual, editor]
                    .into_iter()
                    .flatten()
                    .any(|s| s.contains("vi")) =>
            {
                Self::Vi
            }
            Self::Auto => Self::Emacs,
            mode => mode,
        }
    }

    fn startup(self) -> Self {
        let visual = std::env::var_os("VISUAL");
        let editor = std::env::var_os("EDITOR");
        self.resolve(
            visual.as_ref().map(|s| s.to_string_lossy()).as_deref(),
            editor.as_ref().map(|s| s.to_string_lossy()).as_deref(),
        )
    }
}

pub type ActionKeys = BTreeMap<String, Vec<String>>;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ModeBindings {
    #[serde(default)]
    pub contexts: BTreeMap<String, ActionKeys>,
    #[serde(flatten)]
    pub actions: ActionKeys,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Bindings {
    #[serde(default)]
    pub modes: BTreeMap<String, ModeBindings>,
    #[serde(default)]
    pub contexts: BTreeMap<String, ActionKeys>,
    #[serde(flatten)]
    pub actions: ActionKeys,
}

#[derive(Debug, Clone, Default)]
pub struct Config {
    pub mode: Mode,
    pub keybindings: Bindings,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Capabilities {
    pub enhanced: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EditorMode {
    Emacs,
    ViInsert,
    ViNormal,
    ViVisual,
}

impl EditorMode {
    const ALL: [Self; 4] = [Self::Emacs, Self::ViInsert, Self::ViNormal, Self::ViVisual];

    fn parse(name: &str) -> Option<Self> {
        match name {
            "emacs" => Some(Self::Emacs),
            "vi_insert" => Some(Self::ViInsert),
            "vi_normal" => Some(Self::ViNormal),
            "vi_visual" => Some(Self::ViVisual),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Emacs => "emacs",
            Self::ViInsert => "vi_insert",
            Self::ViNormal => "vi_normal",
            Self::ViVisual => "vi_visual",
        }
    }

    fn current(mode: &PromptEditMode) -> Self {
        match mode {
            PromptEditMode::Vi(PromptViMode::Insert) => Self::ViInsert,
            PromptEditMode::Vi(PromptViMode::Normal) => Self::ViNormal,
            PromptEditMode::Vi(PromptViMode::Visual) => Self::ViVisual,
            _ => Self::Emacs,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Context {
    Editing,
    HistorySearch,
    Menu,
}

impl Context {
    const ALL: [Self; 3] = [Self::Editing, Self::HistorySearch, Self::Menu];

    fn parse(name: &str) -> Option<Self> {
        match name {
            "editing" => Some(Self::Editing),
            "history_search" => Some(Self::HistorySearch),
            "menu" => Some(Self::Menu),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Editing => "editing",
            Self::HistorySearch => "history_search",
            Self::Menu => "menu",
        }
    }

    fn from_editor(context: EditContext) -> Self {
        match context {
            EditContext::Editing => Self::Editing,
            EditContext::HistorySearch => Self::HistorySearch,
            EditContext::Menu => Self::Menu,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Action {
    CompleteOrAi,
    Complete,
    AiSuggest,
    HistorySearch,
    Accept,
    AcceptSearch,
    Cancel,
    InsertNewline,
    Undo,
    Redo,
    CutToStart,
    CutWordLeft,
    KillLine,
    DeleteWordLeft,
    PasteBefore,
    PasteAfter,
}

impl Action {
    fn parse(name: &str) -> Option<Self> {
        match name {
            "complete_or_ai" => Some(Self::CompleteOrAi),
            "complete" => Some(Self::Complete),
            "ai_suggest" => Some(Self::AiSuggest),
            "history_search" => Some(Self::HistorySearch),
            "accept" => Some(Self::Accept),
            "accept_search" => Some(Self::AcceptSearch),
            "cancel" => Some(Self::Cancel),
            "insert_newline" => Some(Self::InsertNewline),
            "undo" => Some(Self::Undo),
            "redo" => Some(Self::Redo),
            "cut_to_start" => Some(Self::CutToStart),
            "cut_word_left" => Some(Self::CutWordLeft),
            "kill_line" => Some(Self::KillLine),
            "delete_word_left" => Some(Self::DeleteWordLeft),
            "paste_before" => Some(Self::PasteBefore),
            "paste_after" => Some(Self::PasteAfter),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::CompleteOrAi => "complete_or_ai",
            Self::Complete => "complete",
            Self::AiSuggest => "ai_suggest",
            Self::HistorySearch => "history_search",
            Self::Accept => "accept",
            Self::AcceptSearch => "accept_search",
            Self::Cancel => "cancel",
            Self::InsertNewline => "insert_newline",
            Self::Undo => "undo",
            Self::Redo => "redo",
            Self::CutToStart => "cut_to_start",
            Self::CutWordLeft => "cut_word_left",
            Self::KillLine => "kill_line",
            Self::DeleteWordLeft => "delete_word_left",
            Self::PasteBefore => "paste_before",
            Self::PasteAfter => "paste_after",
        }
    }

    fn supported(self, context: Context) -> bool {
        match context {
            Context::HistorySearch => matches!(
                self,
                Self::HistorySearch | Self::Accept | Self::AcceptSearch | Self::Cancel
            ),
            Context::Editing | Context::Menu => self != Self::AcceptSearch,
        }
    }

    fn event(self, mode: EditorMode, context: Context, key: Key) -> ReedlineEvent {
        use ReedlineEvent as R;
        if matches!(self, Self::AiSuggest | Self::CompleteOrAi)
            && (context == Context::HistorySearch || mode == EditorMode::ViVisual)
        {
            return R::ExecuteHostCommand(FOCUS_NOTICE.into());
        }
        match self {
            Self::CompleteOrAi => R::CompleteOrHostCommand {
                menu: "completion_menu".into(),
                host_command: COMPLETION_AI_COMMAND.into(),
            },
            Self::Complete => R::UntilFound(vec![
                R::Menu("completion_menu".into()),
                R::MenuNext,
                R::Edit(vec![EditCommand::Complete]),
            ]),
            Self::AiSuggest if context == Context::Menu => {
                R::ExecuteHostCommand(FOCUS_NOTICE.into())
            }
            Self::AiSuggest => R::ExecuteHostCommand(crate::repl::SUGGEST_COMMAND.into()),
            Self::HistorySearch
                if mode == EditorMode::ViNormal
                    && context == Context::Editing
                    && key == Key::new(KeyModifiers::NONE, KeyCode::Char('?')) =>
            {
                R::Multiple(vec![
                    R::SwitchMode(PromptEditMode::Vi(PromptViMode::Insert)),
                    R::SearchHistory,
                ])
            }
            Self::HistorySearch => R::SearchHistory,
            Self::Accept if context == Context::Menu => R::Enter,
            Self::Accept
                if context == Context::Editing
                    && matches!(mode, EditorMode::ViNormal | EditorMode::ViVisual) =>
            {
                R::Multiple(vec![
                    R::SwitchMode(PromptEditMode::Vi(PromptViMode::Insert)),
                    R::Edit(vec![EditCommand::MoveRight { select: false }]),
                    R::Enter,
                ])
            }
            Self::Accept => R::Enter,
            Self::AcceptSearch => R::AcceptHistorySearch,
            Self::Cancel if context == Context::HistorySearch => R::CancelHistorySearch,
            Self::Cancel if context == Context::Menu && mode != EditorMode::ViVisual => R::Esc,
            Self::Cancel if mode == EditorMode::ViInsert => R::Multiple(vec![
                R::SwitchMode(PromptEditMode::Vi(PromptViMode::Normal)),
                R::Esc,
                R::Edit(vec![EditCommand::MoveLeft { select: false }]),
                R::Repaint,
            ]),
            Self::Cancel if mode == EditorMode::ViVisual => R::Multiple(vec![
                R::SwitchMode(PromptEditMode::Vi(PromptViMode::Normal)),
                R::Esc,
                R::Repaint,
            ]),
            Self::Cancel => R::Esc,
            Self::InsertNewline => R::Edit(vec![EditCommand::InsertNewline]),
            Self::Undo => R::Edit(vec![EditCommand::Undo]),
            Self::Redo => R::Edit(vec![EditCommand::Redo]),
            Self::CutToStart => R::Edit(vec![EditCommand::CutFromStart]),
            Self::CutWordLeft => R::Edit(vec![EditCommand::CutWordLeft]),
            Self::KillLine => R::Edit(vec![EditCommand::KillLine]),
            Self::DeleteWordLeft => R::Edit(vec![EditCommand::BackspaceWord]),
            Self::PasteBefore => R::Edit(vec![EditCommand::PasteCutBufferBefore]),
            Self::PasteAfter => R::Edit(vec![EditCommand::PasteCutBufferAfter]),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Key {
    modifiers: KeyModifiers,
    code: KeyCode,
}

impl Key {
    fn new(modifiers: KeyModifiers, code: KeyCode) -> Self {
        Self { modifiers, code }
    }

    fn normalized(mut self, caps: Capabilities) -> Self {
        if self.code == KeyCode::BackTab
            || (self.code == KeyCode::Tab && self.modifiers == KeyModifiers::SHIFT)
        {
            self.code = KeyCode::BackTab;
            self.modifiers = KeyModifiers::SHIFT;
        }
        // Match the mode parsers' shortcut identity without folding AltGr text.
        if !self.modifiers.is_empty()
            && !self
                .modifiers
                .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && let KeyCode::Char(c) = self.code
        {
            self.code = KeyCode::Char(c.to_ascii_lowercase());
        }
        // CSI-u reports Ctrl+_ using the unshifted minus key and a Shift flag.
        if self.modifiers == KeyModifiers::CONTROL | KeyModifiers::SHIFT
            && matches!(self.code, KeyCode::Char('-' | '_'))
        {
            self = Self::new(KeyModifiers::CONTROL, KeyCode::Char('_'));
        }
        if !caps.enhanced && self.modifiers == KeyModifiers::CONTROL {
            self = match self.code {
                KeyCode::Char('i') => Self::new(KeyModifiers::NONE, KeyCode::Tab),
                KeyCode::Char('m') => Self::new(KeyModifiers::NONE, KeyCode::Enter),
                KeyCode::Char('[') => Self::new(KeyModifiers::NONE, KeyCode::Esc),
                KeyCode::Char('_' | '/' | '7') => {
                    Self::new(KeyModifiers::CONTROL, KeyCode::Char('_'))
                }
                KeyCode::Char('@' | ' ') => Self::new(KeyModifiers::CONTROL, KeyCode::Char(' ')),
                _ => self,
            };
        }
        self
    }

    fn parse(text: &str, caps: Capabilities) -> Result<Self, String> {
        let mut parts = text.split('+').peekable();
        let mut modifiers = KeyModifiers::NONE;
        while parts.peek().is_some() {
            let part = parts.next().expect("peeked");
            if parts.peek().is_none() {
                let code = match part.to_ascii_lowercase().as_str() {
                    "enter" => KeyCode::Enter,
                    "tab" => KeyCode::Tab,
                    "backtab" => KeyCode::BackTab,
                    "esc" | "escape" => KeyCode::Esc,
                    "space" => KeyCode::Char(' '),
                    "plus" => KeyCode::Char('+'),
                    "backspace" => KeyCode::Backspace,
                    "delete" => KeyCode::Delete,
                    "left" => KeyCode::Left,
                    "right" => KeyCode::Right,
                    "up" => KeyCode::Up,
                    "down" => KeyCode::Down,
                    "home" => KeyCode::Home,
                    "end" => KeyCode::End,
                    "pageup" => KeyCode::PageUp,
                    "pagedown" => KeyCode::PageDown,
                    name if name.starts_with('f') && name.len() > 1 => {
                        let number = name[1..]
                            .parse::<u8>()
                            .ok()
                            .filter(|number| (1..=24).contains(number))
                            .ok_or_else(|| format!("invalid key {text:?}; expected F1..F24"))?;
                        KeyCode::F(number)
                    }
                    _ => {
                        let mut chars = part.chars();
                        let c = chars
                            .next()
                            .filter(|_| chars.next().is_none())
                            .filter(|c| !c.is_control())
                            .ok_or_else(|| format!("invalid key {text:?}"))?;
                        if c.is_ascii_uppercase() {
                            if !modifiers.contains(KeyModifiers::CONTROL) {
                                modifiers |= KeyModifiers::SHIFT;
                            }
                            KeyCode::Char(c.to_ascii_lowercase())
                        } else {
                            KeyCode::Char(c)
                        }
                    }
                };
                let key = Self::new(modifiers, code);
                if modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
                    && matches!(code, KeyCode::Char(_))
                {
                    return Err(format!("{text:?} is ambiguous with AltGr text input"));
                }
                let enhanced_only = (code == KeyCode::Tab
                    && modifiers.contains(KeyModifiers::CONTROL))
                    || (code == KeyCode::Enter
                        && modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::SHIFT))
                    || (matches!(code, KeyCode::Char('a'..='z'))
                        && modifiers.contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT));
                if enhanced_only && !caps.enhanced {
                    return Err(format!(
                        "{text:?} requires distinguishable enhanced keyboard events"
                    ));
                }
                return Ok(key.normalized(caps));
            }
            let modifier = match part.to_ascii_lowercase().as_str() {
                "ctrl" | "control" => KeyModifiers::CONTROL,
                "alt" => KeyModifiers::ALT,
                "shift" => KeyModifiers::SHIFT,
                _ => return Err(format!("invalid modifier in {text:?}")),
            };
            if modifiers.contains(modifier) {
                return Err(format!("duplicate modifier in {text:?}"));
            }
            modifiers |= modifier;
        }
        Err(format!("invalid key {text:?}"))
    }
}

#[derive(Clone)]
struct Binding {
    action: Option<Action>,
    event: ReedlineEvent,
    custom: bool,
    implicit: bool,
}

struct Map {
    keys: HashMap<Key, Binding>,
    native: Keybindings,
    hints: crate::status::Bindings,
    disabled_commands: Vec<char>,
}

type Maps = BTreeMap<(EditorMode, Context), Map>;

pub(crate) struct Compiled {
    mode: Mode,
    caps: Capabilities,
    maps: Maps,
}

fn action_of(event: &ReedlineEvent) -> Option<Action> {
    match event {
        ReedlineEvent::Enter => Some(Action::Accept),
        ReedlineEvent::Esc => Some(Action::Cancel),
        ReedlineEvent::SearchHistory => Some(Action::HistorySearch),
        ReedlineEvent::AcceptHistorySearch => Some(Action::AcceptSearch),
        ReedlineEvent::CancelHistorySearch => Some(Action::Cancel),
        ReedlineEvent::CompleteOrHostCommand { .. } => Some(Action::CompleteOrAi),
        ReedlineEvent::ExecuteHostCommand(name) if name == crate::repl::SUGGEST_COMMAND => {
            Some(Action::AiSuggest)
        }
        ReedlineEvent::Edit(commands) => match commands.as_slice() {
            [EditCommand::Undo] => Some(Action::Undo),
            [EditCommand::Redo] => Some(Action::Redo),
            [EditCommand::InsertNewline] => Some(Action::InsertNewline),
            [EditCommand::CutFromStart] => Some(Action::CutToStart),
            [EditCommand::CutWordLeft] => Some(Action::CutWordLeft),
            [EditCommand::KillLine] => Some(Action::KillLine),
            [EditCommand::BackspaceWord] => Some(Action::DeleteWordLeft),
            [EditCommand::PasteCutBufferBefore] => Some(Action::PasteBefore),
            [EditCommand::PasteCutBufferAfter] => Some(Action::PasteAfter),
            _ => None,
        },
        _ => None,
    }
}

fn preset(mode: EditorMode, context: Context, caps: Capabilities) -> HashMap<Key, Binding> {
    let native = match (mode, context) {
        (_, Context::HistorySearch) => Keybindings::new(),
        (EditorMode::Emacs, _) => reedline::default_emacs_keybindings(),
        (EditorMode::ViInsert, _) => reedline::default_vi_insert_keybindings(),
        (EditorMode::ViNormal, _) => reedline::default_vi_normal_keybindings(),
        (EditorMode::ViVisual, _) => reedline::default_vi_visual_keybindings(),
    };
    let mut keys: HashMap<_, _> = native
        .get_keybindings()
        .iter()
        .map(|(key, event)| {
            (
                Key::new(key.modifier, key.key_code).normalized(caps),
                Binding {
                    action: action_of(event),
                    event: event.clone(),
                    custom: false,
                    implicit: false,
                },
            )
        })
        .collect();
    let mut bind = |key: Key, action: Action, implicit: bool| {
        let key = key.normalized(caps);
        keys.insert(
            key,
            Binding {
                action: Some(action),
                event: action.event(mode, context, key),
                custom: false,
                implicit,
            },
        );
    };
    if context == Context::HistorySearch {
        bind(
            Key::new(KeyModifiers::CONTROL, KeyCode::Char('r')),
            Action::HistorySearch,
            false,
        );
        bind(
            Key::new(KeyModifiers::NONE, KeyCode::Enter),
            Action::Accept,
            false,
        );
        bind(
            Key::new(KeyModifiers::NONE, KeyCode::Esc),
            Action::AcceptSearch,
            false,
        );
        bind(
            Key::new(KeyModifiers::CONTROL, KeyCode::Char('j')),
            Action::AcceptSearch,
            false,
        );
        bind(
            Key::new(KeyModifiers::CONTROL, KeyCode::Char('g')),
            Action::Cancel,
            false,
        );
        for (key, event) in [
            (
                Key::new(KeyModifiers::NONE, KeyCode::Backspace),
                ReedlineEvent::Edit(vec![EditCommand::Backspace]),
            ),
            (
                Key::new(KeyModifiers::CONTROL, KeyCode::Char('h')),
                ReedlineEvent::Edit(vec![EditCommand::Backspace]),
            ),
            (
                Key::new(KeyModifiers::CONTROL, KeyCode::Char('d')),
                ReedlineEvent::CtrlD,
            ),
            (
                Key::new(KeyModifiers::CONTROL, KeyCode::Char('l')),
                ReedlineEvent::ClearScreen,
            ),
        ] {
            keys.insert(
                key,
                Binding {
                    action: None,
                    event,
                    custom: false,
                    implicit: false,
                },
            );
        }
    } else {
        bind(
            Key::new(KeyModifiers::NONE, KeyCode::Tab),
            Action::CompleteOrAi,
            false,
        );
        if caps.enhanced {
            bind(
                Key::new(KeyModifiers::CONTROL, KeyCode::Char('i')),
                Action::CompleteOrAi,
                false,
            );
            bind(
                Key::new(KeyModifiers::CONTROL, KeyCode::Char('m')),
                Action::Accept,
                false,
            );
            bind(
                Key::new(KeyModifiers::CONTROL, KeyCode::Char('[')),
                Action::Cancel,
                false,
            );
        }
        bind(
            Key::new(KeyModifiers::NONE, KeyCode::F(2)),
            Action::AiSuggest,
            false,
        );
        for key in [
            Key::new(KeyModifiers::CONTROL, KeyCode::Char('z')),
            Key::new(KeyModifiers::CONTROL, KeyCode::Char('_')),
        ] {
            bind(key, Action::Undo, false);
        }
        for key in [
            Key::new(KeyModifiers::CONTROL, KeyCode::Char('y')),
            Key::new(KeyModifiers::ALT, KeyCode::Char('/')),
        ] {
            bind(key, Action::Redo, false);
        }
        if matches!(mode, EditorMode::ViNormal | EditorMode::ViVisual) {
            for (key, action) in [
                (
                    Key::new(KeyModifiers::NONE, KeyCode::Char('p')),
                    Action::PasteAfter,
                ),
                (
                    Key::new(KeyModifiers::SHIFT, KeyCode::Char('p')),
                    Action::PasteBefore,
                ),
            ] {
                bind(key, action, true);
            }
            if mode == EditorMode::ViNormal {
                bind(
                    Key::new(KeyModifiers::NONE, KeyCode::Char('u')),
                    Action::Undo,
                    true,
                );
                bind(
                    Key::new(KeyModifiers::NONE, KeyCode::Char('?')),
                    Action::HistorySearch,
                    true,
                );
            }
        }
        if mode != EditorMode::Emacs {
            bind(
                Key::new(KeyModifiers::NONE, KeyCode::Enter),
                Action::Accept,
                true,
            );
        }
        if mode == EditorMode::ViNormal {
            keys.insert(
                Key::new(KeyModifiers::NONE, KeyCode::Char('v')),
                Binding {
                    action: None,
                    event: ReedlineEvent::SwitchMode(PromptEditMode::Vi(PromptViMode::Visual)),
                    custom: false,
                    implicit: true,
                },
            );
        }
        keys.insert(
            Key::new(KeyModifiers::SHIFT, KeyCode::BackTab),
            Binding {
                action: None,
                event: ReedlineEvent::MenuPrevious,
                custom: false,
                implicit: false,
            },
        );
        keys.insert(
            Key::new(KeyModifiers::CONTROL, KeyCode::Char('o')),
            Binding {
                action: None,
                event: ReedlineEvent::ExecuteHostCommand(EDITOR_NOTICE.into()),
                custom: false,
                implicit: false,
            },
        );
    }
    keys.insert(
        Key::new(KeyModifiers::CONTROL, KeyCode::Char('c')),
        Binding {
            action: None,
            event: ReedlineEvent::CtrlC,
            custom: false,
            implicit: false,
        },
    );
    keys
}

impl Config {
    pub(crate) fn needs_enhanced_keyboard(&self) -> bool {
        self.compile_maps(Capabilities { enhanced: false }, true)
            .is_err()
            && self
                .compile_maps(Capabilities { enhanced: true }, true)
                .is_ok()
    }

    pub fn validate(&self) -> Result<(), Vec<String>> {
        self.compile_maps(Capabilities { enhanced: true }, true)
            .map(|_| ())
    }

    pub(crate) fn compile(
        &self,
        caps: Capabilities,
        ai_enabled: bool,
    ) -> (Arc<Compiled>, Vec<String>) {
        match self.compile_maps(caps, ai_enabled) {
            Ok((maps, notices)) => (
                Arc::new(Compiled {
                    mode: self.mode.startup(),
                    caps,
                    maps,
                }),
                notices,
            ),
            Err(mut errors) => {
                errors.push("shell editing configuration rejected; using defaults".into());
                let (maps, _) = Self::default()
                    .compile_maps(caps, ai_enabled)
                    .expect("built-in bindings are valid");
                (
                    Arc::new(Compiled {
                        mode: Mode::Auto.startup(),
                        caps,
                        maps,
                    }),
                    errors,
                )
            }
        }
    }

    fn compile_maps(
        &self,
        caps: Capabilities,
        ai_enabled: bool,
    ) -> Result<(Maps, Vec<String>), Vec<String>> {
        let mut errors = Vec::new();
        let mut notices = Vec::new();
        for name in self.keybindings.modes.keys() {
            if EditorMode::parse(name).is_none() {
                errors.push(format!("shell.keybindings.modes.{name}: unknown mode"));
            }
        }
        for (name, contexts) in std::iter::once(("", &self.keybindings.contexts)).chain(
            self.keybindings
                .modes
                .iter()
                .map(|(name, mode)| (name.as_str(), &mode.contexts)),
        ) {
            for context in contexts.keys() {
                if Context::parse(context).is_none() {
                    errors.push(format!(
                        "shell.keybindings.{name}.contexts.{context}: unknown context"
                    ));
                }
            }
        }
        let mut maps = BTreeMap::new();
        for mode in EditorMode::ALL {
            let mode_spec = self.keybindings.modes.get(mode.name());
            for context in Context::ALL {
                let mut keys = preset(mode, context, caps);
                let original = keys.clone();
                let mut actions: BTreeMap<Action, Vec<(Key, String)>> = BTreeMap::new();
                let scopes = [
                    Some((&self.keybindings.actions, false, false)),
                    mode_spec.map(|spec| (&spec.actions, false, true)),
                    self.keybindings
                        .contexts
                        .get(context.name())
                        .map(|actions| (actions, true, false)),
                    mode_spec
                        .and_then(|spec| spec.contexts.get(context.name()))
                        .map(|actions| (actions, true, true)),
                ];
                for (scope, specific, explicit_mode) in scopes.into_iter().flatten() {
                    for (name, values) in scope {
                        let path = format!(
                            "shell.keybindings.{}.{}.{}",
                            mode.name(),
                            context.name(),
                            name
                        );
                        let Some(action) = Action::parse(name) else {
                            errors.push(format!("{path}: unknown action"));
                            continue;
                        };
                        if !action.supported(context) {
                            if specific {
                                errors.push(format!("{path}: action unavailable in this context"));
                            }
                            continue;
                        }
                        if (specific && context == Context::Menu && action == Action::AiSuggest)
                            || (explicit_mode
                                && mode == EditorMode::ViVisual
                                && matches!(
                                    action,
                                    Action::AiSuggest | Action::CompleteOrAi | Action::Complete
                                ))
                        {
                            errors.push(format!("{path}: action unavailable in this input owner"));
                            continue;
                        }
                        let mut parsed = Vec::new();
                        let mut seen = HashSet::new();
                        for value in values {
                            match Key::parse(value, caps) {
                                Ok(key) => {
                                    if key == Key::new(KeyModifiers::CONTROL, KeyCode::Char('c'))
                                        || (mode != EditorMode::Emacs
                                            && context != Context::HistorySearch
                                            && key == Key::new(KeyModifiers::NONE, KeyCode::Esc)
                                            && action != Action::Cancel)
                                    {
                                        errors.push(format!(
                                            "{path}: {value:?} is a protected cancellation key"
                                        ));
                                    } else if !seen.insert(key) {
                                        errors.push(format!(
                                            "{path}: duplicate or indistinguishable key {value:?}"
                                        ));
                                    } else {
                                        parsed.push((key, value.clone()));
                                    }
                                }
                                Err(error) => errors.push(format!("{path}: {error}")),
                            }
                        }
                        actions.insert(action, parsed);
                    }
                }
                keys.retain(|_, binding| {
                    binding
                        .action
                        .is_none_or(|action| !actions.contains_key(&action))
                });
                for (action, values) in actions {
                    for (key, label) in values {
                        if let Some(displaced) = keys.get(&key) {
                            if displaced.custom {
                                errors.push(format!(
                                    "shell.keybindings.{}.{}: {label:?} conflicts between {} and {}",
                                    mode.name(), context.name(),
                                    displaced.action.expect("custom action").name(), action.name(),
                                ));
                                continue;
                            }
                            let displaced = displaced
                                .action
                                .map(|action| action.name().to_owned())
                                .unwrap_or_else(|| format!("{:?}", displaced.event));
                            notices.push(format!(
                                "shell.keybindings.{}.{}: {label} replaces default {displaced}",
                                mode.name(),
                                context.name(),
                            ));
                        }
                        keys.insert(
                            key,
                            Binding {
                                action: Some(action),
                                event: action.event(mode, context, key),
                                custom: true,
                                implicit: false,
                            },
                        );
                    }
                }
                // Removed helper keys must not fall through to editing bindings.
                for (key, binding) in &original {
                    if binding.action.is_some() && !keys.contains_key(key) {
                        let emergency_escape = mode != EditorMode::Emacs
                            && context != Context::HistorySearch
                            && *key == Key::new(KeyModifiers::NONE, KeyCode::Esc);
                        keys.insert(
                            *key,
                            if emergency_escape {
                                binding.clone()
                            } else {
                                Binding {
                                    action: None,
                                    event: ReedlineEvent::None,
                                    custom: true,
                                    implicit: false,
                                }
                            },
                        );
                    }
                }
                let mut native = Keybindings::new();
                let mut hints = Keybindings::new();
                let mut disabled_commands = Vec::new();
                for (key, binding) in &original {
                    if binding.implicit && keys.get(key).is_none_or(|value| !value.implicit) {
                        if let KeyCode::Char(c) = key.code {
                            disabled_commands.push(if key.modifiers == KeyModifiers::SHIFT {
                                c.to_ascii_uppercase()
                            } else {
                                c
                            });
                        } else {
                            native.add_binding(key.modifiers, key.code, ReedlineEvent::None);
                        }
                    }
                }
                for (key, binding) in &keys {
                    if !binding.implicit {
                        native.add_binding(key.modifiers, key.code, binding.event.clone());
                    }
                    if ai_enabled || binding.action != Some(Action::AiSuggest) {
                        hints.add_binding(key.modifiers, key.code, binding.event.clone());
                    }
                }
                let mut hints = crate::status::Bindings::from_editor(&hints);
                hints.enable_correction(&native);
                maps.insert(
                    (mode, context),
                    Map {
                        keys,
                        native,
                        hints,
                        disabled_commands,
                    },
                );
            }
        }
        errors.sort();
        errors.dedup();
        notices.sort();
        notices.dedup();
        if errors.is_empty() {
            Ok((maps, notices))
        } else {
            Err(errors)
        }
    }
}

impl Compiled {
    fn map(&self, mode: EditorMode, context: Context) -> &Map {
        self.maps
            .get(&(mode, context))
            .expect("all mode/context maps compiled")
    }

    pub(crate) fn hints(
        &self,
        mode: &PromptEditMode,
        interaction: PromptInteraction<'_>,
    ) -> &crate::status::Bindings {
        let context = match interaction {
            PromptInteraction::Editing => Context::Editing,
            PromptInteraction::HistorySearch { .. } => Context::HistorySearch,
            PromptInteraction::Menu { .. } => Context::Menu,
        };
        &self.map(EditorMode::current(mode), context).hints
    }

    pub(crate) fn ai_key(&self, mode: &PromptEditMode) -> Option<&str> {
        self.hints(mode, PromptInteraction::Editing).suggest_key()
    }

    pub(crate) fn initial_mode(&self) -> PromptEditMode {
        if self.mode == Mode::Vi {
            PromptEditMode::Vi(PromptViMode::Insert)
        } else {
            PromptEditMode::Emacs
        }
    }

    pub(crate) fn plain_action(&self, key: &KeyEvent) -> Option<ReedlineEvent> {
        let mode = if self.mode == Mode::Vi {
            EditorMode::ViInsert
        } else {
            EditorMode::Emacs
        };
        let key = Key::new(key.modifiers, key.code).normalized(self.caps);
        self.map(mode, Context::Editing)
            .keys
            .get(&key)
            .and_then(|binding| match binding.action {
                Some(Action::AiSuggest | Action::CompleteOrAi) => Some(
                    ReedlineEvent::ExecuteHostCommand(crate::repl::SUGGEST_COMMAND.into()),
                ),
                Some(Action::Accept) => Some(ReedlineEvent::Enter),
                Some(Action::Cancel) => Some(ReedlineEvent::CtrlC),
                Some(Action::InsertNewline) => {
                    Some(ReedlineEvent::Edit(vec![EditCommand::InsertNewline]))
                }
                Some(Action::CutToStart) => Some(ReedlineEvent::Edit(vec![EditCommand::Clear])),
                Some(_) => Some(ReedlineEvent::ExecuteHostCommand(PLAIN_NOTICE.into())),
                None => None,
            })
    }

    pub(crate) fn editor(self: &Arc<Self>) -> Box<dyn EditMode> {
        let native: Box<dyn EditMode> = match self.mode {
            Mode::Emacs | Mode::Auto => Box::new(Emacs::new(
                self.map(EditorMode::Emacs, Context::Editing).native.clone(),
            )),
            Mode::Vi => Box::new(
                Vi::new(
                    self.map(EditorMode::ViInsert, Context::Editing)
                        .native
                        .clone(),
                    self.map(EditorMode::ViNormal, Context::Editing)
                        .native
                        .clone(),
                    self.map(EditorMode::ViVisual, Context::Editing)
                        .native
                        .clone(),
                )
                .with_input_limits(
                    NonZeroUsize::new(MAX_VI_REPETITIONS).expect("nonzero limit"),
                    NonZeroUsize::new(MAX_VI_SEQUENCE).expect("nonzero limit"),
                    ReedlineEvent::ExecuteHostCommand(VI_LIMIT_NOTICE.into()),
                )
                .with_disabled_commands(
                    PromptViMode::Normal,
                    self.map(EditorMode::ViNormal, Context::Editing)
                        .disabled_commands
                        .clone(),
                )
                .with_disabled_commands(
                    PromptViMode::Visual,
                    self.map(EditorMode::ViVisual, Context::Editing)
                        .disabled_commands
                        .clone(),
                ),
            ),
        };
        let search = EditorMode::ALL
            .into_iter()
            .map(|mode| {
                (
                    mode,
                    Emacs::new(self.map(mode, Context::HistorySearch).native.clone()),
                )
            })
            .collect();
        Box::new(ConfiguredEditMode {
            inner: native,
            compiled: self.clone(),
            search,
        })
    }
}

struct ConfiguredEditMode {
    inner: Box<dyn EditMode>,
    compiled: Arc<Compiled>,
    search: BTreeMap<EditorMode, Emacs>,
}

impl EditMode for ConfiguredEditMode {
    fn parse_event(&mut self, raw: ReedlineRawEvent) -> ReedlineEvent {
        self.parse_event_with_context(raw, EditContext::Editing)
    }

    fn parse_event_with_context(
        &mut self,
        raw: ReedlineRawEvent,
        context: EditContext,
    ) -> ReedlineEvent {
        let context = Context::from_editor(context);
        let mode = EditorMode::current(&self.inner.edit_mode());
        let mut raw: Event = raw.into();
        let map = self.compiled.map(mode, context);
        if let Event::Key(ref mut key) = raw {
            let normalized = Key::new(key.modifiers, key.code).normalized(self.compiled.caps);
            key.modifiers = normalized.modifiers;
            key.code = normalized.code;
            if let Some(binding) = map.keys.get(&normalized) {
                if matches!(
                    binding.action,
                    Some(Action::AiSuggest | Action::CompleteOrAi | Action::Complete)
                ) && (self.inner.has_pending_input() || mode == EditorMode::ViVisual)
                {
                    return ReedlineEvent::ExecuteHostCommand(FOCUS_NOTICE.into());
                }
                if context == Context::Menu && binding.action == Some(Action::AiSuggest) {
                    return ReedlineEvent::ExecuteHostCommand(FOCUS_NOTICE.into());
                }
                if context == Context::Menu && binding.custom {
                    let pending = self.inner.has_pending_input();
                    let motion_argument = matches!(normalized.code, KeyCode::Char(_))
                        && (normalized.modifiers.is_empty()
                            || normalized.modifiers == KeyModifiers::SHIFT);
                    if !pending || !motion_argument {
                        if pending && binding.event != ReedlineEvent::None {
                            let current = self.inner.edit_mode();
                            self.inner
                                .handle_mode_specific_event(ReedlineEvent::SwitchMode(current));
                        }
                        return binding.event.clone();
                    }
                }
            } else if context == Context::HistorySearch
                && self
                    .compiled
                    .map(mode, Context::Editing)
                    .keys
                    .get(&normalized)
                    .is_some_and(|binding| binding.action == Some(Action::AiSuggest))
            {
                return ReedlineEvent::ExecuteHostCommand(FOCUS_NOTICE.into());
            }
        }
        let raw = ReedlineRawEvent::try_from(raw).expect("normalized raw event");
        if context == Context::HistorySearch {
            self.search
                .get_mut(&mode)
                .expect("compiled search mode")
                .parse_event(raw)
        } else {
            self.inner.parse_event(raw)
        }
    }

    fn edit_mode(&self) -> PromptEditMode {
        self.inner.edit_mode()
    }

    fn has_pending_input(&self) -> bool {
        self.inner.has_pending_input()
    }

    fn handle_mode_specific_event(&mut self, event: ReedlineEvent) -> EventStatus {
        self.inner.handle_mode_specific_event(event)
    }

    fn after_event(&mut self, context: EditContext, edited: bool) {
        self.inner.after_event(context, edited);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> ReedlineRawEvent {
        ReedlineRawEvent::try_from(Event::Key(KeyEvent::new(code, modifiers))).unwrap()
    }

    #[test]
    fn auto_is_startup_selection_not_a_third_keymap() {
        for (visual, editor, expected) in [
            (None, None, Mode::Emacs),
            (Some("vim"), Some("emacs"), Mode::Vi),
            (None, Some("/usr/bin/nvim"), Mode::Vi),
            (Some("code"), Some("nano"), Mode::Emacs),
        ] {
            assert_eq!(Mode::Auto.resolve(visual, editor), expected);
            assert_eq!(Mode::Emacs.resolve(visual, editor), Mode::Emacs);
            assert_eq!(Mode::Vi.resolve(visual, editor), Mode::Vi);
        }
    }

    #[test]
    fn defaults_keep_native_keys_except_approved_overrides() {
        let cfg = Config {
            mode: Mode::Emacs,
            ..Default::default()
        };
        let (compiled, notices) = cfg.compile(Capabilities { enhanced: false }, true);
        assert!(notices.is_empty());
        let mut mode = compiled.editor();
        for (code, modifiers, expected) in [
            (KeyCode::Char('z'), KeyModifiers::CONTROL, EditCommand::Undo),
            (KeyCode::Char('7'), KeyModifiers::CONTROL, EditCommand::Undo),
            (KeyCode::Char('y'), KeyModifiers::CONTROL, EditCommand::Redo),
            (KeyCode::Char('/'), KeyModifiers::ALT, EditCommand::Redo),
            (KeyCode::Char('g'), KeyModifiers::CONTROL, EditCommand::Redo),
            (
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
                EditCommand::CutFromStart,
            ),
        ] {
            assert_eq!(
                mode.parse_event(key(code, modifiers)),
                ReedlineEvent::Edit(vec![expected])
            );
        }
        assert_eq!(
            mode.parse_event(key(KeyCode::F(2), KeyModifiers::NONE)),
            ReedlineEvent::ExecuteHostCommand(crate::repl::SUGGEST_COMMAND.into()),
        );
        assert_eq!(
            mode.parse_event_with_context(
                key(KeyCode::Char('g'), KeyModifiers::CONTROL),
                EditContext::HistorySearch
            ),
            ReedlineEvent::CancelHistorySearch,
        );
    }

    #[test]
    fn shifted_shortcuts_share_the_mode_parser_identity_and_focus_guards() {
        for mode in [Mode::Emacs, Mode::Vi] {
            let mut cfg = Config {
                mode,
                ..Default::default()
            };
            cfg.keybindings
                .actions
                .insert("ai_suggest".into(), vec!["X".into(), "Alt+G".into()]);
            let (compiled, notices) = cfg.compile(Capabilities { enhanced: false }, true);
            assert!(notices.is_empty(), "{notices:?}");
            let mut editor = compiled.editor();
            for (code, modifiers) in [
                (KeyCode::Char('X'), KeyModifiers::SHIFT),
                (KeyCode::Char('x'), KeyModifiers::SHIFT),
                (KeyCode::Char('G'), KeyModifiers::ALT | KeyModifiers::SHIFT),
                (KeyCode::Char('g'), KeyModifiers::ALT | KeyModifiers::SHIFT),
            ] {
                assert_eq!(
                    editor.parse_event(key(code, modifiers)),
                    ReedlineEvent::ExecuteHostCommand(crate::repl::SUGGEST_COMMAND.into()),
                );
                for context in [EditContext::Menu, EditContext::HistorySearch] {
                    assert_eq!(
                        editor.parse_event_with_context(key(code, modifiers), context),
                        ReedlineEvent::ExecuteHostCommand(FOCUS_NOTICE.into()),
                        "{mode:?} {context:?} {code:?}",
                    );
                }
            }
        }
    }

    #[test]
    fn menu_lists_replace_or_unbind_defaults_without_changing_editing_bindings() {
        for accept in [Vec::new(), vec!["F3".into()]] {
            let mut cfg = Config {
                mode: Mode::Emacs,
                ..Default::default()
            };
            cfg.keybindings.contexts.insert(
                "menu".into(),
                BTreeMap::from([
                    ("accept".into(), accept.clone()),
                    ("undo".into(), Vec::new()),
                ]),
            );
            let (compiled, notices) = cfg.compile(Capabilities { enhanced: false }, true);
            assert!(notices.is_empty(), "{notices:?}");
            let mut editor = compiled.editor();
            assert_eq!(
                editor.parse_event_with_context(
                    key(KeyCode::Enter, KeyModifiers::NONE),
                    EditContext::Menu,
                ),
                ReedlineEvent::None,
            );
            assert_eq!(
                editor.parse_event_with_context(
                    key(KeyCode::Char('z'), KeyModifiers::CONTROL),
                    EditContext::Menu,
                ),
                ReedlineEvent::None,
            );
            assert_eq!(
                editor.parse_event_with_context(
                    key(KeyCode::F(3), KeyModifiers::NONE),
                    EditContext::Menu,
                ),
                if accept.is_empty() {
                    ReedlineEvent::None
                } else {
                    ReedlineEvent::Enter
                },
            );
            assert_eq!(
                editor.parse_event(key(KeyCode::Enter, KeyModifiers::NONE)),
                ReedlineEvent::Enter
            );
            assert_eq!(
                editor.parse_event(key(KeyCode::Char('z'), KeyModifiers::CONTROL)),
                ReedlineEvent::Edit(vec![EditCommand::Undo]),
            );
        }
    }

    #[test]
    fn remapped_visual_cancel_returns_the_mode_machine_to_normal() {
        let mut cfg = Config {
            mode: Mode::Vi,
            ..Default::default()
        };
        cfg.keybindings.modes.insert(
            "vi_visual".into(),
            ModeBindings {
                actions: BTreeMap::from([("cancel".into(), vec!["F3".into()])]),
                ..Default::default()
            },
        );
        let (compiled, notices) = cfg.compile(Capabilities { enhanced: false }, true);
        assert!(notices.is_empty(), "{notices:?}");
        let mut editor = compiled.editor();
        for context in [EditContext::Editing, EditContext::Menu] {
            assert!(matches!(
                editor.handle_mode_specific_event(ReedlineEvent::SwitchMode(PromptEditMode::Vi(
                    PromptViMode::Visual
                ),)),
                EventStatus::Handled,
            ));
            let event =
                editor.parse_event_with_context(key(KeyCode::F(3), KeyModifiers::NONE), context);
            let ReedlineEvent::Multiple(events) = event else {
                panic!("visual cancellation must also switch the native mode machine");
            };
            assert!(events.contains(&ReedlineEvent::Esc));
            for event in events {
                if matches!(event, ReedlineEvent::SwitchMode(_)) {
                    assert!(matches!(
                        editor.handle_mode_specific_event(event),
                        EventStatus::Handled
                    ));
                }
            }
            assert_eq!(editor.edit_mode(), PromptEditMode::Vi(PromptViMode::Normal));
        }
    }

    #[test]
    fn menu_chord_overrides_do_not_steal_native_vi_motion_arguments() {
        let mut cfg = Config {
            mode: Mode::Vi,
            ..Default::default()
        };
        cfg.keybindings.contexts.insert(
            "menu".into(),
            BTreeMap::from([
                ("accept".into(), Vec::new()),
                ("undo".into(), Vec::new()),
                ("cancel".into(), vec!["F4".into()]),
            ]),
        );
        let (compiled, notices) = cfg.compile(Capabilities { enhanced: false }, true);
        assert!(notices.is_empty(), "{notices:?}");
        let mut editor = compiled.editor();
        editor.handle_mode_specific_event(ReedlineEvent::SwitchMode(PromptEditMode::Vi(
            PromptViMode::Normal,
        )));
        editor.parse_event_with_context(
            key(KeyCode::Char('f'), KeyModifiers::NONE),
            EditContext::Menu,
        );
        let argument = editor.parse_event_with_context(
            key(KeyCode::Char('u'), KeyModifiers::NONE),
            EditContext::Menu,
        );
        assert!(
            matches!(argument, ReedlineEvent::Multiple(events) if events.iter().any(|event|
                matches!(event, ReedlineEvent::Edit(commands) if commands.iter().any(|command|
                    matches!(command, EditCommand::Move(reedline::MotionTarget::Find { ch: 'u', .. }))
                ))
            ))
        );
        assert!(!editor.has_pending_input());
        editor.parse_event_with_context(
            key(KeyCode::Char('3'), KeyModifiers::NONE),
            EditContext::Menu,
        );
        assert_eq!(
            editor.parse_event_with_context(
                key(KeyCode::Enter, KeyModifiers::NONE),
                EditContext::Menu
            ),
            ReedlineEvent::None,
        );
        assert!(editor.has_pending_input());
        assert_eq!(
            editor.parse_event_with_context(
                key(KeyCode::F(4), KeyModifiers::NONE),
                EditContext::Menu
            ),
            ReedlineEvent::Esc,
        );
        assert!(!editor.has_pending_input());
    }

    #[test]
    fn enhanced_underscore_encodings_match_undo_and_conflict_as_the_same_key() {
        let caps = Capabilities { enhanced: true };
        let mut cfg = Config {
            mode: Mode::Emacs,
            ..Default::default()
        };
        cfg.keybindings
            .actions
            .insert("redo".into(), vec!["Ctrl+Shift+Z".into()]);
        let (compiled, notices) = cfg.compile(caps, true);
        assert!(notices.is_empty(), "{notices:?}");
        let mut editor = compiled.editor();
        for (code, modifiers) in [
            (
                KeyCode::Char('-'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (
                KeyCode::Char('_'),
                KeyModifiers::CONTROL | KeyModifiers::SHIFT,
            ),
            (KeyCode::Char('_'), KeyModifiers::CONTROL),
        ] {
            assert_eq!(
                editor.parse_event(key(code, modifiers)),
                ReedlineEvent::Edit(vec![EditCommand::Undo]),
            );
        }
        assert_eq!(
            Key::parse("Ctrl+_", caps).unwrap(),
            Key::parse("Ctrl+Shift+-", caps).unwrap(),
        );
        cfg.keybindings
            .actions
            .insert("undo".into(), vec!["Ctrl+_".into(), "Ctrl+Shift+-".into()]);
        assert!(
            cfg.validate().is_err(),
            "equivalent spellings must be diagnosed"
        );
    }

    #[test]
    fn shortcut_identity_does_not_change_unbound_text_or_altgr() {
        for mode in [Mode::Emacs, Mode::Vi] {
            let cfg = Config {
                mode,
                ..Default::default()
            };
            let (compiled, _) = cfg.compile(Capabilities { enhanced: true }, true);
            let mut editor = compiled.editor();
            for (c, modifiers) in [
                ('A', KeyModifiers::NONE),
                ('A', KeyModifiers::SHIFT),
                ('A', KeyModifiers::CONTROL | KeyModifiers::ALT),
                (
                    'A',
                    KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
                ),
                ('-', KeyModifiers::SHIFT),
                ('\u{20ac}', KeyModifiers::CONTROL | KeyModifiers::ALT),
                ('\u{4e2d}', KeyModifiers::SHIFT),
            ] {
                assert_eq!(
                    editor.parse_event(key(KeyCode::Char(c), modifiers)),
                    ReedlineEvent::Edit(vec![EditCommand::InsertChar(c)]),
                    "{mode:?} {c:?} {modifiers:?}",
                );
            }
        }
    }

    #[test]
    fn explicit_defaults_can_be_displaced_but_custom_collisions_reject_the_group() {
        let mut cfg = Config {
            mode: Mode::Emacs,
            ..Default::default()
        };
        cfg.keybindings
            .actions
            .insert("ai_suggest".into(), vec!["Ctrl+R".into()]);
        let (compiled, notices) = cfg.compile(Capabilities { enhanced: false }, true);
        assert!(
            notices
                .iter()
                .any(|s| s.contains("replaces default history_search"))
        );
        let mut mode = compiled.editor();
        assert_eq!(
            mode.parse_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            ReedlineEvent::ExecuteHostCommand(crate::repl::SUGGEST_COMMAND.into()),
        );
        cfg.keybindings
            .actions
            .insert("undo".into(), vec!["Ctrl+R".into()]);
        let (compiled, errors) = cfg.compile(Capabilities { enhanced: false }, true);
        assert!(errors.iter().any(|s| s.contains("conflicts between")));
        assert!(errors.iter().any(|s| s.contains("using defaults")));
        assert_eq!(
            compiled
                .editor()
                .parse_event(key(KeyCode::F(2), KeyModifiers::NONE)),
            ReedlineEvent::ExecuteHostCommand(crate::repl::SUGGEST_COMMAND.into()),
        );
    }

    #[test]
    fn alias_conflicts_and_enhanced_only_keys_are_not_silently_accepted() {
        let mut cfg = Config::default();
        cfg.keybindings
            .actions
            .insert("ai_suggest".into(), vec!["Tab".into()]);
        cfg.keybindings
            .actions
            .insert("undo".into(), vec!["Ctrl+I".into()]);
        assert!(
            cfg.compile_maps(Capabilities { enhanced: false }, true)
                .is_err()
        );
        assert!(
            cfg.compile_maps(Capabilities { enhanced: true }, true)
                .is_ok()
        );
        assert!(cfg.needs_enhanced_keyboard());
        cfg.keybindings.actions.clear();
        cfg.keybindings
            .actions
            .insert("redo".into(), vec!["Ctrl+Shift+Z".into()]);
        assert!(
            cfg.compile_maps(Capabilities { enhanced: false }, true)
                .is_err()
        );
        assert!(
            cfg.compile_maps(Capabilities { enhanced: true }, true)
                .is_ok()
        );
        for value in ["Ctrl+C", "Ctrl+Alt+E", "Ctrl+Ctrl+G", "Ctrl-G", "F25"] {
            cfg.keybindings.actions.clear();
            cfg.keybindings
                .actions
                .insert("ai_suggest".into(), vec![value.into()]);
            assert!(cfg.validate().is_err(), "{value}");
        }
    }

    #[test]
    fn unknown_names_and_unavailable_contexts_are_errors() {
        for (name, keys) in [
            ("unknown_action", vec!["F2"]),
            ("ai_suggest", vec!["F2", "F2"]),
        ] {
            let mut cfg = Config::default();
            cfg.keybindings
                .actions
                .insert(name.into(), keys.into_iter().map(str::to_owned).collect());
            assert!(cfg.validate().is_err(), "{name}");
        }
        let mut cfg = Config::default();
        cfg.keybindings
            .modes
            .insert("wrong".into(), ModeBindings::default());
        assert!(cfg.validate().is_err());
        cfg.keybindings.modes.clear();
        cfg.keybindings.contexts.clear();
        cfg.keybindings.contexts.insert(
            "menu".into(),
            BTreeMap::from([("ai_suggest".into(), vec!["F4".into()])]),
        );
        assert!(cfg.validate().is_err());
        cfg.keybindings.contexts.clear();
        cfg.keybindings.modes.insert(
            "vi_visual".into(),
            ModeBindings {
                actions: BTreeMap::from([("ai_suggest".into(), vec!["F4".into()])]),
                ..Default::default()
            },
        );
        assert!(cfg.validate().is_err());
        cfg.keybindings.modes.clear();
        cfg.keybindings
            .contexts
            .insert("wrong".into(), ActionKeys::default());
        assert!(cfg.validate().is_err());
        cfg.keybindings.contexts.clear();
        cfg.keybindings.contexts.insert(
            "history_search".into(),
            BTreeMap::from([("undo".into(), vec!["F4".into()])]),
        );
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn empty_lists_disable_native_vi_aliases_including_counted_forms() {
        let mut cfg = Config {
            mode: Mode::Vi,
            ..Default::default()
        };
        cfg.keybindings.modes.insert(
            "vi_normal".into(),
            ModeBindings {
                actions: BTreeMap::from([
                    ("undo".into(), Vec::new()),
                    ("paste_before".into(), Vec::new()),
                    ("paste_after".into(), Vec::new()),
                ]),
                ..Default::default()
            },
        );
        let (compiled, errors) = cfg.compile(Capabilities { enhanced: false }, true);
        assert!(errors.is_empty(), "{errors:?}");
        let mut mode = compiled.editor();
        mode.parse_event(key(KeyCode::Esc, KeyModifiers::NONE));
        for command in ["u", "3u", "p", "4p", "P"] {
            let mut result = ReedlineEvent::None;
            for c in command.chars() {
                let modifiers = if c.is_ascii_uppercase() {
                    KeyModifiers::SHIFT
                } else {
                    KeyModifiers::NONE
                };
                result = mode.parse_event(key(KeyCode::Char(c), modifiers));
            }
            assert_eq!(result, ReedlineEvent::None, "{command}");
        }
        assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Normal));
    }

    #[test]
    fn vi_limits_cover_products_cached_repeats_sequences_and_cancellation() {
        let cfg = Config {
            mode: Mode::Vi,
            ..Default::default()
        };
        let (compiled, _) = cfg.compile(Capabilities { enhanced: false }, true);
        let mut mode = compiled.editor();
        mode.parse_event(key(KeyCode::Esc, KeyModifiers::NONE));
        let mut sequence = |value: &str| {
            let mut result = ReedlineEvent::None;
            for c in value.chars() {
                result = mode.parse_event(key(KeyCode::Char(c), KeyModifiers::NONE));
            }
            result
        };
        assert!(
            matches!(sequence("1024x"), ReedlineEvent::Multiple(events) if events.len() == 1024)
        );
        for value in ["1025x", "33d33w", "2."] {
            assert_eq!(
                sequence(value),
                ReedlineEvent::ExecuteHostCommand(VI_LIMIT_NOTICE.into()),
                "{value}"
            );
        }
        for _ in 0..MAX_VI_SEQUENCE {
            assert_eq!(
                mode.parse_event(key(KeyCode::Char('9'), KeyModifiers::NONE)),
                ReedlineEvent::None
            );
        }
        assert_eq!(
            mode.parse_event(key(KeyCode::Char('9'), KeyModifiers::NONE)),
            ReedlineEvent::ExecuteHostCommand(VI_LIMIT_NOTICE.into()),
        );
        assert_eq!(
            mode.parse_event(key(KeyCode::Char('x'), KeyModifiers::NONE)),
            ReedlineEvent::None
        );
        assert!(!mode.has_pending_input());
        mode.parse_event(key(KeyCode::Char('f'), KeyModifiers::NONE));
        assert!(mode.has_pending_input());
        assert_eq!(
            mode.parse_event(key(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            ReedlineEvent::CtrlC
        );
        assert!(!mode.has_pending_input());
        mode.parse_event(key(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(
            mode.parse_event(key(KeyCode::F(2), KeyModifiers::NONE)),
            ReedlineEvent::ExecuteHostCommand(FOCUS_NOTICE.into()),
        );
        assert!(mode.has_pending_input());
        mode.parse_event(key(KeyCode::Esc, KeyModifiers::NONE));
        assert!(!mode.has_pending_input());
    }

    #[test]
    fn altgr_and_plain_digits_remain_text_and_vi_mode_switch_events_are_forwarded() {
        let cfg = Config {
            mode: Mode::Emacs,
            ..Default::default()
        };
        let (compiled, _) = cfg.compile(Capabilities { enhanced: false }, true);
        let mut mode = compiled.editor();
        for (c, modifiers) in [
            ('7', KeyModifiers::NONE),
            ('A', KeyModifiers::CONTROL | KeyModifiers::ALT),
            ('\u{20ac}', KeyModifiers::CONTROL | KeyModifiers::ALT),
        ] {
            assert_eq!(
                mode.parse_event(key(KeyCode::Char(c), modifiers)),
                ReedlineEvent::Edit(vec![EditCommand::InsertChar(c)])
            );
        }
        let cfg = Config {
            mode: Mode::Vi,
            ..Default::default()
        };
        let (compiled, _) = cfg.compile(Capabilities { enhanced: false }, true);
        let mut mode = compiled.editor();
        assert!(matches!(
            mode.handle_mode_specific_event(ReedlineEvent::SwitchMode(PromptEditMode::Vi(
                PromptViMode::Normal
            ))),
            EventStatus::Handled,
        ));
        assert_eq!(mode.edit_mode(), PromptEditMode::Vi(PromptViMode::Normal));
        assert_eq!(
            mode.parse_event(key(KeyCode::Char('r'), KeyModifiers::CONTROL)),
            ReedlineEvent::SearchHistory,
        );
    }
}
