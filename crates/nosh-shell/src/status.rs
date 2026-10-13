//! Pure, read-only prompt composition. Reedline owns all terminal I/O.

mod palette;
mod theme;
pub(crate) use palette::{ColorDepth, color_depth};
pub use palette::{ColorPair, InvalidColorIndex, Region, StatusPalette, StatusTone, Theme};
pub(crate) use theme::Subscription as ThemeSubscription;
pub use theme::ThemeHandle;

use std::collections::{BTreeMap, BTreeSet};

use nosh_platform::tr;
use reedline::{
    EditCommand, KeyCode, KeyModifiers, Keybindings, PromptContext, PromptInteraction,
    ReedlineEvent,
};
use unicode_segmentation::UnicodeSegmentation;

use crate::{Assistance, input_assist, style};

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub theme: ThemeHandle,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            theme: ThemeHandle::default(),
        }
    }
}

pub(crate) fn supported() -> bool {
    use std::io::IsTerminal;
    if !std::io::stdin().is_terminal()
        || !std::io::stdout().is_terminal()
        || !std::io::stderr().is_terminal()
        || !style::stdout().ansi
        || !crate::term::available()
    {
        return false;
    }
    let mut device = None;
    for fd in 0..=2 {
        let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
        // SAFETY: fstat initializes this storage on success.
        if unsafe { libc::fstat(fd, stat.as_mut_ptr()) } != 0 {
            return false;
        }
        let current = unsafe { stat.assume_init() }.st_rdev;
        if device.is_some_and(|previous| previous != current) {
            return false;
        }
        device = Some(current);
    }
    // SAFETY: these queries do not modify terminal or process state.
    unsafe { libc::tcgetpgrp(0) == libc::getpgrp() }
}

pub(crate) fn plain(text: &str) -> String {
    let mut text = style::visible_text(&style::strip_ansi(text))
        .replace('\n', "\\n")
        .replace('\t', " ");
    if text.len() > 1024 {
        let end = text
            .grapheme_indices(true)
            .take_while(|(i, g)| i + g.len() <= 1024)
            .map(|(i, g)| i + g.len())
            .last()
            .unwrap_or(0);
        text.truncate(end);
        text.push_str("...");
    }
    text
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Action {
    Complete,
    History,
    HistoryScope,
    Suggest,
    Cancel,
    Accept,
    Submit,
    AcceptSearch,
    RestoreSearch,
    Close,
    Next,
    Previous,
    Newline,
    Correct,
}

#[derive(Clone, Default)]
pub(crate) struct Bindings(BTreeMap<Action, String>);

impl Bindings {
    pub(crate) fn history_help(
        &self,
        has_match: bool,
        status: reedline::HistorySearchStatus<'_>,
        included: &BTreeSet<Part>,
    ) -> String {
        [
            (Action::HistoryScope, tr!("范围", "scope")),
            (
                Action::AcceptSearch,
                if status.no_matches {
                    tr!("退出", "close")
                } else {
                    tr!("回填", "edit")
                },
            ),
            (Action::Previous, tr!("更早", "older")),
            (Action::Next, tr!("更近", "newer")),
            (Action::RestoreSearch, tr!("取消", "cancel")),
        ]
        .into_iter()
        .filter(|(action, _)| {
            has_match
                || (*action == Action::AcceptSearch && status.no_matches)
                || *action != Action::AcceptSearch
        })
        .filter(|(action, _)| {
            status.can_navigate || !matches!(action, Action::Next | Action::Previous)
        })
        .filter(|(action, _)| !included.contains(&Part::Action(*action)))
        .filter_map(|(action, label)| self.0.get(&action).map(|key| format!("{key} {label}")))
        .collect::<Vec<_>>()
        .join("  ")
    }

    pub(crate) fn suggest_key(&self) -> Option<&str> {
        self.0.get(&Action::Suggest).map(String::as_str)
    }

    pub(crate) fn without_suggestions(mut self) -> Self {
        self.0.remove(&Action::Suggest);
        self
    }

    pub(crate) fn enable_correction(&mut self, bindings: &Keybindings) {
        if bindings.get_keybindings().iter().any(|(key, event)| {
            key.modifier.is_empty()
                && key.key_code == KeyCode::Right
                && input_assist::right_navigation(event)
        }) {
            self.0.insert(Action::Correct, "Right".into());
        }
    }

    pub(crate) fn from_editor(bindings: &Keybindings) -> Self {
        let mut keys: Vec<_> = bindings
            .get_keybindings()
            .iter()
            .filter_map(|(key, event)| {
                key_label(key.modifier, key.key_code).map(|label| (label, event))
            })
            .collect();
        keys.sort_by(|a, b| (a.0.len(), &a.0).cmp(&(b.0.len(), &b.0)));
        let mut result = Self::default();
        for (label, event) in keys {
            result.collect(event, &label);
        }
        result
    }

    fn collect(&mut self, event: &ReedlineEvent, key: &str) {
        let action = match event {
            ReedlineEvent::Menu(name) if name == "completion_menu" => Action::Complete,
            ReedlineEvent::SearchHistory => {
                self.0
                    .entry(Action::HistoryScope)
                    .or_insert_with(|| key.into());
                Action::History
            }
            ReedlineEvent::ExecuteHostCommand(name) if name == crate::repl::SUGGEST_COMMAND => {
                Action::Suggest
            }
            ReedlineEvent::CtrlC => Action::Cancel,
            ReedlineEvent::Enter => {
                self.0.entry(Action::Submit).or_insert_with(|| key.into());
                Action::Accept
            }
            ReedlineEvent::Submit => Action::Submit,
            ReedlineEvent::AcceptHistorySearch => Action::AcceptSearch,
            ReedlineEvent::CancelHistorySearch => Action::RestoreSearch,
            ReedlineEvent::CompleteOrHostCommand { .. } => Action::Complete,
            ReedlineEvent::Esc => Action::Close,
            ReedlineEvent::MenuNext => Action::Next,
            ReedlineEvent::MenuPrevious => Action::Previous,
            ReedlineEvent::Edit(commands)
                if commands.as_slice() == [EditCommand::InsertNewline] =>
            {
                Action::Newline
            }
            ReedlineEvent::UntilFound(events) | ReedlineEvent::Multiple(events) => {
                for event in events {
                    self.collect(event, key);
                }
                return;
            }
            _ => return,
        };
        self.0.entry(action).or_insert_with(|| key.into());
    }

    fn field(
        &self,
        action: Action,
        text: &str,
        priority: u8,
        required: bool,
        unicode: bool,
    ) -> Option<Field> {
        let key = self.0.get(&action)?;
        let key = if action == Action::Correct && unicode {
            "→"
        } else {
            key
        };
        let label = format!("{key} {text}");
        let compact = if matches!(action, Action::Cancel | Action::RestoreSearch)
            && key.starts_with("Ctrl+")
            && key.chars().count() == 6
        {
            format!("^{} {text}", key.chars().last().expect("control-key label"))
        } else if action == Action::Accept || (action == Action::Correct && !unicode) {
            format!("{key} {}", tr!("采用", "use"))
        } else if action == Action::Complete {
            format!("{key} {}", tr!("补全", "menu"))
        } else if action == Action::History {
            format!("{key} {}", tr!("历史", "find"))
        } else {
            label.clone()
        };
        let mut field = Field::new(2, &label, &compact, &compact, priority, required);
        field.part = Some(Part::Action(action));
        field.correction = action == Action::Correct;
        if matches!(action, Action::Cancel | Action::RestoreSearch) {
            field.tone = Tone::Secondary;
        }
        Some(field)
    }
}

fn key_label(modifiers: KeyModifiers, key: KeyCode) -> Option<String> {
    if !(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT).contains(modifiers) {
        return None;
    }
    let name = match key {
        KeyCode::Char(' ') => "Space".into(),
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
        KeyCode::Esc => "Esc".into(),
        KeyCode::F(number) => format!("F{number}"),
        KeyCode::Backspace => "Backspace".into(),
        KeyCode::Right => "Right".into(),
        KeyCode::Left => "Left".into(),
        KeyCode::Up => "Up".into(),
        KeyCode::Down => "Down".into(),
        KeyCode::Home => "Home".into(),
        KeyCode::End => "End".into(),
        _ => return None,
    };
    let mut label = String::new();
    for (modifier, prefix) in [
        (KeyModifiers::CONTROL, "Ctrl+"),
        (KeyModifiers::ALT, "Alt+"),
        (KeyModifiers::SHIFT, "Shift+"),
    ] {
        if modifiers.contains(modifier) {
            label.push_str(prefix);
        }
    }
    label.push_str(&name);
    Some(label)
}

#[derive(Debug, Clone)]
pub(crate) struct Completion {
    pub input: String,
    pub cursor: usize,
    pub error: Option<String>,
    pub count: usize,
    pub phase: crate::completion::Phase,
}

pub(crate) struct Context<'a> {
    pub editor: &'a PromptContext<'a>,
    pub environment: Option<(&'a str, Option<&'a str>)>,
    pub external_environment: Option<&'a crate::theme::Fragment>,
    pub feedback: Option<&'a input_assist::Feedback>,
    pub completion: Option<&'a Completion>,
    pub assistance: Option<&'a Assistance>,
    pub latest_command: Option<u64>,
    pub failed_exit: Option<i32>,
    pub last_result: Option<(i32, std::time::Duration)>,
    pub context_usage: Option<crate::repl::ContextUsage>,
    pub approval: &'a str,
    pub note: Option<&'a str>,
    pub bindings: &'a Bindings,
    pub color: bool,
    pub color_depth: ColorDepth,
    pub unicode: bool,
    pub theme: Theme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Part {
    HistoryTitle,
    HistoryMessage,
    HistoryDescription,
    Action(Action),
}

pub(crate) fn history_message(
    status: reedline::HistorySearchStatus<'_>,
    has_match: bool,
) -> Option<&str> {
    status.message().or_else(|| {
        if status.failed {
            Some(tr!("历史不可用", "History unavailable"))
        } else if !has_match {
            Some(tr!("没有匹配", "No matches"))
        } else {
            None
        }
    })
}

#[derive(Default)]
pub(crate) struct Layout {
    pub text: String,
    pub note_included: bool,
    pub correction_included: bool,
    pub included: BTreeSet<Part>,
}

struct Field {
    zone: u8,
    variants: [String; 3],
    priority: u8,
    required: bool,
    note: bool,
    correction: bool,
    part: Option<Part>,
    tone: Tone,
}

#[derive(Clone, Copy)]
enum Tone {
    Environment,
    Advice,
    Error,
    Pending,
    Notice,
    Action,
    Secondary,
}

impl Field {
    fn new(
        zone: u8,
        full: &str,
        compact: &str,
        minimum: &str,
        priority: u8,
        required: bool,
    ) -> Self {
        let full = plain(full);
        let compact = plain(compact);
        let minimum = plain(minimum);
        let compact = if !compact.is_empty() && style::width(&compact) < style::width(&full) {
            compact
        } else {
            full.clone()
        };
        let minimum = if !minimum.is_empty() && style::width(&minimum) < style::width(&compact) {
            minimum
        } else {
            compact.clone()
        };
        Self {
            zone,
            variants: [full, compact, minimum],
            priority,
            required,
            note: false,
            correction: false,
            part: None,
            tone: match zone {
                0 => Tone::Environment,
                2 => Tone::Action,
                _ => Tone::Secondary,
            },
        }
    }
}

fn path_summary(path: &str, columns: usize) -> String {
    let path = style::visible_text(&style::strip_ansi(path))
        .replace('\n', "\\n")
        .replace('\t', " ");
    if style::width(&path) <= columns {
        return path;
    }
    let prefix = if path.starts_with("~/") {
        "~/"
    } else if path.starts_with('/') {
        "/"
    } else {
        ""
    };
    let parts: Vec<_> = path
        .trim_start_matches("~/")
        .split('/')
        .filter(|part| !part.is_empty())
        .collect();
    for keep in [2, 1] {
        if parts.len() > keep {
            let summary = format!("{prefix}.../{}", parts[parts.len() - keep..].join("/"));
            if style::width(&summary) <= columns {
                return summary;
            }
        }
    }
    if let Some(leaf) = parts.last().filter(|_| parts.len() > 1) {
        let marked = format!(".../{leaf}");
        if style::width(&marked) <= columns {
            return marked;
        }
    }
    let prefix = format!("{prefix}{}", if parts.len() > 1 { ".../" } else { "" });
    if style::width(&prefix) >= columns {
        return style::clip_line("...", columns, 0, "");
    }
    format!(
        "{prefix}{}",
        style::clip_line(
            parts.last().copied().unwrap_or(&path),
            columns.saturating_sub(style::width(&prefix)),
            0,
            "..."
        )
    )
}

pub(crate) fn compose(context: Context<'_>) -> Layout {
    let editor = context.editor;
    if editor.rows < 3 {
        return Layout::default();
    }
    let mut fields = Vec::new();
    let add_action = |fields: &mut Vec<Field>, action, label: &str, priority, required| {
        if let Some(field) =
            context
                .bindings
                .field(action, label, priority, required, context.unicode)
        {
            fields.push(field);
        }
    };
    let state = |fields: &mut Vec<Field>, full: &str, compact: &str, tone: Tone| {
        let icon = match tone {
            Tone::Advice => {
                if context.unicode {
                    "› "
                } else {
                    "> "
                }
            }
            Tone::Error => "! ",
            Tone::Notice => "? ",
            _ => "",
        };
        let minimum = if matches!(tone, Tone::Advice)
            && let Some(proposal) = context
                .feedback
                .and_then(|feedback| feedback.correction.as_ref())
        {
            format!(
                "{}{}{}",
                proposal.from,
                if context.unicode { "→" } else { "->" },
                proposal.to
            )
        } else {
            compact.to_owned()
        };
        let mut field = Field::new(
            1,
            &format!("{icon}{full}"),
            &format!("{icon}{compact}"),
            &minimum,
            100,
            true,
        );
        field.tone = tone;
        fields.push(field);
    };
    match editor.interaction {
        PromptInteraction::HistorySearch {
            term,
            has_match,
            status,
        } => {
            if let Some(status) = status {
                let tone = if status.failed {
                    Tone::Error
                } else if status.notice.is_some() {
                    Tone::Notice
                } else if status.progress.is_some() {
                    Tone::Pending
                } else if !has_match {
                    Tone::Notice
                } else {
                    Tone::Secondary
                };
                let mut title = Field::new(1, status.title, status.title, status.title, 101, true);
                title.part = Some(Part::HistoryTitle);
                title.tone = tone;
                fields.push(title);
                if let Some(message) = history_message(status, has_match) {
                    let compact = style::clip_line(&plain(message), 32, 0, "...");
                    let minimum = if status.failed {
                        tr!("历史不可用", "History unavailable").to_owned()
                    } else if status.notice.is_none() && status.progress.is_some() {
                        tr!("查询中", "Searching").to_owned()
                    } else {
                        style::clip_line(&plain(message), 20, 0, "...")
                    };
                    let mut field = Field::new(1, message, &compact, &minimum, 100, true);
                    field.part = Some(Part::HistoryMessage);
                    field.tone = tone;
                    fields.push(field);
                }
                if let Some(description) = status.description {
                    let mut field = Field::new(1, description, description, description, 60, false);
                    field.part = Some(Part::HistoryDescription);
                    fields.push(field);
                }
            } else {
                let label = if has_match {
                    tr!("历史搜索", "History search")
                } else {
                    tr!("历史无匹配", "No history match")
                };
                state(
                    &mut fields,
                    &format!("{label}: {}", plain(term)),
                    if has_match {
                        tr!("历史", "History")
                    } else {
                        tr!("未匹配", "No match")
                    },
                    if has_match {
                        Tone::Secondary
                    } else {
                        Tone::Notice
                    },
                );
            }
            if has_match && status.is_none() {
                add_action(&mut fields, Action::Submit, tr!("提交", "submit"), 98, true);
            }
            if status.is_some() {
                add_action(
                    &mut fields,
                    Action::HistoryScope,
                    tr!("范围", "scope"),
                    96,
                    true,
                );
            }
            if context.bindings.0.contains_key(&Action::AcceptSearch)
                || context.bindings.0.contains_key(&Action::RestoreSearch)
            {
                if has_match || status.is_some_and(|status| status.no_matches) {
                    add_action(
                        &mut fields,
                        Action::AcceptSearch,
                        if has_match {
                            tr!("回填", "edit")
                        } else {
                            tr!("退出", "close")
                        },
                        99,
                        true,
                    );
                }
                add_action(
                    &mut fields,
                    Action::RestoreSearch,
                    tr!("取消", "cancel"),
                    97,
                    true,
                );
            } else {
                add_action(&mut fields, Action::Close, tr!("退出", "exit"), 99, true);
            }
            if status.is_none() {
                add_action(
                    &mut fields,
                    Action::History,
                    tr!("继续搜索", "search again"),
                    85,
                    false,
                );
            } else if status.is_some_and(|status| status.can_navigate) {
                add_action(
                    &mut fields,
                    Action::Previous,
                    tr!("更早", "older"),
                    85,
                    false,
                );
                add_action(&mut fields, Action::Next, tr!("更近", "newer"), 84, false);
            }
        }
        PromptInteraction::Menu {
            count, provisional, ..
        } => {
            let limited = context
                .completion
                .is_some_and(|completion| completion.phase == crate::completion::Phase::Partial);
            let error = context
                .completion
                .and_then(|completion| completion.error.as_deref());
            let label = if error.is_some() {
                tr!("补全失败", "Completion failed")
            } else if limited {
                tr!("部分补全结果", "Partial completions")
            } else if provisional {
                tr!("补全查询中", "Completion pending")
            } else if count == 0 {
                tr!("无补全候选", "No completions")
            } else {
                tr!("补全", "Completion")
            };
            state(
                &mut fields,
                &error.map_or_else(|| label.into(), |error| format!("{label}: {error}")),
                if error.is_some() {
                    tr!("失败", "Failed")
                } else if limited {
                    tr!("部分", "Partial")
                } else if provisional {
                    tr!("查询中", "Pending")
                } else if count == 0 {
                    tr!("无候选", "No match")
                } else {
                    tr!("补全", "Menu")
                },
                if error.is_some() {
                    Tone::Error
                } else if provisional && !limited {
                    Tone::Pending
                } else if count == 0 && !provisional {
                    Tone::Notice
                } else {
                    Tone::Secondary
                },
            );
            if count > 0 {
                let count = format!("{count}{}", if provisional { "+" } else { "" });
                fields.push(Field::new(1, &count, &count, &count, 90, false));
            }
            if count > 0 && (!provisional || limited) && error.is_none() {
                add_action(&mut fields, Action::Accept, tr!("采用", "accept"), 98, true);
            }
            add_action(&mut fields, Action::Close, tr!("关闭", "close"), 99, true);
            if count > 1 && (!provisional || limited) && error.is_none() {
                add_action(&mut fields, Action::Next, tr!("下一个", "next"), 85, false);
                add_action(
                    &mut fields,
                    Action::Previous,
                    tr!("上一个", "previous"),
                    84,
                    false,
                );
            }
        }
        PromptInteraction::Editing => {
            let completion = context.completion.filter(|completion| {
                completion.input == editor.buffer && completion.cursor == editor.completion_cursor
            });
            let blank = editor.buffer.trim().is_empty();
            let correction = context
                .feedback
                .and_then(|feedback| feedback.correction.as_ref())
                .filter(|proposal| {
                    context.bindings.0.contains_key(&Action::Correct)
                        && editor.cursor == editor.buffer.len()
                        && editor.selection.is_none()
                        && editor.buffer.get(proposal.edit_range.clone())
                            == Some(proposal.from.as_str())
                });
            let candidate = context.assistance.and_then(|result| match result {
                Assistance::Command {
                    command_id,
                    intent,
                    program,
                } if blank && Some(*command_id) == context.latest_command => {
                    Some((intent, program))
                }
                _ => None,
            });
            if let Some(feedback) = context.feedback {
                let tone = if feedback.correction.is_some() {
                    Tone::Advice
                } else {
                    match feedback.state {
                        input_assist::State::Error => Tone::Error,
                        input_assist::State::Pending => Tone::Pending,
                        input_assist::State::Unavailable
                        | input_assist::State::Unknown
                        | input_assist::State::Incomplete => Tone::Notice,
                        _ => Tone::Secondary,
                    }
                };
                state(&mut fields, &feedback.text, &feedback.compact, tone);
            } else if let Some(error) =
                completion.and_then(|completion| completion.error.as_deref())
            {
                state(
                    &mut fields,
                    &format!("{}: {error}", tr!("补全失败", "Completion failed")),
                    tr!("补全失败", "Completion failed"),
                    Tone::Error,
                );
            } else if completion.is_some_and(|completion| {
                completion.count == 0 && completion.phase == crate::completion::Phase::Complete
            }) {
                state(
                    &mut fields,
                    tr!("无补全候选", "No completions"),
                    tr!("无补全候选", "No completions"),
                    Tone::Notice,
                );
            } else if let Some((intent, program)) = candidate {
                state(
                    &mut fields,
                    &format!("{intent}: {program}"),
                    tr!("有建议", "Advice"),
                    Tone::Advice,
                );
            } else if let Some(Assistance::Pending { command_id, intent }) =
                context.assistance.filter(|_| blank)
                && context.latest_command == Some(*command_id)
            {
                state(
                    &mut fields,
                    &format!(
                        "AI {intent}: {}",
                        tr!("生成建议中", "generating suggestion")
                    ),
                    &format!("AI {intent}"),
                    Tone::Pending,
                );
            } else if let Some(Assistance::Message(message)) = context.assistance.filter(|_| blank)
            {
                state(
                    &mut fields,
                    message,
                    &style::clip_line(&plain(message), 28, 0, "..."),
                    Tone::Secondary,
                );
            } else if blank
                && context.last_result.is_none()
                && context.external_environment.is_none()
                && let Some(exit) = context.failed_exit
            {
                let text = format!("{}: {exit}", tr!("上次失败", "Last failure"));
                state(&mut fields, &text, &text, Tone::Notice);
            } else if !blank && editor.buffer.contains('\n') {
                let text = format!(
                    "{} {}",
                    tr!("多行", "Multiline"),
                    editor.buffer.lines().count()
                );
                fields.push(Field::new(1, &text, &text, &text, 90, false));
            }
            let has_state = fields.iter().any(|field| field.zone == 1);
            let needs_cancel = has_state
                && candidate.is_none()
                && context
                    .feedback
                    .is_none_or(|feedback| feedback.state != input_assist::State::Known);
            if needs_cancel {
                add_action(
                    &mut fields,
                    Action::Cancel,
                    tr!("取消", "cancel"),
                    95,
                    needs_cancel,
                );
            }
            if correction.is_some() {
                add_action(
                    &mut fields,
                    Action::Correct,
                    tr!("采用", "accept"),
                    98,
                    true,
                );
            } else if candidate.is_some() {
                add_action(
                    &mut fields,
                    Action::Suggest,
                    tr!("采用", "accept"),
                    97,
                    true,
                );
            } else if blank {
                add_action(
                    &mut fields,
                    Action::History,
                    tr!("历史", "history"),
                    55,
                    false,
                );
            } else {
                add_action(
                    &mut fields,
                    Action::Complete,
                    tr!("补全", "complete"),
                    55,
                    false,
                );
            }
            if (!blank && editor.buffer.contains('\n'))
                || context
                    .feedback
                    .is_some_and(|feedback| feedback.state == input_assist::State::Incomplete)
            {
                add_action(
                    &mut fields,
                    Action::Newline,
                    tr!("换行", "newline"),
                    85,
                    false,
                );
            }
        }
    }
    if let Some(note) = context.note.filter(|note| !note.is_empty()) {
        let compact = style::clip_line(&plain(note), 24, 0, "...");
        let mut field = Field::new(1, note, &compact, &compact, 96, false);
        field.note = true;
        fields.push(field);
    }
    if !matches!(
        editor.edit_mode,
        reedline::PromptEditMode::Emacs | reedline::PromptEditMode::Default
    ) {
        let edit_mode = editor.edit_mode.to_string();
        fields.push(Field::new(3, &edit_mode, &edit_mode, &edit_mode, 10, false));
    }
    if !context.approval.is_empty() {
        let compact = context
            .approval
            .strip_prefix("Approval: ")
            .or_else(|| context.approval.strip_prefix("审批: "))
            .unwrap_or(context.approval);
        fields.push(Field::new(3, compact, compact, compact, 20, false));
    }
    if context.external_environment.is_none()
        && let Some((exit, duration)) = context.last_result
        && (exit != 0 || duration >= std::time::Duration::from_secs(2))
    {
        let elapsed = if duration.as_secs() >= 60 {
            format!(
                "{}m{:02}s",
                duration.as_secs() / 60,
                duration.as_secs() % 60
            )
        } else {
            format!("{:.1}s", duration.as_secs_f64())
        };
        let compact = if exit == 0 {
            elapsed.clone()
        } else {
            format!("exit {exit}")
        };
        let full = if exit == 0 {
            format!("{} {elapsed}", tr!("上次用时", "last"))
        } else {
            format!("{} {exit}", tr!("上次退出", "last exit"))
        };
        let full = if exit != 0 && duration >= std::time::Duration::from_secs(2) {
            format!("{full} {elapsed}")
        } else {
            full
        };
        let mut field = Field::new(4, &full, &compact, &compact, 70, false);
        field.tone = if exit == 0 {
            Tone::Pending
        } else {
            Tone::Error
        };
        fields.push(field);
    }
    if let Some(usage) = context.context_usage.filter(|usage| usage.max > 0) {
        let percent = (usage.used as u128 * 100) / usage.max as u128;
        let compact = format!("ctx {percent}%");
        let mut field = Field::new(
            5,
            &format!("ctx {}/{}", usage.used, usage.max),
            &compact,
            &compact,
            30,
            false,
        );
        if percent >= 85 {
            field.tone = Tone::Notice;
        }
        fields.push(field);
    }
    render(&fields, &context)
}

fn minimum_width(fields: &[Field], number: u8) -> usize {
    let mut required = fields.iter().filter(|field| {
        field.zone == number
            && (field.required
                || (number == 1 && !field.note && field.part != Some(Part::HistoryDescription)))
    });
    if let Some(first) = required.next() {
        return style::width(&first.variants[2])
            + required
                .map(|field| 1 + style::width(&field.variants[2]))
                .sum::<usize>();
    }
    fields
        .iter()
        .filter(|field| field.zone == number)
        .max_by_key(|field| field.priority)
        .map_or(0, |field| style::width(&field.variants[2]))
}

fn allocate(
    columns: usize,
    minimum: [usize; 6],
    preferred: [usize; 6],
    powerline: bool,
    native_environment: bool,
) -> [usize; 6] {
    let mut widths = [0; 6];
    let mut remaining = columns;
    let local_segment = |number| matches!(number, 1 | 4) || (number == 0 && !native_environment);
    for number in [1, 2, 4, 0, 5, 3] {
        let has_left = [0, 1, 4].iter().any(|&index| widths[index] > 0);
        let gap = if widths.iter().any(|&width| width > 0)
            && !(matches!(number, 0 | 1 | 4) && has_left)
        {
            2
        } else {
            0
        };
        let shared_cap = usize::from(
            powerline
                && local_segment(number)
                && [0, 1, 4]
                    .iter()
                    .any(|&index| widths[index] > 0 && local_segment(index)),
        );
        let available = remaining.saturating_sub(gap) + shared_cap;
        if minimum[number] == 0
            || available == 0
            || (matches!(number, 0 | 3 | 4 | 5) && available < minimum[number])
        {
            continue;
        }
        widths[number] = minimum[number].min(available);
        remaining = available - widths[number];
    }
    for number in [2, 1, 4, 0, 5, 3] {
        if widths[number] > 0 {
            let extra = preferred[number]
                .saturating_sub(widths[number])
                .min(remaining);
            widths[number] += extra;
            remaining -= extra;
        }
    }
    widths
}

struct Zone {
    text: String,
    tone: Tone,
    note_included: bool,
    correction_included: bool,
    state_complete: bool,
    included: BTreeSet<Part>,
}

fn zone(
    fields: &[Field],
    number: u8,
    columns: usize,
    allow_correction: bool,
    inset: usize,
) -> Zone {
    let budget = columns.saturating_sub(inset);
    let mut order: Vec<_> = fields
        .iter()
        .filter(|field| {
            field.zone == number
                && !field.variants[0].is_empty()
                && (allow_correction || !field.correction)
        })
        .collect();
    order.sort_by_key(|field| {
        (
            std::cmp::Reverse(field.required),
            std::cmp::Reverse(field.priority),
        )
    });
    let mut selected: Vec<(&Field, String)> = Vec::new();
    let mut used = 0;
    let mut state_complete = true;
    for field in order {
        let text = &field.variants[2];
        let separator = usize::from(!selected.is_empty());
        let width = style::width(text);
        if used + separator + width <= budget {
            selected.push((field, text.clone()));
            used += separator + width;
        } else if selected.is_empty() && !matches!(number, 2 | 3 | 5) && budget > 0 {
            let clipped = style::clip_line(text, budget, 0, "...");
            used = style::width(&clipped);
            selected.push((field, clipped));
            state_complete = !field.required;
        }
    }
    // Keep the primary action before secondary cancellation hints.
    if number == 2 {
        selected.sort_by_key(|(field, _)| {
            (
                matches!(field.tone, Tone::Secondary),
                std::cmp::Reverse(field.priority),
            )
        });
    }
    for (field, text) in &mut selected {
        for next in [1, 0] {
            let width = style::width(&field.variants[next]);
            if used - style::width(text) + width <= budget {
                used = used - style::width(text) + width;
                text.clone_from(&field.variants[next]);
            }
        }
    }
    Zone {
        tone: selected
            .first()
            .map_or(Tone::Secondary, |(field, _)| field.tone),
        note_included: selected
            .iter()
            .any(|(field, text)| field.note && *text == field.variants[0]),
        correction_included: selected
            .iter()
            .any(|(field, text)| field.correction && field.variants.contains(text)),
        included: selected
            .iter()
            .filter_map(|(field, text)| {
                let part = field.part?;
                let complete = match part {
                    Part::Action(_) => field.variants.contains(text),
                    _ => *text == field.variants[0],
                };
                complete.then_some(part)
            })
            .collect(),
        text: selected
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join(" "),
        state_complete,
    }
}

fn segment_inset(count: usize, powerline: bool) -> usize {
    2 * count + if powerline && count > 0 { count + 1 } else { 0 }
}

fn environment_parts(context: &Context<'_>, columns: usize) -> Vec<(String, ColorPair)> {
    let Some((cwd, branch)) = context.environment else {
        return Vec::new();
    };
    let theme = context.theme;
    let inset = segment_inset(1, theme.powerline);
    if columns <= inset {
        return Vec::new();
    }
    let branch = branch
        .filter(|branch| !branch.is_empty())
        .map(|branch| format!("({})", style::clip_line(&plain(branch), 28, 0, "...")));
    let extra = segment_inset(2, theme.powerline) - inset;
    let branch = branch.filter(|branch| {
        style::width(branch) + extra + inset + style::width(&path_summary(cwd, 12)) <= columns
    });
    let path_budget = columns
        - inset
        - branch
            .as_ref()
            .map_or(0, |branch| style::width(branch) + extra);
    let mut parts = vec![(path_summary(cwd, path_budget.min(64)), theme.environment)];
    if let Some(branch) = branch {
        parts.push((branch, theme.git_branch));
    }
    parts
}

pub(crate) fn paint_segments(
    parts: &[(String, ColorPair)],
    styles: crate::theme::Styles,
) -> String {
    paint_segments_after(parts, styles, None)
}

fn paint_segments_after(
    parts: &[(String, ColorPair)],
    styles: crate::theme::Styles,
    previous_background: Option<nu_ansi_term::Color>,
) -> String {
    let colored = styles.terminal.color && styles.depth != ColorDepth::Plain;
    let caps = styles.theme.powerline;
    let shapes = styles.powerline();
    let depth = styles.depth;
    let mut output = String::new();
    for (index, (text, pair)) in parts.iter().enumerate() {
        if index == 0 && caps {
            if shapes {
                if let Some(previous) = previous_background {
                    let background = styles
                        .pair_style(*pair, true)
                        .background
                        .expect("colored segment");
                    output.push_str(
                        &nu_ansi_term::Style::new()
                            .fg(previous)
                            .on(background)
                            .paint("\u{e0b0}")
                            .to_string(),
                    );
                } else {
                    output.push_str(&format!("{}\u{e0b6}", cap_sgr(*pair, depth)));
                }
            } else if let Some(previous) = previous_background.filter(|_| colored) {
                output.push_str(
                    &nu_ansi_term::Style::new()
                        .on(previous)
                        .paint(" ")
                        .to_string(),
                );
            } else {
                output.push(' ');
            }
        }
        if colored {
            output.push_str(&pair.sgr(depth));
        }
        output.push(' ');
        output.push_str(text);
        output.push(' ');
        if caps {
            if shapes {
                if let Some((_, next)) = parts.get(index + 1) {
                    let transition = ColorPair::new(
                        pair.background_rgb(),
                        next.background_rgb(),
                        pair.background_index(),
                        next.background_index(),
                    )
                    .expect("validated extended colors");
                    output.push_str(&format!("{}\u{e0b0}", transition.sgr(depth)));
                } else {
                    output.push_str(&format!("{}\u{e0b4}", cap_sgr(*pair, depth)));
                }
            } else {
                output.push(' ');
            }
        }
        if colored {
            output.push_str("\x1b[0m");
        }
    }
    output
}

fn render(fields: &[Field], context: &Context<'_>) -> Layout {
    let columns = usize::from(context.editor.columns.saturating_sub(1));
    if columns < 16 {
        return Layout::default();
    }
    let theme = context.theme;
    let inset = segment_inset(1, theme.powerline);
    let environment_width = context.external_environment.map_or_else(
        || {
            let parts = environment_parts(context, columns);
            parts
                .iter()
                .map(|(text, _)| style::width(text))
                .sum::<usize>()
                + segment_inset(parts.len(), theme.powerline)
        },
        crate::theme::Fragment::width,
    );
    let mut preferred = [environment_width, 0, 0, 0, 0, 0];
    let mut minimum = [environment_width.min(12 + inset), 0, 0, 0, 0, 0];
    for number in 1..6 {
        let padding = if matches!(number, 1 | 4) { inset } else { 0 };
        let full = zone(fields, number as u8, columns, true, padding);
        if !full.text.is_empty() {
            preferred[number] = style::width(&full.text) + padding;
            minimum[number] = minimum_width(fields, number as u8) + padding;
        }
    }
    let mut widths = allocate(
        columns,
        minimum,
        preferred,
        theme.powerline,
        context.external_environment.is_some(),
    );
    let state = zone(fields, 1, widths[1], true, inset);
    let actions = zone(fields, 2, widths[2], state.state_complete, 0);
    let mode = zone(fields, 3, widths[3], true, 0);
    let previous = zone(fields, 4, widths[4], true, inset);
    let usage = zone(fields, 5, widths[5], true, 0);
    if widths[0] > 0 {
        let state_width = if state.text.is_empty() {
            0
        } else {
            style::width(&state.text) + inset
        };
        widths[0] += widths[1].saturating_sub(state_width)
            + widths[2].saturating_sub(style::width(&actions.text))
            + widths[3].saturating_sub(style::width(&mode.text))
            + widths[4].saturating_sub(if previous.text.is_empty() {
                0
            } else {
                style::width(&previous.text) + inset
            })
            + widths[5].saturating_sub(style::width(&usage.text));
    }
    let styles = crate::theme::Styles {
        theme,
        terminal: style::Terminal {
            tty: true,
            ansi: true,
            color: context.color,
            unicode: context.unicode,
        },
        depth: context.color_depth,
    };
    let mut parts = if widths[0] > 0 && context.external_environment.is_none() {
        environment_parts(context, widths[0])
    } else {
        Vec::new()
    };
    for segment in [&previous, &state] {
        if !segment.text.is_empty() {
            parts.push((segment.text.clone(), theme.pair(1, segment.tone)));
        }
    }
    let mut left = if let Some(fragment) = context.external_environment.filter(|_| widths[0] > 0) {
        if parts.is_empty() {
            fragment.render(widths[0], styles)
        } else {
            let (mut prefix, background) = fragment.render_prefix(widths[0], styles);
            prefix.push_str(&paint_segments_after(&parts, styles, background));
            prefix
        }
    } else {
        paint_segments(&parts, styles)
    };
    let usage_color = if matches!(usage.tone, Tone::Notice) {
        theme.text_color(crate::theme::Role::Warning)
    } else {
        theme.mode
    };
    let mut right = String::new();
    for (text, color) in [
        (&usage.text, usage_color),
        (&actions.text, theme.operation),
        (&mode.text, theme.mode),
    ] {
        if text.is_empty() {
            continue;
        }
        if !right.is_empty() {
            right.push_str("  ");
        }
        right.push_str(&styles.pair_style(color, false).paint(text).to_string());
    }
    let used = style::width(&style::strip_ansi(&left)) + style::width(&style::strip_ansi(&right));
    left.push_str(&" ".repeat(columns - used));
    left.push_str(&right);
    if context.color && context.color_depth != ColorDepth::Plain {
        left.push_str("\x1b[0m");
    }
    Layout {
        text: left,
        note_included: state.note_included,
        correction_included: state.state_complete && actions.correction_included,
        included: state.included.into_iter().chain(actions.included).collect(),
    }
}

fn cap_sgr(pair: ColorPair, depth: ColorDepth) -> String {
    let [r, g, b] = pair.background_rgb();
    match depth {
        ColorDepth::Rgb => format!("\x1b[0;49;38;2;{r};{g};{b}m"),
        ColorDepth::Indexed => format!("\x1b[0;49;38;5;{}m", pair.background_index()),
        ColorDepth::Plain => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::PromptEditMode;

    #[test]
    fn injected_theme_changes_only_sgr_with_geometry_and_capability_fallbacks_preserved() {
        let keys = bindings();
        let handle = ThemeHandle::default();
        let mut replacement = handle.snapshot();
        replacement.environment = ColorPair::new([238, 243, 248], [30, 64, 83], 231, 24).unwrap();
        replacement.status.advice = ColorPair::new([238, 243, 248], [39, 51, 74], 231, 25).unwrap();
        let feedback = input_assist::Feedback {
            text: "Unknown gti; try git".into(),
            compact: "gti -> git".into(),
            state: input_assist::State::Error,
            correction: Some(input_assist::Correction {
                version: Default::default(),
                range: 0..3,
                edit_range: 0..3,
                from: "gti".into(),
                to: "git".into(),
            }),
        };
        let mut editor = editor("gti status");
        for width in [32, 48, 64, 80, 120, 160] {
            editor.columns = width;
            for depth in [ColorDepth::Rgb, ColorDepth::Indexed, ColorDepth::Plain] {
                for color in [false, true] {
                    for unicode in [false, true] {
                        handle.replace(Theme::default());
                        let render = || {
                            let mut context = context(&editor, &keys);
                            context.theme = handle.snapshot();
                            context.feedback = Some(&feedback);
                            context.color = color;
                            context.color_depth = depth;
                            context.unicode = unicode;
                            compose(context)
                        };
                        let original = render();
                        handle.replace(replacement);
                        let updated = render();
                        assert_eq!(
                            style::strip_ansi(&original.text),
                            style::strip_ansi(&updated.text)
                        );
                        assert_eq!(original.correction_included, updated.correction_included);
                        assert_eq!(original.note_included, updated.note_included);
                        assert_eq!(
                            style::width(&style::strip_ansi(&updated.text)),
                            usize::from(width - 1)
                        );
                        if color && depth != ColorDepth::Plain {
                            assert_ne!(
                                original.text, updated.text,
                                "the injected palette was not consumed"
                            );
                        } else {
                            assert_eq!(
                                original.text, updated.text,
                                "a theme overrode disabled color"
                            );
                            assert!(!updated.text.contains('\x1b'));
                        }
                    }
                }
            }
        }
    }

    fn bindings() -> Bindings {
        let mut keys = reedline::default_emacs_keybindings();
        keys.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::UntilFound(vec![
                ReedlineEvent::Menu("completion_menu".into()),
                ReedlineEvent::MenuNext,
            ]),
        );
        keys.add_binding(
            KeyModifiers::SHIFT,
            KeyCode::BackTab,
            ReedlineEvent::MenuPrevious,
        );
        keys.add_binding(
            KeyModifiers::NONE,
            KeyCode::F(2),
            ReedlineEvent::ExecuteHostCommand(crate::repl::SUGGEST_COMMAND.into()),
        );
        let mut bindings = Bindings::from_editor(&keys);
        bindings.enable_correction(&keys);
        bindings
    }

    fn editor(buffer: &str) -> PromptContext<'_> {
        PromptContext {
            buffer,
            cursor: buffer.len(),
            completion_cursor: buffer.len(),
            selection: None,
            columns: 180,
            rows: 24,
            edit_mode: PromptEditMode::Emacs,
            interaction: PromptInteraction::Editing,
        }
    }

    fn context<'a>(editor: &'a PromptContext<'a>, keys: &'a Bindings) -> Context<'a> {
        Context {
            editor,
            environment: Some(("~/work/nosh", Some("main"))),
            external_environment: None,
            feedback: None,
            completion: None,
            assistance: None,
            latest_command: None,
            failed_exit: None,
            last_result: None,
            context_usage: None,
            approval: "Approval: Auto",
            note: None,
            bindings: keys,
            color: false,
            color_depth: ColorDepth::Rgb,
            unicode: true,
            theme: Theme::default(),
        }
    }

    fn background_cells(text: &str) -> Vec<Option<nu_ansi_term::Color>> {
        crate::theme::Fragment::parse(text)
            .runs()
            .flat_map(|(text, paint)| std::iter::repeat_n(paint.background, style::width(text)))
            .collect()
    }

    #[test]
    fn compact_segments_preserve_order_without_empty_status_blocks() {
        let keys = bindings();
        let editor = editor("echo ok");
        let feedback = input_assist::Feedback {
            text: "example diagnostic".into(),
            compact: "diagnostic".into(),
            state: input_assist::State::Error,
            correction: None,
        };
        let mut data = context(&editor, &keys);
        data.feedback = Some(&feedback);
        let text = compose(data).text;
        let positions = [
            "~/work/nosh",
            "(main)",
            "! example diagnostic",
            "Ctrl+C cancel",
            "Auto",
        ]
        .map(|part| {
            text.find(part)
                .unwrap_or_else(|| panic!("missing {part}: {text}"))
        });
        assert!(positions.windows(2).all(|pair| pair[0] < pair[1]));
        let mut data = context(&editor, &keys);
        data.color = true;
        let text = compose(data).text;
        assert_eq!(
            background_cells(&text)
                .iter()
                .filter(|color| color.is_some())
                .count(),
            21
        );
        let text = style::strip_ansi(&text);
        assert_eq!(style::width(&text), 179);
        assert!(!text.contains("READY"));
        assert!(text.ends_with("Tab complete  Auto"));
    }

    #[test]
    fn command_metrics_use_the_real_exit_and_two_second_duration_threshold() {
        let keys = bindings();
        let editor = editor("");
        for (exit, millis, expected) in [
            (0, 1999, None),
            (0, 2000, Some("last 2.0s")),
            (0, 63000, Some("last 1m03s")),
            (7, 1, Some("last exit 7")),
            (7, 2500, Some("last exit 7 2.5s")),
        ] {
            let mut data = context(&editor, &keys);
            data.last_result = Some((exit, std::time::Duration::from_millis(millis)));
            data.failed_exit = (exit != 0).then_some(exit);
            let text = compose(data).text;
            if let Some(expected) = expected {
                assert!(text.contains(expected), "{text}");
            } else {
                assert!(!text.contains("last") && !text.contains("exit"), "{text}");
            }
            assert!(!text.contains("Last failure"), "{text}");
        }
    }

    #[test]
    fn context_usage_is_optional_and_high_usage_keeps_a_readable_warning() {
        let keys = bindings();
        let editor = editor("");
        for (used, max) in [(0, 0), (1536, 8192), (870, 1000), (usize::MAX, usize::MAX)] {
            let mut data = context(&editor, &keys);
            data.context_usage = Some(crate::repl::ContextUsage { used, max });
            data.color = true;
            let row = compose(data).text;
            let text = style::strip_ansi(&row);
            assert_eq!(text.contains("ctx"), max > 0, "{text}");
            if max == 1000 {
                assert!(text.contains("ctx 870/1000"), "{text}");
                let warning = Theme::default().text_color(crate::theme::Role::Warning);
                let style = crate::theme::Styles {
                    theme: Theme::default(),
                    terminal: style::Terminal {
                        tty: true,
                        ansi: true,
                        color: true,
                        unicode: true,
                    },
                    depth: ColorDepth::Rgb,
                };
                assert!(
                    row.contains(
                        &style
                            .pair_style(warning, false)
                            .paint("ctx 870/1000")
                            .to_string()
                    )
                );
            }
        }
    }

    #[test]
    fn pending_ai_state_requires_a_matching_command_and_never_offers_acceptance() {
        let keys = bindings();
        let pending = Assistance::Pending {
            command_id: 4,
            intent: "Next".into(),
        };
        for (buffer, latest, shown) in [
            ("", Some(4), true),
            ("", Some(5), false),
            ("echo", Some(4), false),
        ] {
            let editor = editor(buffer);
            let mut data = context(&editor, &keys);
            data.assistance = Some(&pending);
            data.latest_command = latest;
            let text = compose(data).text;
            assert_eq!(text.contains("generating suggestion"), shown, "{text}");
            assert!(!text.contains("F2"), "{text}");
        }
    }

    #[test]
    fn additional_metrics_yield_to_menu_actions_and_respect_native_ownership() {
        let keys = bindings();
        let mut editor = editor("");
        editor.interaction = PromptInteraction::Menu {
            name: "completion_menu",
            count: 9,
            provisional: false,
        };
        for width in 0..=240 {
            editor.columns = width;
            let mut data = context(&editor, &keys);
            data.last_result = Some((2, std::time::Duration::from_millis(2400)));
            data.context_usage = Some(crate::repl::ContextUsage {
                used: 7000,
                max: 8192,
            });
            let text = compose(data).text;
            assert!(
                style::width(&text) <= usize::from(width.saturating_sub(1)),
                "{width}: {text}"
            );
            if width >= 48 {
                assert!(
                    text.contains("Esc") && text.contains("Enter"),
                    "{width}: {text}"
                );
            }
        }
        editor.columns = 180;
        let native = crate::theme::Fragment::parse("native exit 2 2s");
        let mut data = context(&editor, &keys);
        data.external_environment = Some(&native);
        data.last_result = Some((2, std::time::Duration::from_secs(2)));
        data.failed_exit = Some(2);
        let text = compose(data).text;
        assert_eq!(text.matches("exit 2").count(), 1, "{text}");
        assert!(
            !text.contains("last") && !text.contains("Last failure"),
            "{text}"
        );
    }

    #[test]
    fn every_column_budget_preserves_graphemes_and_restores_information() {
        let keys = bindings();
        let mut editor = editor("echo");
        let path = format!("~/long/{}/project", "中e\u{301}👩\u{200d}💻".repeat(24));
        for color in [false, true] {
            for width in 0..=240 {
                editor.columns = width;
                let mut data = context(&editor, &keys);
                data.environment = Some((&path, Some("feature/very-long-branch")));
                data.color = color;
                let text = style::strip_ansi(&compose(data).text);
                assert!(
                    style::width(&text) <= usize::from(width.saturating_sub(1)),
                    "{width}: {text}"
                );
                assert!(!text.contains(['\n', '\r', '\x1b']));
                assert!(!text.starts_with(" | ") && !text.ends_with(" | "));
                assert!(!text.contains("|  |"));
            }
        }
        editor.columns = 500;
        let mut data = context(&editor, &keys);
        data.environment = Some((&path, Some("main")));
        let text = compose(data).text;
        assert!(text.contains("project"), "{text}");
        assert!(
            !text.contains(&path),
            "long parent paths must stay abbreviated"
        );
    }

    #[test]
    fn mode_is_optional_and_never_replaces_the_primary_action() {
        let keys = bindings();
        let mut editor = editor("echo hello");
        for width in 1..=160 {
            editor.columns = width;
            let full = compose(context(&editor, &keys)).text;
            let mut data = context(&editor, &keys);
            data.approval = "";
            let without = compose(data).text;
            let columns = usize::from(width.saturating_sub(1));
            assert!(style::width(&full) <= columns && style::width(&without) <= columns);
            assert!(!without.contains("Auto"));
            if width >= 32 {
                assert!(
                    full.contains("Tab") && without.contains("Tab"),
                    "{width}: {full}"
                );
            }
            if full.contains("Auto") {
                assert!(full.ends_with("Auto"));
            }
        }
    }

    #[test]
    fn diagnostics_and_required_actions_beat_long_environment_and_modes() {
        let keys = bindings();
        let mut editor = editor("missing");
        editor.columns = 55;
        let feedback = input_assist::Feedback {
            text: "No executable command".into(),
            compact: "No executable command".into(),
            state: input_assist::State::Error,
            correction: None,
        };
        let mut data = context(&editor, &keys);
        data.environment = Some((
            "/a/very/long/directory/whose/name/does/not/fit",
            Some("feature/long"),
        ));
        data.feedback = Some(&feedback);
        let text = compose(data).text;
        assert!(text.contains("No executable command"), "{text}");
        assert!(
            text.contains("^C cancel") || text.contains("Ctrl+C cancel"),
            "{text}"
        );
        assert!(!text.contains("Approval:"), "{text}");
    }

    #[test]
    fn menu_and_search_hide_draft_feedback_and_gate_acceptance() {
        let keys = bindings();
        let feedback = input_assist::Feedback {
            text: "STALE DRAFT".into(),
            compact: "STALE".into(),
            state: input_assist::State::Error,
            correction: None,
        };
        let mut editor = editor("query");
        for interaction in [
            PromptInteraction::Menu {
                name: "completion_menu",
                count: 0,
                provisional: false,
            },
            PromptInteraction::Menu {
                name: "completion_menu",
                count: 2,
                provisional: true,
            },
            PromptInteraction::HistorySearch {
                term: "needle",
                has_match: false,
                status: None,
            },
        ] {
            editor.interaction = interaction;
            let mut data = context(&editor, &keys);
            data.feedback = Some(&feedback);
            let text = compose(data).text;
            assert!(text.contains("Esc"), "{text}");
            assert!(
                !text.contains("Enter") && !text.contains("STALE") && !text.contains("Ctrl+G"),
                "{text}"
            );
        }
        editor.interaction = PromptInteraction::Menu {
            name: "completion_menu",
            count: 12,
            provisional: false,
        };
        let text = compose(context(&editor, &keys)).text;
        assert!(
            text.contains("12") && text.contains("Enter") && text.contains("Esc"),
            "{text}"
        );
    }

    #[test]
    fn contextual_search_hints_remain_independent_when_one_action_is_unbound() {
        for (action, expected, missing) in [("accept", "Esc", "Enter"), ("cancel", "Enter", "Esc")]
        {
            let mut config = crate::editing::Config {
                mode: crate::editing::Mode::Emacs,
                ..Default::default()
            };
            config.keybindings.contexts.insert(
                "history_search".into(),
                std::collections::BTreeMap::from([(action.into(), Vec::new())]),
            );
            let (compiled, notices) =
                config.compile(crate::editing::Capabilities { enhanced: false }, true);
            assert!(notices.is_empty(), "{notices:?}");
            for has_match in [false, true] {
                let mut editor = editor("query");
                editor.interaction = PromptInteraction::HistorySearch {
                    term: "needle",
                    has_match,
                    status: None,
                };
                let keys = compiled.hints(&editor.edit_mode, editor.interaction);
                let text = compose(context(&editor, keys)).text;
                if expected == "Enter" && !has_match {
                    assert!(!text.contains("Enter"), "{action}: {text}");
                } else {
                    assert!(
                        text.contains(expected)
                            || (expected == "Ctrl+G" && text.contains("^G cancel")),
                        "{action}: {text}"
                    );
                }
                assert!(!text.contains(missing), "{action}: {text}");
            }
        }
    }

    #[test]
    fn ai_acceptance_requires_a_current_candidate_not_just_a_failure() {
        let keys = bindings();
        for buffer in ["", "   "] {
            let editor = editor(buffer);
            assert!(!compose(context(&editor, &keys)).text.contains("F2"));
            let mut data = context(&editor, &keys);
            data.failed_exit = Some(7);
            let text = compose(data).text;
            assert!(!text.contains("F2") && text.contains('7'), "{text}");
            let candidate = Assistance::Command {
                command_id: 9,
                intent: "next".into(),
                program: "printf safe".into(),
            };
            let mut data = context(&editor, &keys);
            data.assistance = Some(&candidate);
            data.latest_command = Some(9);
            let text = compose(data).text;
            assert!(
                text.contains("next: printf safe") && text.contains("F2"),
                "{text}"
            );
            let mut data = context(&editor, &keys);
            data.assistance = Some(&candidate);
            data.latest_command = Some(10);
            assert!(!compose(data).text.contains("printf safe"));
        }
        let editor = editor("describe this task");
        let text = compose(context(&editor, &keys)).text;
        assert!(text.contains("Tab") && !text.contains("F2"), "{text}");
    }

    #[test]
    fn hints_come_from_the_actual_keymap() {
        let mut keys = reedline::default_emacs_keybindings();
        keys.remove_binding(KeyModifiers::CONTROL, KeyCode::Char('c'));
        keys.add_binding(KeyModifiers::ALT, KeyCode::Char('q'), ReedlineEvent::CtrlC);
        let keys = Bindings::from_editor(&keys);
        let editor = editor("echo");
        let feedback = input_assist::Feedback {
            text: "Input error".into(),
            compact: "Error".into(),
            state: input_assist::State::Error,
            correction: None,
        };
        let mut data = context(&editor, &keys);
        data.feedback = Some(&feedback);
        let text = compose(data).text;
        assert!(text.contains("Alt+Q"), "{text}");
        assert!(
            !text.contains("Ctrl+C") && !text.contains("Ctrl+G") && !text.contains("Tab"),
            "{text}"
        );
    }

    #[test]
    fn errors_are_not_empty_completions_and_controls_cannot_escape_fields() {
        let keys = bindings();
        let mut editor = editor("abc");
        editor.interaction = PromptInteraction::Menu {
            name: "completion_menu",
            count: 0,
            provisional: false,
        };
        let completion = Completion {
            input: "abc".into(),
            cursor: 3,
            count: 0,
            error: Some("failure\x1b[2J\nnext".into()),
            phase: crate::completion::Phase::Unavailable,
        };
        let mut data = context(&editor, &keys);
        data.completion = Some(&completion);
        let text = compose(data).text;
        assert!(
            text.contains("failure") && text.contains("\\nnext"),
            "{text}"
        );
        assert!(
            !text.contains('\x1b') && !text.contains('\n') && !text.contains("Enter"),
            "{text}"
        );
        assert!(plain(&"👩\u{200d}💻".repeat(200)).ends_with("..."));
    }

    #[test]
    fn partial_menu_results_show_acceptance_but_pending_results_do_not() {
        let keys = bindings();
        let mut editor = editor("query");
        editor.columns = 200;
        editor.interaction = PromptInteraction::Menu {
            name: "completion_menu",
            count: 2,
            provisional: true,
        };
        for phase in [
            crate::completion::Phase::Partial,
            crate::completion::Phase::Pending,
        ] {
            let completion = Completion {
                input: "query".into(),
                cursor: 5,
                error: None,
                count: 2,
                phase,
            };
            let mut data = context(&editor, &keys);
            data.completion = Some(&completion);
            let text = compose(data).text;
            if phase == crate::completion::Phase::Partial {
                assert!(text.contains("Partial") && text.contains("Enter"), "{text}");
            } else {
                assert!(
                    text.contains("pending") && !text.contains("Enter"),
                    "{text}"
                );
            }
            assert!(
                !text.contains("No completions") && text.contains("2+"),
                "{text}"
            );
        }
    }

    #[test]
    fn short_terminal_yields_the_entire_decoration() {
        let keys = bindings();
        let mut editor = editor("echo");
        editor.rows = 2;
        assert!(compose(context(&editor, &keys)).text.is_empty());
    }

    #[test]
    fn narrow_information_row_keeps_real_feedback_and_does_not_fall_back() {
        let keys = bindings();
        let mut editor = editor("printf safe >> output");
        editor.columns = 46;
        let feedback = input_assist::Feedback {
            text: "Input: new output target; writability not checked".into(),
            compact: "Output unchecked".into(),
            state: input_assist::State::Known,
            correction: None,
        };
        let mut data = context(&editor, &keys);
        data.environment = Some(("~", Some("main")));
        data.feedback = Some(&feedback);
        let text = compose(data).text;
        assert!(text.contains("Output unchecked"), "{text}");
        assert_eq!(style::width(&text), 45);
        assert!(!text.contains("Approval:"));
    }

    #[test]
    fn zero_width_text_still_reserves_its_padding() {
        let fields = [
            Field::new(1, "\u{301}", "\u{301}", "\u{301}", 100, true),
            Field::new(2, "Esc exit", "Esc exit", "Esc exit", 99, true),
        ];
        let keys = bindings();
        let mut editor = editor("");
        for columns in 0..40 {
            editor.columns = columns + 1;
            let layout = render(&fields, &context(&editor, &keys));
            assert!(
                style::width(&layout.text) <= usize::from(columns),
                "{columns}: {:?}",
                layout.text
            );
        }
    }

    #[test]
    fn ordinary_editing_has_one_primary_hint_and_no_emacs_label() {
        let keys = bindings();
        for buffer in ["", " \t\n", "echo 中文", "echo 'quoted args'"] {
            let editor = editor(buffer);
            let mut data = context(&editor, &keys);
            data.approval = "";
            let text = compose(data).text;
            assert_eq!(text.contains("Ctrl+R"), buffer.trim().is_empty(), "{text}");
            assert_eq!(text.contains("Tab"), !buffer.trim().is_empty(), "{text}");
            assert!(
                !text.contains("Emacs") && !text.contains("Approval:") && !text.contains("Ctrl+G"),
                "{text}"
            );
        }
        let mut editor = editor("");
        assert!(compose(context(&editor, &keys)).text.ends_with("Auto"));
        editor.columns = 80;
        let text = compose(context(&editor, &keys)).text;
        assert!(text.ends_with("Auto"));
    }

    #[test]
    fn long_paths_keep_recognizable_tail_components_without_multiple_fragments() {
        let path = "/mnt/c/github/copilot-worktrees/nosh/newfuture-glowing-spoon";
        assert_eq!(path_summary(path, 40), "/.../nosh/newfuture-glowing-spoon");
        assert_eq!(path_summary(path, 28), "/.../newfuture-glowing-spoon");
        assert_eq!(
            path_summary("~/projects/other/nosh", 20),
            "~/.../other/nosh"
        );
        for path in ["/", "~", "~/nosh", "/中文/e\u{301}", "/work/nosh"] {
            assert_eq!(path_summary(path, 40), path);
        }
        for columns in 4..=40 {
            let text = path_summary("/very/long/👩\u{200d}💻e\u{301}中文目录", columns);
            assert!(style::width(&text) <= columns, "{columns}: {text}");
            assert!(text.matches("...").count() <= 2, "{text}");
            assert!(!text.contains('\u{fffd}'));
        }
        let home = "~/work/copilot-worktrees/nosh/newfuture-glowing-spoon";
        let narrow = path_summary(home, 28);
        assert!(
            narrow == ".../newfuture-glowing-spoon" && style::width(&narrow) <= 28,
            "{narrow}"
        );
        let keys = bindings();
        let mut editor = editor("");
        editor.columns = 80;
        let mut data = context(&editor, &keys);
        data.environment = Some((home, Some("main")));
        data.approval = "";
        let text = compose(data).text;
        assert!(
            text.contains(home) || text.contains(".../newfuture-glowing-spoon"),
            "{text}"
        );
    }

    #[test]
    fn correction_action_replaces_tab_and_requires_end_cursor_and_real_binding() {
        let keys = bindings();
        let mut editor = editor("gti status");
        let feedback = input_assist::Feedback {
            text: "Unknown gti; try git".into(),
            compact: "gti -> git".into(),
            state: input_assist::State::Error,
            correction: Some(input_assist::Correction {
                version: Default::default(),
                range: 0..3,
                edit_range: 0..3,
                from: "gti".into(),
                to: "git".into(),
            }),
        };
        let mut data = context(&editor, &keys);
        data.feedback = Some(&feedback);
        let layout = compose(data);
        assert!(
            layout.correction_included
                && layout.text.contains("git")
                && !layout.text.contains("Tab"),
            "{}",
            layout.text
        );
        for (cursor, selection) in [(1, None), (10, Some((0, 3)))] {
            editor.cursor = cursor;
            editor.selection = selection;
            let mut data = context(&editor, &keys);
            data.feedback = Some(&feedback);
            let layout = compose(data);
            assert!(
                !layout.correction_included && layout.text.contains("Tab"),
                "{}",
                layout.text
            );
        }
        editor.cursor = editor.buffer.len();
        editor.selection = None;
        let mut remapped = reedline::default_emacs_keybindings();
        remapped.add_binding(KeyModifiers::NONE, KeyCode::Right, ReedlineEvent::Esc);
        let mut remapped = Bindings::from_editor(&remapped);
        remapped.enable_correction(&reedline::Keybindings::new());
        let mut data = context(&editor, &remapped);
        data.feedback = Some(&feedback);
        assert!(!compose(data).correction_included);
    }

    #[test]
    fn styled_and_plain_labels_have_identical_visible_text_and_column_budget() {
        let keys = bindings();
        let feedback = input_assist::Feedback {
            text: "Unknown gti; try git".into(),
            compact: "gti -> git".into(),
            state: input_assist::State::Error,
            correction: Some(input_assist::Correction {
                version: Default::default(),
                range: 0..3,
                edit_range: 0..3,
                from: "gti".into(),
                to: "git".into(),
            }),
        };
        let mut editor = editor("gti status");
        for unicode in [false, true] {
            for columns in 0..=200 {
                editor.columns = columns;
                let make = |color| {
                    let mut data = context(&editor, &keys);
                    data.feedback = Some(&feedback);
                    data.unicode = unicode;
                    data.color = color;
                    compose(data)
                };
                let plain = make(false);
                let styled = make(true);
                assert_eq!(
                    style::strip_ansi(&styled.text),
                    plain.text,
                    "{unicode}/{columns}"
                );
                assert!(!plain.text.contains('\x1b'));
                assert!(style::width(&plain.text) <= usize::from(columns.saturating_sub(1)));
                assert!(
                    !styled.text.contains("\x1b[32m"),
                    "a recommendation must not look like success"
                );
                if !unicode {
                    assert!(plain.text.is_ascii(), "{}", plain.text);
                }
                if !styled.text.is_empty() {
                    assert!(styled.text.ends_with("\x1b[0m"), "style leaked at row end");
                }
            }
        }
    }

    #[test]
    fn suggestions_errors_and_unavailable_states_use_distinct_non_success_tones() {
        let keys = bindings();
        let editor = editor("missing");
        for (state, prefix, tone) in [
            (input_assist::State::Error, "! ", Tone::Error),
            (input_assist::State::Unavailable, "? ", Tone::Notice),
        ] {
            let feedback = input_assist::Feedback {
                text: "clear reason".into(),
                compact: "reason".into(),
                state,
                correction: None,
            };
            let mut data = context(&editor, &keys);
            data.feedback = Some(&feedback);
            data.color = true;
            let text = compose(data).text;
            let sgr = palette::pair(1, tone).sgr(ColorDepth::Rgb);
            assert!(
                text.contains(&sgr)
                    && style::strip_ansi(&text).contains(&format!("{prefix}clear reason")),
                "{text}"
            );
            assert!(!text.contains("\x1b[32m"));
        }
    }

    #[test]
    fn narrow_compact_layout_preserves_required_exit() {
        let mut keys = bindings();
        keys.0.clear();
        keys.0.insert(Action::Close, "Esc".into());
        let label = keys.field(Action::Close, "exit", 99, true, false).unwrap();
        let mut editor = editor("");
        for columns in 32..=55 {
            editor.columns = columns + 1;
            let layout = render(std::slice::from_ref(&label), &context(&editor, &keys));
            assert!(
                !layout.text.is_empty()
                    && layout.text.contains("Esc")
                    && layout.text.contains("exit"),
                "{columns}: {}",
                layout.text
            );
            assert_eq!(style::width(&layout.text), usize::from(columns));
        }
    }

    #[test]
    fn state_transitions_keep_one_safe_row_without_input_geometry_changes() {
        let keys = bindings();
        let feedback = |text: &str, compact: &str, state, correction| input_assist::Feedback {
            text: text.into(),
            compact: compact.into(),
            state,
            correction,
        };
        let samples = [
            ("", None),
            ("echo 中文 e\u{301}", None),
            (
                "echo 中文 e\u{301}",
                Some(feedback(
                    "Pending: looking up a very long input",
                    "Query pending",
                    input_assist::State::Pending,
                    None,
                )),
            ),
            (
                "echo 中文 e\u{301}",
                Some(feedback(
                    "The diagnostic worker is not available right now",
                    "Unavailable",
                    input_assist::State::Unavailable,
                    None,
                )),
            ),
            (
                "unknown",
                Some(feedback(
                    "No executable command: unknown",
                    "No command",
                    input_assist::State::Error,
                    None,
                )),
            ),
            (
                "gti status",
                Some(feedback(
                    "Unknown gti; try git",
                    "gti -> git",
                    input_assist::State::Error,
                    Some(input_assist::Correction {
                        version: Default::default(),
                        range: 0..3,
                        edit_range: 0..3,
                        from: "gti".into(),
                        to: "git".into(),
                    }),
                )),
            ),
            ("git status", None),
            ("", None),
        ];
        for width in [17, 32, 48, 64, 80, 100, 120, 160, 240] {
            let columns = usize::from(width - 1);
            for unicode in [false, true] {
                for color in [false, true] {
                    for (index, (buffer, feedback)) in samples.iter().enumerate() {
                        let mut editor = editor(buffer);
                        editor.columns = width;
                        let mut data = context(&editor, &keys);
                        data.feedback = feedback.as_ref();
                        data.environment = Some((
                            "~/work/copilot-worktrees/nosh/newfuture-glowing-spoon",
                            Some("main"),
                        ));
                        data.unicode = unicode;
                        data.color = color;
                        if index % 2 == 1 {
                            data.approval = "";
                        }
                        let layout = compose(data);
                        assert_eq!(
                            style::width(&style::strip_ansi(&layout.text)),
                            columns,
                            "{width}/{index}"
                        );
                        assert!(!layout.text.contains(['\n', '\r']), "{width}/{index}");
                        let text = style::strip_ansi(&layout.text);
                        if columns >= 79
                            && !buffer.trim().is_empty()
                            && feedback
                                .as_ref()
                                .is_none_or(|feedback| feedback.correction.is_none())
                        {
                            assert!(
                                text.contains("Tab "),
                                "ordinary action lost after asynchronous feedback: {width}/{index}: {text}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn menu_counts_and_mode_visibility_preserve_status_and_required_actions() {
        let keys = bindings();
        for width in [48, 80, 120, 160] {
            let mut editor = editor("cat candidate_");
            editor.columns = width;
            for count in [9, 10, 99, 100] {
                editor.interaction = PromptInteraction::Menu {
                    name: "completion_menu",
                    count,
                    provisional: false,
                };
                for approval in ["", "Approval: Auto", "Approval: Confirm"] {
                    let mut data = context(&editor, &keys);
                    data.approval = approval;
                    let text = compose(data).text;
                    assert!(
                        (text.contains("Menu") || text.contains("Completion"))
                            && text.contains(&count.to_string())
                            && text.contains("Esc")
                            && text.contains("Enter"),
                        "{text}"
                    );
                    assert_eq!(style::width(&text), usize::from(width - 1));
                }
            }
        }
    }

    #[test]
    fn backgrounds_cover_only_content_segments_not_gaps_or_right_hints() {
        let keys = bindings();
        let mut editor = editor("gti status");
        editor.columns = 120;
        for state in [
            input_assist::State::Known,
            input_assist::State::Pending,
            input_assist::State::Unavailable,
            input_assist::State::Error,
        ] {
            let feedback = input_assist::Feedback {
                text: "clear reason".into(),
                compact: "reason".into(),
                state,
                correction: None,
            };
            let mut data = context(&editor, &keys);
            data.color = true;
            data.feedback = Some(&feedback);
            let text = compose(data).text;
            assert!(text.starts_with(&format!(
                "{} ",
                palette::pair(0, Tone::Environment).sgr(ColorDepth::Rgb)
            )));
            assert!(text.ends_with("\x1b[0m"));
            assert_eq!(style::width(&style::strip_ansi(&text)), 119);
            let backgrounds = background_cells(&text);
            let colored = backgrounds.iter().filter(|color| color.is_some()).count();
            assert!(colored > 21 && colored < 50, "{colored}: {text}");
            let plain = style::strip_ansi(&text);
            let right = style::width(&plain[..plain.find("Tab").unwrap()]);
            assert!(backgrounds[right..].iter().all(Option::is_none));
            match state {
                input_assist::State::Pending => {
                    assert!(text.contains(&palette::pair(1, Tone::Pending).sgr(ColorDepth::Rgb)))
                }
                input_assist::State::Unavailable => {
                    assert!(text.contains(&palette::pair(1, Tone::Notice).sgr(ColorDepth::Rgb)))
                }
                input_assist::State::Error => {
                    assert!(text.contains(&palette::pair(1, Tone::Error).sgr(ColorDepth::Rgb)))
                }
                _ => assert!(!text.contains(&palette::pair(1, Tone::Error).sgr(ColorDepth::Rgb))),
            }
        }
    }

    #[test]
    fn adjacent_left_segments_have_no_host_background_between_them() {
        let keys = bindings();
        let feedback = input_assist::Feedback {
            text: "display issue".into(),
            compact: "issue".into(),
            state: input_assist::State::Error,
            correction: None,
        };
        let mut editor = editor("missing");
        for width in [48, 80, 120, 219] {
            editor.columns = width;
            for powerline in [false, true] {
                for unicode in [false, true] {
                    for depth in [ColorDepth::Rgb, ColorDepth::Indexed] {
                        let mut data = context(&editor, &keys);
                        data.color = true;
                        data.color_depth = depth;
                        data.unicode = unicode;
                        data.theme.powerline = powerline;
                        data.feedback = Some(&feedback);
                        data.last_result = Some((7, std::time::Duration::from_millis(2400)));
                        let row = compose(data).text;
                        let cells = background_cells(&row);
                        let first = cells.iter().position(Option::is_some).unwrap();
                        let last = cells.iter().rposition(Option::is_some).unwrap();
                        assert!(
                            cells[first..=last].iter().all(Option::is_some),
                            "{width}: {row:?}"
                        );
                        assert!(cells[last + 1..].iter().all(Option::is_none));
                        let text = style::strip_ansi(&row);
                        assert_eq!(style::width(&text), usize::from(width - 1));
                        if powerline && unicode {
                            assert_eq!(text.matches('\u{e0b6}').count(), 1, "{text}");
                            assert_eq!(text.matches('\u{e0b4}').count(), 1, "{text}");
                        } else {
                            assert!(!text.contains(['\u{e0b6}', '\u{e0b4}', '\u{e0b0}']));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn native_environment_joins_following_state_without_double_caps_or_white_ellipsis() {
        let keys = bindings();
        let feedback = input_assist::Feedback {
            text: "display issue".into(),
            compact: "issue".into(),
            state: input_assist::State::Error,
            correction: None,
        };
        let mut editor = editor("missing");
        for powerline in [false, true] {
            for unicode in [false, true] {
                let theme = Theme {
                    powerline,
                    ..Theme::default()
                };
                let native_styles = crate::theme::Styles {
                    theme: Theme {
                        powerline: powerline && unicode,
                        ..theme
                    },
                    terminal: style::Terminal {
                        tty: true,
                        ansi: true,
                        color: true,
                        unicode: true,
                    },
                    depth: ColorDepth::Rgb,
                };
                let fragment = crate::theme::Fragment::parse(&paint_segments(
                    &[
                        ("native directory".into(), theme.environment),
                        ("main".into(), theme.git_branch),
                    ],
                    native_styles,
                ));
                for depth in [ColorDepth::Rgb, ColorDepth::Indexed, ColorDepth::Plain] {
                    for color in [false, true] {
                        for columns in 32..=160 {
                            editor.columns = columns;
                            let mut data = context(&editor, &keys);
                            data.color = color;
                            data.color_depth = depth;
                            data.unicode = unicode;
                            data.theme = theme;
                            data.external_environment = Some(&fragment);
                            data.feedback = Some(&feedback);
                            let row = compose(data).text;
                            let text = style::strip_ansi(&row);
                            assert_eq!(style::width(&text), usize::from(columns - 1));
                            if !powerline || !unicode {
                                assert!(!text.contains(['\u{e0b6}', '\u{e0b4}', '\u{e0b0}']));
                            }
                            let cells = background_cells(&row);
                            if !color || depth == ColorDepth::Plain {
                                assert!(!row.contains('\x1b'));
                                assert!(cells.iter().all(Option::is_none));
                                continue;
                            }
                            if powerline && unicode {
                                assert_eq!(
                                    text.matches('\u{e0b6}').count(),
                                    1,
                                    "{columns}: {text}"
                                );
                                assert_eq!(
                                    text.matches('\u{e0b4}').count(),
                                    1,
                                    "{columns}: {text}"
                                );
                            }
                            let first = cells.iter().position(Option::is_some).unwrap();
                            let last = cells.iter().rposition(Option::is_some).unwrap();
                            assert!(
                                cells[first..=last].iter().all(Option::is_some),
                                "{columns}/{powerline}/{unicode}/{depth:?}: {row:?}"
                            );
                            assert!(cells[last + 1..].iter().all(Option::is_none));
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn compact_background_area_does_not_grow_with_terminal_width() {
        let keys = bindings();
        let mut editor = editor("echo 中文 e\u{301}");
        for width in 17..=240 {
            editor.columns = width;
            for unicode in [false, true] {
                for depth in [ColorDepth::Plain, ColorDepth::Indexed, ColorDepth::Rgb] {
                    for color in [false, true] {
                        let mut data = context(&editor, &keys);
                        data.color = color;
                        data.color_depth = depth;
                        data.unicode = unicode;
                        let text = compose(data).text;
                        let visible = style::strip_ansi(&text);
                        assert_eq!(style::width(&visible), usize::from(width - 1));
                        assert!(!visible.contains(['│', '|']));
                        if !color || depth == ColorDepth::Plain {
                            assert!(!text.contains('\x1b'));
                        } else {
                            assert!(text.ends_with("\x1b[0m"));
                            assert!(
                                background_cells(&text)
                                    .iter()
                                    .filter(|color| color.is_some())
                                    .count()
                                    <= 21
                            );
                        }
                    }
                }
            }
        }
        for width in [80, 120, 219, 500] {
            editor.columns = width;
            let mut data = context(&editor, &keys);
            data.color = true;
            let text = compose(data).text;
            assert_eq!(
                background_cells(&text)
                    .iter()
                    .filter(|color| color.is_some())
                    .count(),
                21
            );
            assert!(style::strip_ansi(&text).ends_with("Tab complete  Auto"));
        }
    }

    #[test]
    fn powerline_caps_only_content_segments_and_preserves_non_nerd_fallbacks() {
        let keys = bindings();
        for width in [17, 32, 48, 64, 80, 120, 160] {
            let mut editor = editor("echo value");
            editor.columns = width;
            for unicode in [false, true] {
                for color in [false, true] {
                    let mut data = context(&editor, &keys);
                    data.theme.powerline = true;
                    data.unicode = unicode;
                    data.color = color;
                    let row = compose(data).text;
                    let text = style::strip_ansi(&row);
                    assert_eq!(style::width(&text), usize::from(width - 1));
                    if color && unicode && width >= 64 {
                        assert!(text.starts_with('\u{e0b6}') && text.contains('\u{e0b4}'));
                        assert!(!text.ends_with('\u{e0b4}'));
                    } else if !color || !unicode {
                        assert!(!text.contains(['\u{e0b6}', '\u{e0b4}', '\u{e0b0}']));
                    }
                    if width >= 80 {
                        assert!(text.ends_with("Tab complete  Auto"));
                    }
                }
            }
        }
    }

    #[test]
    fn cached_external_environment_keeps_native_styles_without_an_extra_wrapper() {
        let keys = bindings();
        let fragment = crate::theme::Fragment::parse(
            "\x1b[38;2;80;160;220m\u{e0b6}project (main)\u{e0b4}\x1b[0m",
        );
        let mut editor = editor("echo value");
        for width in [48, 80, 120, 160] {
            editor.columns = width;
            let mut data = context(&editor, &keys);
            data.color = true;
            data.theme.powerline = true;
            data.external_environment = Some(&fragment);
            let after = compose(data);
            let text = style::strip_ansi(&after.text);
            assert_eq!(text.matches('\u{e0b6}').count(), 1);
            assert_eq!(text.matches('\u{e0b4}').count(), 1);
            assert!(after.text.contains("38;2;80;160;220"));
            assert!(text.ends_with("Tab complete  Auto"));
            assert!(background_cells(&after.text).iter().all(Option::is_none));
        }
    }

    #[test]
    fn removing_dividers_does_not_strip_literal_text_from_regions() {
        let fields = [Field::new(
            1,
            "literal │",
            "literal │",
            "literal │",
            100,
            true,
        )];
        let keys = bindings();
        let editor = editor("");
        let mut data = context(&editor, &keys);
        data.environment = Some(("~/literal|", None));
        let layout = render(&fields, &data);
        assert!(layout.text.contains("~/literal|") && layout.text.contains("literal │"));
    }

    #[test]
    fn narrow_layout_keeps_complete_recommendation_and_adoption_keys() {
        let keys = bindings();
        let mut editor = editor("gti status");
        editor.columns = 80;
        let feedback = input_assist::Feedback {
            text: "Unknown gti; try git".into(),
            compact: "gti -> git".into(),
            state: input_assist::State::Error,
            correction: Some(input_assist::Correction {
                version: Default::default(),
                range: 0..3,
                edit_range: 0..3,
                from: "gti".into(),
                to: "git".into(),
            }),
        };
        for unicode in [false, true] {
            for color in [false, true] {
                let mut data = context(&editor, &keys);
                data.environment = Some((
                    "~/work/copilot-worktrees/nosh/newfuture-glowing-spoon",
                    None,
                ));
                data.feedback = Some(&feedback);
                data.unicode = unicode;
                data.color = color;
                let layout = compose(data);
                let visible = style::strip_ansi(&layout.text);
                assert!(
                    visible.contains("gti") && visible.contains("git"),
                    "{visible}"
                );
                assert!(
                    visible.contains(if unicode { "→" } else { "Right" }),
                    "{visible}"
                );
                assert!(
                    visible.contains("^C cancel") || visible.contains("Ctrl+C cancel"),
                    "{visible}"
                );
                assert!(
                    layout.correction_included,
                    "narrow display lost complete candidate/adoption meaning"
                );
                assert!(visible.contains(".../"), "{visible}");
                assert_eq!(style::width(&visible), 79);
            }
        }
        editor.buffer = "unknown";
        editor.cursor = editor.buffer.len();
        let feedback = input_assist::Feedback {
            text: "No executable command: unknown".into(),
            compact: "No cmd".into(),
            state: input_assist::State::Error,
            correction: None,
        };
        let mut data = context(&editor, &keys);
        data.feedback = Some(&feedback);
        let text = compose(data).text;
        assert!(
            text.contains("No executable command") || text.contains("No cmd"),
            "{text}"
        );
        assert!(
            text.contains("Tab") && (text.contains("^C cancel") || text.contains("Ctrl+C cancel")),
            "{text}"
        );
    }

    #[test]
    fn experience_examples_cover_wide_narrow_and_plain_presentations() {
        let keys = bindings();
        for (name, buffer, correction) in [
            ("blank", "", false),
            ("ordinary", "git status", false),
            ("correction", "gti status", true),
            ("unknown", "not_a_known_command", false),
        ] {
            let mut editor = editor(buffer);
            let feedback = if correction {
                Some(input_assist::Feedback {
                    text: "Unknown gti; try git".into(),
                    compact: "gti -> git".into(),
                    state: input_assist::State::Error,
                    correction: Some(input_assist::Correction {
                        version: Default::default(),
                        range: 0..3,
                        edit_range: 0..3,
                        from: "gti".into(),
                        to: "git".into(),
                    }),
                })
            } else if name == "unknown" {
                Some(input_assist::Feedback {
                    text: "No executable command: not_a_known_command".into(),
                    compact: "No executable command".into(),
                    state: input_assist::State::Error,
                    correction: None,
                })
            } else {
                None
            };
            for (columns, color, unicode) in [
                (120, true, true),
                (48, true, true),
                (120, false, true),
                (120, false, false),
            ] {
                editor.columns = columns;
                let mut data = context(&editor, &keys);
                data.environment = Some((
                    "~/work/copilot-worktrees/nosh/newfuture-glowing-spoon",
                    Some("main"),
                ));
                data.approval = "";
                data.feedback = feedback.as_ref();
                data.color = color;
                data.unicode = unicode;
                let text = compose(data).text;
                assert!(style::width(&style::strip_ansi(&text)) <= usize::from(columns - 1));
                println!("{name}/{columns}/color={color}/unicode={unicode}: {text}");
            }
        }
    }
}
