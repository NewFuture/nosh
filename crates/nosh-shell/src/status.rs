//! Pure, read-only prompt composition. Reedline owns all terminal I/O.

mod palette;
mod theme;
pub(crate) use palette::{ColorDepth, color_depth};
pub use palette::{ColorPair, InvalidColorIndex, Region, StatusPalette, StatusTone, Theme};
pub(crate) use theme::Subscription as ThemeSubscription;
pub use theme::ThemeHandle;

use std::collections::BTreeMap;

use nosh_hub::tr;
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
    Suggest,
    Cancel,
    Accept,
    Close,
    Next,
    Previous,
    Newline,
    Correct,
}

#[derive(Clone, Default)]
pub(crate) struct Bindings(BTreeMap<Action, String>);

impl Bindings {
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
            ReedlineEvent::SearchHistory => Action::History,
            ReedlineEvent::ExecuteHostCommand(name) if name == crate::repl::SUGGEST_COMMAND => {
                Action::Suggest
            }
            ReedlineEvent::CtrlC => Action::Cancel,
            ReedlineEvent::Enter => Action::Accept,
            ReedlineEvent::Esc => Action::Close,
            ReedlineEvent::MenuNext => Action::Next,
            ReedlineEvent::MenuPrevious => Action::Previous,
            ReedlineEvent::Edit(commands)
                if commands.as_slice() == [EditCommand::InsertNewline] =>
            {
                Action::Newline
            }
            ReedlineEvent::UntilFound(events) => {
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
        let compact = if action == Action::Cancel && key == "Ctrl+C" {
            format!("^C {text}")
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
        field.correction = action == Action::Correct;
        if action == Action::Cancel {
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
        KeyCode::Char(c) => c.to_ascii_uppercase().to_string(),
        KeyCode::Enter => "Enter".into(),
        KeyCode::Tab | KeyCode::BackTab => "Tab".into(),
        KeyCode::Esc => "Esc".into(),
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
}

pub(crate) struct Context<'a> {
    pub editor: &'a PromptContext<'a>,
    pub environment: Option<(&'a str, Option<&'a str>)>,
    pub feedback: Option<&'a input_assist::Feedback>,
    pub completion: Option<&'a Completion>,
    pub assistance: Option<&'a Assistance>,
    pub latest_command: Option<u64>,
    pub failed_exit: Option<i32>,
    pub approval: &'a str,
    pub note: Option<&'a str>,
    pub bindings: &'a Bindings,
    pub color: bool,
    pub color_depth: ColorDepth,
    pub unicode: bool,
    pub theme: Theme,
}

#[derive(Default)]
pub(crate) struct Layout {
    pub text: String,
    pub note_included: bool,
    pub correction_included: bool,
}

struct Field {
    zone: u8,
    variants: [String; 3],
    priority: u8,
    required: bool,
    note: bool,
    correction: bool,
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
    let columns = usize::from(editor.columns.saturating_sub(1));
    let slots = Slots::new(columns);
    let mut fields = Vec::new();
    if let Some((cwd, branch)) = context.environment {
        let budget = slots.widths[0].saturating_sub(3);
        let mut environment = path_summary(cwd, budget.min(40));
        if let Some(branch) = branch.filter(|branch| !branch.is_empty()) {
            let branch = format!("({})", plain(branch));
            if style::width(&environment) + 1 + style::width(&branch) <= budget {
                environment.push(' ');
                environment.push_str(&branch);
            }
        }
        fields.push(Field::new(
            0,
            &environment,
            &environment,
            &environment,
            80,
            false,
        ));
    }
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
        PromptInteraction::HistorySearch { term, has_match } => {
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
            if has_match {
                add_action(&mut fields, Action::Accept, tr!("采用", "accept"), 98, true);
            }
            add_action(&mut fields, Action::Close, tr!("退出", "exit"), 99, true);
            add_action(
                &mut fields,
                Action::History,
                tr!("继续搜索", "search again"),
                85,
                false,
            );
        }
        PromptInteraction::Menu {
            count, provisional, ..
        } => {
            let error = context
                .completion
                .and_then(|completion| completion.error.as_deref());
            let label = if error.is_some() {
                tr!("补全失败", "Completion failed")
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
                } else if provisional {
                    tr!("查询中", "Pending")
                } else if count == 0 {
                    tr!("无候选", "No match")
                } else {
                    tr!("补全", "Menu")
                },
                if error.is_some() {
                    Tone::Error
                } else if provisional {
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
            if count > 0 && !provisional && error.is_none() {
                add_action(&mut fields, Action::Accept, tr!("采用", "accept"), 98, true);
            }
            add_action(&mut fields, Action::Close, tr!("关闭", "close"), 99, true);
            if count > 1 && !provisional && error.is_none() {
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
                completion.input == editor.buffer && completion.cursor == editor.cursor
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
            } else if completion.is_some_and(|completion| completion.count == 0) {
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
                    tr!("命令建议可用", "Suggestion ready"),
                    Tone::Advice,
                );
            } else if let Some(Assistance::Message(message)) = context.assistance.filter(|_| blank)
            {
                state(
                    &mut fields,
                    message,
                    &style::clip_line(&plain(message), 28, 0, "..."),
                    Tone::Secondary,
                );
            } else if blank && let Some(exit) = context.failed_exit {
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
            } else if blank && context.failed_exit.is_some() {
                add_action(&mut fields, Action::Suggest, tr!("修复", "fix"), 97, true);
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
        let mut field = Field::new(
            1,
            note,
            &style::clip_line(&plain(note), 24, 0, "..."),
            &style::clip_line(&plain(note), 24, 0, "..."),
            96,
            false,
        );
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
        fields.push(Field::new(3, context.approval, compact, compact, 20, false));
    }
    render(
        &fields,
        columns,
        context.color,
        context.color_depth,
        context.unicode,
        &context.theme,
    )
}

struct Slots {
    widths: [usize; 4],
}

impl Slots {
    fn new(columns: usize) -> Self {
        let widths = match columns {
            0..=15 => [0; 4],
            16..=31 => [0, columns, 0, 0],
            32..=55 => [0, columns - 24, 24, 0],
            56..=75 => [16, columns - 44, 28, 0],
            76..=111 => [31, columns - 67, 24, 12],
            _ => {
                let environment = (columns / 3).clamp(31, 42);
                [environment, columns - environment - 44, 28, 16]
            }
        };
        Self { widths }
    }
}

struct Zone {
    text: String,
    tone: Tone,
    note_included: bool,
    correction_included: bool,
    state_complete: bool,
}

fn zone(fields: &[Field], number: u8, columns: usize, allow_correction: bool, edge: bool) -> Zone {
    let budget = columns.saturating_sub(if edge { 3 } else { 2 });
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
    let mut selected: Vec<(&Field, usize, String)> = Vec::new();
    let mut used = 0;
    let mut state_complete = true;
    for field in order {
        let text = &field.variants[2];
        let separator = usize::from(!selected.is_empty());
        let width = style::width(text);
        if used + separator + width <= budget {
            selected.push((field, 2, text.clone()));
            used += separator + width;
        } else if selected.is_empty() && !matches!(number, 2 | 3) && budget > 0 {
            let clipped = style::clip_line(text, budget, 0, "...");
            used = style::width(&clipped);
            selected.push((field, 2, clipped));
            state_complete = !field.required;
        }
    }
    // Keep the main action at the slot start and expand it before secondary cancel.
    if number == 2 {
        selected.sort_by_key(|(field, _, _)| {
            (
                matches!(field.tone, Tone::Secondary),
                std::cmp::Reverse(field.priority),
            )
        });
    }
    for (field, level, text) in &mut selected {
        for next in [1, 0] {
            let width = style::width(&field.variants[next]);
            if used - style::width(text) + width <= budget {
                used = used - style::width(text) + width;
                *level = next;
                text.clone_from(&field.variants[next]);
            }
        }
    }
    Zone {
        tone: selected
            .first()
            .map_or(Tone::Secondary, |(field, _, _)| field.tone),
        note_included: selected
            .iter()
            .any(|(field, level, text)| field.note && *level == 0 && *text == field.variants[0]),
        correction_included: selected
            .iter()
            .any(|(field, _, text)| field.correction && field.variants.contains(text)),
        text: selected
            .into_iter()
            .map(|(_, _, text)| text)
            .collect::<Vec<_>>()
            .join(" "),
        state_complete,
    }
}

fn render(
    fields: &[Field],
    columns: usize,
    color: bool,
    depth: ColorDepth,
    unicode: bool,
    theme: &Theme,
) -> Layout {
    let slots = Slots::new(columns);
    let mut layout = Layout::default();
    let mut state_complete = true;
    for (number, width) in slots.widths.into_iter().enumerate() {
        if width == 0 {
            continue;
        }
        let has_next = slots.widths[number + 1..].iter().any(|width| *width > 0);
        let zone = zone(fields, number as u8, width, state_complete, has_next);
        if number == 1 {
            state_complete = zone.state_complete;
        }
        layout.note_included |= zone.note_included;
        layout.correction_included |= zone.correction_included;
        let padding = width - style::width(&zone.text) - 2;
        let edge = if has_next {
            if unicode { '│' } else { '|' }
        } else {
            ' '
        };
        let block = format!(" {}{}{edge}", zone.text, " ".repeat(padding));
        if color && depth != ColorDepth::Plain {
            // Explicit pairs avoid the host's theme-overridable ANSI 0..15 palette.
            let sgr = theme.pair(number, zone.tone).sgr(depth);
            layout.text.push_str(&format!("{sgr}{block}\x1b[0m"));
        } else {
            layout.text.push_str(&block);
        }
    }
    layout.correction_included &= state_complete;
    layout
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
            KeyModifiers::CONTROL,
            KeyCode::Char('g'),
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
            feedback: None,
            completion: None,
            assistance: None,
            latest_command: None,
            failed_exit: None,
            approval: "Approval: Auto",
            note: None,
            bindings: keys,
            color: false,
            color_depth: ColorDepth::Rgb,
            unicode: true,
            theme: Theme::default(),
        }
    }

    fn split_zones(text: &str, columns: usize) -> [String; 4] {
        let plain = style::strip_ansi(text);
        let mut remaining = plain.as_str();
        let result = Slots::new(columns).widths.map(|width| {
            let part = style::clip_line(remaining, width, 0, "");
            assert_eq!(style::width(&part), width, "{width}: {part:?}");
            remaining = &remaining[part.len()..];
            part
        });
        assert!(remaining.is_empty(), "unexpected columns: {remaining:?}");
        result
    }

    fn contents(slot: &str) -> &str {
        slot.trim().trim_end_matches(['│', '|']).trim_end()
    }

    #[test]
    fn zones_have_stable_order_and_reserve_empty_slots() {
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
        let zones = split_zones(&text, 179);
        assert_eq!(contents(&zones[0]), "~/work/nosh (main)");
        assert_eq!(contents(&zones[1]), "! example diagnostic");
        assert!(zones[2].contains("^C cancel"), "{text}");
        assert_eq!(zones[3].trim(), "Approval: Auto");
        let text = compose(context(&editor, &keys)).text;
        assert!(contents(&split_zones(&text, 179)[1]).is_empty(), "{text:?}");
        assert_eq!(style::width(&text), 179);
        assert!(!text.contains("READY"));
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
    fn mode_never_shrinks_or_displaces_other_zones() {
        let keys = bindings();
        let mut editor = editor("echo hello");
        for width in 1..=160 {
            editor.columns = width;
            let full = compose(context(&editor, &keys)).text;
            let mut data = context(&editor, &keys);
            data.approval = "";
            let without = compose(data).text;
            let columns = usize::from(width.saturating_sub(1));
            let full = split_zones(&full, columns);
            let without = split_zones(&without, columns);
            assert_eq!(full[..3], without[..3], "{width}");
            assert!(contents(&without[3]).is_empty());
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
        assert!(text.contains("^C cancel"), "{text}");
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
    fn ctrl_g_depends_on_blank_input_failure_and_current_background_candidate() {
        let keys = bindings();
        for buffer in ["", "   "] {
            let editor = editor(buffer);
            assert!(!compose(context(&editor, &keys)).text.contains("Ctrl+G"));
            let mut data = context(&editor, &keys);
            data.failed_exit = Some(7);
            let text = compose(data).text;
            assert!(text.contains("Ctrl+G") && text.contains('7'), "{text}");
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
                text.contains("next: printf safe") && text.contains("Ctrl+G"),
                "{text}"
            );
            let mut data = context(&editor, &keys);
            data.assistance = Some(&candidate);
            data.latest_command = Some(10);
            assert!(!compose(data).text.contains("printf safe"));
        }
        let editor = editor("describe this task");
        let text = compose(context(&editor, &keys)).text;
        assert!(text.contains("Tab") && !text.contains("Ctrl+G"), "{text}");
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
    fn zero_width_text_still_reserves_its_separators() {
        let fields = [
            Field::new(0, "~", "~", "~", 80, false),
            Field::new(1, "\u{301}", "\u{301}", "\u{301}", 100, true),
            Field::new(2, "Esc exit", "Esc exit", "Esc exit", 99, true),
        ];
        for columns in 0..40 {
            let layout = render(
                &fields,
                columns,
                false,
                ColorDepth::Plain,
                false,
                &Theme::default(),
            );
            assert!(
                style::width(&layout.text) <= columns,
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
        assert!(
            compose(context(&editor, &keys))
                .text
                .contains("Approval: Auto")
        );
        editor.columns = 80;
        let text = compose(context(&editor, &keys)).text;
        let zones = split_zones(&text, 79);
        assert_eq!(zones[3].trim(), "Auto");
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
        assert!(compose(data).text.contains(".../newfuture-glowing-spoon"));
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
    fn narrow_fixed_action_slot_preserves_required_exit() {
        let mut keys = bindings();
        keys.0.clear();
        keys.0.insert(Action::Close, "Esc".into());
        let label = keys.field(Action::Close, "exit", 99, true, false).unwrap();
        for columns in 32..=55 {
            let layout = render(
                std::slice::from_ref(&label),
                columns,
                false,
                ColorDepth::Plain,
                false,
                &Theme::default(),
            );
            assert!(
                !layout.text.is_empty()
                    && layout.text.contains("Esc")
                    && layout.text.contains("exit"),
                "{columns}: {}",
                layout.text
            );
            assert_eq!(style::width(&layout.text), columns);
        }
    }

    #[test]
    fn fixed_width_state_transitions_keep_full_row_and_zone_anchors() {
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
                    let mut environment = None;
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
                        let zones = split_zones(&layout.text, columns);
                        if let Some(previous) = &environment {
                            assert_eq!(previous, &zones[0]);
                        }
                        environment = Some(zones[0].clone());
                        if !contents(&zones[2]).is_empty() {
                            assert!(
                                zones[2].starts_with(' ') && !zones[2].starts_with("  "),
                                "operation shifted within its slot: {width}/{index}: {:?}",
                                zones[2]
                            );
                        }
                        if columns >= 56
                            && !buffer.trim().is_empty()
                            && feedback
                                .as_ref()
                                .is_none_or(|feedback| feedback.correction.is_none())
                        {
                            assert!(
                                zones[2].trim_start().starts_with("Tab "),
                                "ordinary action moved after asynchronous feedback: {width}/{index}: {:?}",
                                zones[2]
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn menu_counts_and_mode_visibility_do_not_move_status_or_operation_slots() {
        let keys = bindings();
        for width in [48, 80, 120, 160] {
            let mut editor = editor("cat candidate_");
            editor.columns = width;
            let mut baseline = None;
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
                    let zones = split_zones(&text, usize::from(width - 1));
                    assert!(
                        contents(&zones[1]).starts_with(if width == 80 {
                            "Menu"
                        } else {
                            "Completion"
                        }),
                        "{text}"
                    );
                    if let Some((previous_environment, previous_actions)) = &baseline {
                        assert_eq!(previous_environment, &zones[0]);
                        assert_eq!(previous_actions, &zones[2]);
                    }
                    baseline = Some((zones[0].clone(), zones[2].clone()));
                }
            }
        }
    }

    #[test]
    fn backgrounds_cover_padding_and_reset_at_every_block_boundary() {
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
            assert_eq!(text.matches("\x1b[0m").count(), 4);
            assert!(text.ends_with(" \x1b[0m"));
            assert_eq!(style::width(&style::strip_ansi(&text)), 119);
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
    fn one_column_region_edges_preserve_padding_text_anchors_and_color_fallbacks() {
        let keys = bindings();
        let mut editor = editor("echo 中文 e\u{301}");
        for width in 17..=240 {
            editor.columns = width;
            let slots = Slots::new(usize::from(width - 1));
            for unicode in [false, true] {
                for depth in [ColorDepth::Plain, ColorDepth::Indexed, ColorDepth::Rgb] {
                    for color in [false, true] {
                        let mut data = context(&editor, &keys);
                        data.color = color;
                        data.color_depth = depth;
                        data.unicode = unicode;
                        let text = compose(data).text;
                        let visible = style::strip_ansi(&text);
                        let zones = split_zones(&text, usize::from(width - 1));
                        assert_eq!(style::width(&visible), usize::from(width - 1));
                        let edge = if unicode { '│' } else { '|' };
                        assert_eq!(
                            visible
                                .chars()
                                .filter(|character| *character == edge)
                                .count(),
                            slots.widths.iter().filter(|&&columns| columns > 0).count() - 1
                        );
                        for (zone, columns) in zones.iter().zip(slots.widths) {
                            if columns == 0 {
                                continue;
                            }
                            assert!(zone.starts_with(' '));
                            if zone.ends_with(edge) {
                                assert!(
                                    zone.ends_with(&format!(" {edge}")),
                                    "text touched the region edge at {width}: {zone:?}"
                                );
                            } else {
                                assert!(zone.ends_with(' '));
                            }
                        }
                        if !color || depth == ColorDepth::Plain {
                            assert!(!text.contains('\x1b'));
                        } else {
                            assert!(text.ends_with("\x1b[0m"));
                            assert_eq!(
                                text.matches("\x1b[0m").count(),
                                slots.widths.iter().filter(|&&columns| columns > 0).count()
                            );
                        }
                    }
                }
            }
        }
        editor.columns = 80;
        let mut data = context(&editor, &keys);
        data.color = false;
        let text = compose(data).text;
        assert_eq!(style::width(text.split("Tab").next().unwrap()), 44);
        assert_eq!(style::width(text.split("Auto").next().unwrap()), 68);
    }

    #[test]
    fn narrow_insets_keep_marked_leaf_and_complete_recommendation_and_adoption_keys() {
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
                let zones = split_zones(&layout.text, 79);
                let visible = style::strip_ansi(&layout.text);
                let edge = if unicode { '│' } else { '|' };
                assert_eq!(contents(&zones[0]), ".../newfuture-glowing-spoon");
                assert_eq!(
                    contents(&zones[1]),
                    if unicode { "gti→git" } else { "gti->git" }
                );
                assert!(contents(&zones[2]).starts_with(if unicode {
                    "→ accept"
                } else {
                    "Right use"
                }));
                assert!(contents(&zones[2]).contains("^C cancel"));
                assert!(
                    layout.correction_included,
                    "narrow display lost complete candidate/adoption meaning"
                );
                for zone in &zones[..3] {
                    assert!(zone.ends_with(&format!(" {edge}")), "{zone:?}");
                }
                assert_eq!(contents(&zones[3]), "Auto");
                assert_eq!(
                    style::width(
                        visible
                            .split(if unicode { "→ accept" } else { "Right use" })
                            .next()
                            .unwrap()
                    ),
                    44
                );
                assert_eq!(style::width(visible.split("Auto").next().unwrap()), 68);
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
        let zones = split_zones(&compose(data).text, 79);
        assert_eq!(contents(&zones[1]), "! No cmd");
        assert!(contents(&zones[2]).starts_with("Tab menu"));
        assert!(contents(&zones[2]).contains("^C cancel"));
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
