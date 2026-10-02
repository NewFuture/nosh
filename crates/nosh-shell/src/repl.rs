//! Interactive REPL: the reedline editor plus the per-line pipeline of §4.2.

use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::HashSet;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use nosh_hub::tr;
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, CompletionResult, KeyCode, MenuBuilder, Prompt, PromptContext, PromptEditMode,
    PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineMenu, Signal, Suggestion,
    ValidationResult,
};

use crate::UserOutput;
use crate::backend::{BrushShell, EmbeddedShell, UserCommand};
use crate::trigger::{self, Action, Trigger, TriggerConfig};
use crate::{editing, input_assist, style, term};

/// A request for the AI, produced by the pipeline.
#[derive(Debug, Clone)]
pub struct AiRequest {
    pub trigger: Trigger,
    pub text: String,
    /// The failed command, for [`Trigger::Failed`].
    pub failed: Option<UserCommand>,
    pub user_output: Option<UserOutput>,
}

#[derive(Debug, Default)]
pub struct AiOutcome {
    /// Placed in the next input line (never executed automatically).
    pub prefill: Option<String>,
    pub exit_code: i32,
}

/// Right-hand prompt information.
#[derive(Debug, Clone, Default)]
pub struct Badge {
    pub mode: String,
    pub yolo: bool,
    pub note: Option<String>,
}

pub trait AiHandler {
    fn handle(&mut self, shell: &mut EmbeddedShell, req: AiRequest) -> AiOutcome;
    /// `ai <subcommand> …` management commands (mode, think, clear, ctx, …).
    fn builtin(&mut self, shell: &mut EmbeddedShell, args: &[String]) -> AiOutcome;
    /// Rewrite the input into a command draft, never submit it.
    fn suggest(&mut self, shell: &mut EmbeddedShell, line: &str) -> Option<String>;
    fn badge(&self) -> Badge;
    fn assistance(&self) -> Option<crate::AssistDisplay> {
        None
    }
    fn after_command(
        &mut self,
        _shell: &EmbeddedShell,
        _command: UserCommand,
        _output: Option<UserOutput>,
    ) {
    }
}

/// Handler used when AI is disabled (`NOSH_DISABLE_AI=1`).
pub struct NoAi;

impl AiHandler for NoAi {
    fn handle(&mut self, _: &mut EmbeddedShell, _: AiRequest) -> AiOutcome {
        eprintln!("{}", tr!("nosh: AI 已禁用", "nosh: AI is disabled"));
        AiOutcome {
            exit_code: 1,
            ..AiOutcome::default()
        }
    }

    fn builtin(&mut self, s: &mut EmbeddedShell, _: &[String]) -> AiOutcome {
        self.handle(
            s,
            AiRequest {
                trigger: Trigger::Builtin,
                text: String::new(),
                failed: None,
                user_output: None,
            },
        )
    }

    fn suggest(&mut self, _: &mut EmbeddedShell, _: &str) -> Option<String> {
        None
    }

    fn badge(&self) -> Badge {
        Badge::default()
    }
}

/// What to do after a user command fails (`shell.on_failure`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum OnFailure {
    #[default]
    Hint,
    Auto,
    Off,
}

impl OnFailure {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "hint" => Some(Self::Hint),
            "auto" => Some(Self::Auto),
            "off" => Some(Self::Off),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ReplConfig {
    pub status_bar: crate::status::Config,
    pub trigger: TriggerConfig,
    pub on_failure: OnFailure,
    pub command_assist: bool,
    pub editing: editing::Config,
    pub input_assist: input_assist::Config,
    pub input_abbreviations: input_assist::Abbreviations,
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            status_bar: Default::default(),
            trigger: Default::default(),
            on_failure: Default::default(),
            command_assist: true,
            editing: Default::default(),
            input_assist: Default::default(),
            input_abbreviations: Default::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GuardChoice {
    Ai,
    Run,
    Cancel,
}

/// The interactive bits of the pipeline (replaced by a script in tests).
pub trait ReplUi {
    fn guard(&mut self, line: &str) -> GuardChoice;
    fn notice(&mut self, msg: &str);
}

/// Terminal implementation of [`ReplUi`].
pub struct TermUi;

impl ReplUi for TermUi {
    fn guard(&mut self, _line: &str) -> GuardChoice {
        eprint!(
            "{} ",
            style::yellow(style::glyph(
                tr!(
                    "看起来像自然语言：↵ 交给 AI · Ctrl+E 仍按命令执行 · Esc 取消",
                    "Looks like natural language: ↵ ask AI · Ctrl+E run as command · Esc cancel"
                ),
                tr!(
                    "看起来像自然语言：Enter 交给 AI | Ctrl+E 仍按命令执行 | Esc 取消",
                    "Looks like natural language: Enter ask AI | Ctrl+E run as command | Esc cancel"
                )
            ))
        );
        let _ = std::io::stderr().flush();
        let key = term::read_key();
        eprintln!();
        match key {
            Some(k) if k.code == KeyCode::Enter => GuardChoice::Ai,
            Some(k) if term::is_ctrl(&k, 'e') => GuardChoice::Run,
            _ => GuardChoice::Cancel,
        }
    }

    fn notice(&mut self, msg: &str) {
        eprintln!("{msg}");
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LineOutcome {
    /// Keep going; optionally prefill the next input line.
    Continue(Option<String>),
    Exit(i32),
}

const HANDLER_SUBCOMMANDS: &[&str] = &[
    "mode", "think", "clear", "ctx", "status", "out", "history", "private", "undo", "model", "next",
];

/// Decides what to do with each input line and runs it.
pub struct Pipeline {
    pub cfg: ReplConfig,
    auto_paused: bool,
    last_failure: Option<UserCommand>,
}

impl Pipeline {
    pub fn new(cfg: ReplConfig) -> Self {
        Self {
            cfg,
            auto_paused: false,
            last_failure: None,
        }
    }

    pub fn last_failure(&self) -> Option<&UserCommand> {
        self.last_failure.as_ref()
    }

    fn trigger_cfg(&self) -> TriggerConfig {
        let mut t = self.cfg.trigger.clone();
        if self.auto_paused {
            t.trigger_on_error = false;
        }
        t
    }

    pub fn process(
        &mut self,
        shell: &mut EmbeddedShell,
        ai: &mut dyn AiHandler,
        ui: &mut dyn ReplUi,
        line: &str,
    ) -> LineOutcome {
        if let Some(display) = ai.assistance() {
            display.invalidate();
        }
        let tc = self.trigger_cfg();
        let t = line.trim();
        if tc.ai_enabled && !tc.ai_prefix.is_empty() && t == tc.ai_prefix {
            return self.fix(shell, ai, ui);
        }
        match trigger::classify(line, shell, &tc) {
            Action::Empty => LineOutcome::Continue(None),
            Action::Execute => self.execute(shell, ai, ui, line),
            Action::Ai { trigger, text } => ask(shell, ai, trigger, text, None),
            Action::Correct {
                corrected,
                from,
                to,
            } => {
                ui.notice(&format!(
                    "{} {from} → {to}  {}",
                    style::cyan("nosh:"),
                    style::dim(tr!("（回车执行）", "(press Enter to run)"))
                ));
                LineOutcome::Continue(Some(corrected))
            }
            Action::Guard => match ui.guard(line) {
                GuardChoice::Ai => ask(shell, ai, Trigger::Hash, t.to_string(), None),
                GuardChoice::Run => self.execute(shell, ai, ui, line),
                GuardChoice::Cancel => LineOutcome::Continue(Some(t.to_string())),
            },
            Action::AiBuiltin(rest) => self.builtin(shell, ai, ui, &rest),
        }
    }

    /// Asks the AI about the last failed command (`ai fix [question]`, bare
    /// `#` or an explicit `ai fix` request).
    pub fn fix(
        &mut self,
        shell: &mut EmbeddedShell,
        ai: &mut dyn AiHandler,
        ui: &mut dyn ReplUi,
    ) -> LineOutcome {
        self.fix_with_text(shell, ai, ui, String::new())
    }

    fn fix_with_text(
        &mut self,
        shell: &mut EmbeddedShell,
        ai: &mut dyn AiHandler,
        ui: &mut dyn ReplUi,
        text: String,
    ) -> LineOutcome {
        match self.last_failure.clone() {
            Some(cmd) => ask(
                shell,
                ai,
                Trigger::Failed { exit: cmd.exit },
                text,
                Some(cmd),
            ),
            None => {
                ui.notice(&style::dim(tr!(
                    "nosh: 没有失败的命令；用 # 描述任务",
                    "nosh: no failed command; describe a task after #"
                )));
                LineOutcome::Continue(None)
            }
        }
    }

    fn builtin(
        &mut self,
        shell: &mut EmbeddedShell,
        ai: &mut dyn AiHandler,
        ui: &mut dyn ReplUi,
        rest: &str,
    ) -> LineOutcome {
        let words: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
        let quoted = rest.starts_with(['"', '\'']);
        match words.first().map(String::as_str) {
            None | Some("help") if !quoted => {
                ui.notice(&builtin_help(&self.cfg.trigger.builtin_name));
                LineOutcome::Continue(None)
            }
            Some("fix") if !quoted => self.fix_with_text(
                shell,
                ai,
                ui,
                rest.strip_prefix("fix")
                    .unwrap_or_default()
                    .trim()
                    .to_string(),
            ),
            Some("auto") if !quoted && words.len() <= 2 => {
                match words.get(1).map(String::as_str) {
                    Some("off") => self.auto_paused = true,
                    Some("on") => self.auto_paused = false,
                    _ => {}
                }
                ui.notice(&format!(
                    "auto: {}",
                    if self.auto_paused { "off" } else { "on" }
                ));
                LineOutcome::Continue(None)
            }
            Some(sub) if !quoted && words.len() <= 2 && HANDLER_SUBCOMMANDS.contains(&sub) => {
                let out = isolate(|| ai.builtin(shell, &words)).unwrap_or_default();
                LineOutcome::Continue(out.prefill)
            }
            _ => {
                let text = unquote(rest);
                if text.is_empty() {
                    return LineOutcome::Continue(None);
                }
                ask(shell, ai, Trigger::Builtin, text, None)
            }
        }
    }

    fn execute(
        &mut self,
        shell: &mut EmbeddedShell,
        ai: &mut dyn AiHandler,
        ui: &mut dyn ReplUi,
        line: &str,
    ) -> LineOutcome {
        self.last_failure = None;
        let run = shell.run_user_line(line);
        if run.exit_shell {
            return LineOutcome::Exit(run.exit_code);
        }
        if run.exit_code == 0 {
            self.last_failure = None;
            if self.cfg.command_assist
                && self.cfg.trigger.ai_enabled
                && !self.auto_paused
                && let Some(command) = shell.recent_commands().last().cloned()
            {
                let _ = isolate(|| ai.after_command(shell, command, None));
            }
            return LineOutcome::Continue(None);
        }
        let tc = self.trigger_cfg();
        if !tc.ai_enabled || !trigger::failure_is_notable(line, run.exit_code) {
            return LineOutcome::Continue(None);
        }
        let cmd = shell.recent_commands().last().cloned();
        self.last_failure = cmd.clone();
        if self.cfg.command_assist
            && self.cfg.on_failure != OnFailure::Off
            && !self.auto_paused
            && let Some(command) = cmd.clone()
        {
            let output = shell
                .last_user_output()
                .filter(|o| o.command_id == command.id)
                .cloned();
            let _ = isolate(|| ai.after_command(shell, command, output));
            if ai.assistance().is_some() {
                ui.notice(&style::dim(&format!(
                    "{} exit {} · {} fix",
                    style::glyph("✗", "x"),
                    run.exit_code,
                    style::visible_text(&self.cfg.trigger.builtin_name),
                )));
                return LineOutcome::Continue(None);
            }
        }
        let auto = ai.assistance().is_none()
            && !self.auto_paused
            && (self.cfg.on_failure == OnFailure::Auto || trigger::contains_cjk(line));
        match self.cfg.on_failure {
            OnFailure::Off => LineOutcome::Continue(None),
            _ if auto => ask(
                shell,
                ai,
                Trigger::Failed {
                    exit: run.exit_code,
                },
                String::new(),
                cmd,
            ),
            _ => {
                let code = run.exit_code;
                ui.notice(&style::dim(&format!(
                    "{} exit {code} {} {} fix",
                    style::glyph("✗", "x"),
                    style::glyph("·", "|"),
                    style::visible_text(&self.cfg.trigger.builtin_name)
                )));
                LineOutcome::Continue(None)
            }
        }
    }
}

fn ask(
    shell: &mut EmbeddedShell,
    ai: &mut dyn AiHandler,
    trigger: Trigger,
    text: String,
    failed: Option<UserCommand>,
) -> LineOutcome {
    let user_output = if matches!(&trigger, Trigger::Failed { .. }) {
        shell
            .last_user_output()
            .filter(|output| {
                failed
                    .as_ref()
                    .is_some_and(|command| command.id == output.command_id)
            })
            .cloned()
    } else {
        None
    };
    let req = AiRequest {
        trigger,
        text,
        failed,
        user_output,
    };
    let out = isolate(|| ai.handle(shell, req)).unwrap_or_default();
    LineOutcome::Continue(out.prefill)
}

/// Runs AI code so that a panic in it cannot take the shell down (§3.6).
pub fn isolate<T>(f: impl FnOnce() -> T) -> Option<T> {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        Ok(v) => Some(v),
        Err(e) => {
            let msg = e
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| e.downcast_ref::<&str>().copied())
                .unwrap_or("unknown error");
            eprintln!(
                "{}",
                style::red(&format!(
                    "{}: {msg}",
                    tr!("nosh: AI 子系统出错", "nosh: AI subsystem failed")
                ))
            );
            None
        }
    }
}

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['"', '\''] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].trim().to_string();
        }
    }
    s.to_string()
}

fn builtin_help(name: &str) -> String {
    let lines: &[(&str, &str, &str)] = &[
        ("\"<task>\"", "执行任务", "run a task"),
        (
            "mode confirm|auto|yolo",
            "切换审批模式",
            "switch approval mode",
        ),
        ("think on|off", "开关思考模式", "toggle thinking"),
        (
            "auto on|off",
            "出错时自动触发 AI",
            "auto-trigger AI on errors",
        ),
        (
            "fix [question]",
            "生成修复命令；附问题时交给 Agent 诊断",
            "suggest a fix; add a question for Agent diagnosis",
        ),
        (
            "next",
            "生成上一条成功命令的后续建议",
            "suggest a next command after success",
        ),
        (
            "out <n>",
            "查看 agent 命令的完整输出",
            "show full output of an agent command",
        ),
        ("clear", "新建对话", "start a new conversation"),
        ("ctx", "查看上下文占用", "show context usage"),
        ("status", "查看运行状态", "show status"),
    ];
    let mut s = String::new();
    for (cmd, zh, en) in lines {
        s.push_str(&format!("  {name} {cmd:<24} {}\n", tr!(*zh, *en)));
    }
    s.pop();
    s
}

pub(crate) const SUGGEST_COMMAND: &str = "__nosh_suggest__";

#[derive(Clone, Default)]
struct EditorState {
    completion: Arc<Mutex<Option<crate::status::Completion>>>,
    bindings: crate::status::Bindings,
    editing: Option<Arc<editing::Compiled>>,
    _theme_subscription: Option<crate::status::ThemeSubscription>,
}

struct RenderedPrompt {
    left: String,
    right: String,
    inline: bool,
    mode: PromptEditMode,
}

struct ReplPrompt {
    status_enabled: bool,
    status_feedback: bool,
    theme: crate::status::ThemeHandle,
    environment: Option<(String, Option<String>)>,
    approval: String,
    note: Option<String>,
    latest_command: Option<u64>,
    failed_exit: Option<i32>,
    editor: EditorState,
    rendered: RefCell<Option<RenderedPrompt>>,
    left: String,
    indicator: String,
    indicator_color: Color,
    right: String,
    right_color: Color,
    continuation: String,
    input_assist: Option<input_assist::InputAssist>,
    command_assist: Option<crate::AssistDisplay>,
}

impl ReplPrompt {
    fn build(shell: &EmbeddedShell, badge: &Badge) -> Self {
        let (left, custom) = shell.prompt();
        let ok = shell.last_exit_status() == 0;
        let environment = (!custom).then(|| (left.clone(), git_branch(&shell.cwd())));
        let (left, indicator) = if custom {
            (left, String::new())
        } else {
            let branch = environment
                .as_ref()
                .and_then(|(_, branch)| branch.as_ref())
                .map(|b| format!(" ({b})"))
                .unwrap_or_default();
            (
                format!(
                    "{}{} ",
                    style::stdout().paint("1;34", &left),
                    style::stdout().paint("2", &branch)
                ),
                style::stdout().glyph("❯ ", "> ").to_string(),
            )
        };
        let mut right = badge.mode.clone();
        if let Some(n) = &badge.note {
            if !right.is_empty() {
                right.push_str(" · ");
            }
            right.push_str(n);
        }
        Self {
            status_enabled: false,
            status_feedback: false,
            theme: crate::status::ThemeHandle::default(),
            environment,
            approval: badge.mode.clone(),
            note: badge.note.clone(),
            latest_command: shell.recent_commands().last().map(|command| command.id),
            failed_exit: None,
            editor: EditorState::default(),
            rendered: RefCell::new(None),
            left,
            indicator,
            indicator_color: if ok { Color::Green } else { Color::Red },
            right,
            right_color: if badge.yolo {
                Color::LightRed
            } else {
                Color::DarkGray
            },
            continuation: shell.continuation_prompt(),
            input_assist: None,
            command_assist: None,
        }
    }
}

impl Prompt for ReplPrompt {
    fn update_context(&self, context: PromptContext<'_>) {
        let theme = self.theme.snapshot();
        let mut feedback = self
            .input_assist
            .as_ref()
            .and_then(|assist| assist.feedback(context.buffer));
        let assistance = self
            .command_assist
            .as_ref()
            .and_then(|assist| assist.result());
        let completion = self.editor.completion.try_lock();
        let unavailable = completion
            .as_ref()
            .err()
            .map(|_| crate::status::Completion {
                input: context.buffer.to_owned(),
                cursor: context.completion_cursor,
                error: Some(tr!("补全状态暂不可用", "Completion state unavailable").into()),
                count: 0,
            });
        let completion = match &completion {
            Ok(snapshot) => snapshot.as_ref().filter(|snapshot| {
                snapshot.input == context.buffer && snapshot.cursor == context.completion_cursor
            }),
            Err(_) => unavailable.as_ref(),
        };
        let compose = |feedback: Option<&input_assist::Feedback>| {
            if !self.status_enabled {
                return crate::status::Layout::default();
            }
            crate::status::compose(crate::status::Context {
                editor: &context,
                environment: self
                    .environment
                    .as_ref()
                    .map(|(cwd, branch)| (cwd.as_str(), branch.as_deref())),
                feedback,
                completion,
                assistance: assistance.as_ref(),
                latest_command: self.latest_command,
                failed_exit: self.failed_exit,
                approval: &self.approval,
                note: self.note.as_deref(),
                bindings: self
                    .editor
                    .editing
                    .as_ref()
                    .map_or(&self.editor.bindings, |maps| {
                        maps.hints(&context.edit_mode, context.interaction)
                    }),
                color: style::stdout().color,
                color_depth: crate::status::color_depth(),
                unicode: style::stdout().unicode,
                theme,
            })
        };
        let mut layout = compose(feedback.as_ref());
        if let Some(assist) = &self.input_assist {
            assist.set_correction_enabled(self.status_enabled);
            if !assist.present_correction(&context, layout.correction_included)
                && layout.correction_included
            {
                if let Some(feedback) = &mut feedback {
                    feedback.correction = None;
                }
                layout = compose(feedback.as_ref());
            }
        }
        let inline = !layout.text.is_empty();
        let (left, right) = if inline {
            let base = if self.environment.is_some() {
                ""
            } else {
                &self.left
            };
            (
                format!("{}\n{base}", layout.text),
                if layout.note_included {
                    String::new()
                } else {
                    self.note.clone().unwrap_or_default()
                },
            )
        } else {
            let mut left = if self.left.starts_with('\n') {
                format!(" {}", self.left)
            } else {
                self.left.clone()
            };
            let command_status =
                if matches!(context.interaction, reedline::PromptInteraction::Editing) {
                    crate::assist_display::status_text(
                        assistance.as_ref(),
                        self.editor.editing.as_ref().map_or_else(
                            || self.editor.bindings.suggest_key(),
                            |maps| maps.ai_key(&context.edit_mode),
                        ),
                    )
                } else {
                    String::new()
                };
            if !command_status.is_empty() {
                left = format!("{}\n{left}", style::stdout().paint("2", &command_status));
            }
            let status =
                if let Some(error) = completion.and_then(|snapshot| snapshot.error.as_ref()) {
                    crate::status::plain(error)
                } else if self.status_feedback {
                    feedback
                        .as_ref()
                        .map(|feedback| feedback.text.clone())
                        .unwrap_or_default()
                } else {
                    self.input_assist
                        .as_ref()
                        .map(|assist| assist.status())
                        .unwrap_or_default()
                };
            if !status.is_empty() {
                let status = style::clip_line(
                    &status,
                    usize::from(context.columns.saturating_sub(1)),
                    0,
                    "...",
                );
                left = format!("{}\n{left}", style::stdout().paint("2", &status));
            }
            (left, self.right.clone())
        };
        *self.rendered.borrow_mut() = Some(RenderedPrompt {
            left,
            right,
            inline,
            mode: context.edit_mode,
        });
    }

    fn render_prompt_left(&self) -> Cow<'_, str> {
        if let Some(rendered) = self.rendered.borrow().as_ref() {
            return Cow::Owned(rendered.left.clone());
        }
        let left: Cow<'_, str> = if self.left.starts_with('\n') {
            format!(" {}", self.left).into()
        } else {
            self.left.as_str().into()
        };
        let left = if let Some(assist) = &self.command_assist {
            let status = assist.status(None);
            if status.is_empty() {
                left
            } else {
                Cow::Owned(format!("{}\n{left}", style::stdout().paint("2", &status)))
            }
        } else {
            left
        };
        if let Some(assist) = &self.input_assist {
            let status = assist.status();
            if !status.is_empty() {
                return format!("{}\n{left}", style::stdout().paint("2", &status)).into();
            }
        }
        left
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        if let Some(rendered) = self.rendered.borrow().as_ref() {
            return Cow::Owned(rendered.right.clone());
        }
        self.right.as_str().into()
    }

    fn render_prompt_indicator(&self, mode: PromptEditMode) -> Cow<'_, str> {
        match mode {
            PromptEditMode::Vi(mode) => format!("{}{}", vi_indicator(mode), self.indicator).into(),
            _ => self.indicator.as_str().into(),
        }
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        self.continuation.as_str().into()
    }

    fn render_prompt_history_search_indicator(&self, hs: PromptHistorySearch) -> Cow<'_, str> {
        if self
            .rendered
            .borrow()
            .as_ref()
            .is_some_and(|rendered| rendered.inline)
        {
            return "? ".into();
        }
        let indicator = match hs.status {
            PromptHistorySearchStatus::Passing if hs.term.is_empty() => {
                "(reverse-i-search) ".to_string()
            }
            PromptHistorySearchStatus::Passing => {
                format!("(reverse-i-search: {}) ", hs.term)
            }
            PromptHistorySearchStatus::Failing => {
                format!("(failing reverse-i-search: {}) ", hs.term)
            }
        };
        if let Some(RenderedPrompt {
            mode: PromptEditMode::Vi(mode),
            ..
        }) = self.rendered.borrow().as_ref()
        {
            format!("{}{indicator}", vi_indicator(mode.clone())).into()
        } else {
            indicator.into()
        }
    }

    fn get_prompt_color(&self) -> Color {
        Color::Default
    }

    fn get_indicator_color(&self) -> Color {
        self.indicator_color
    }

    fn get_prompt_right_color(&self) -> Color {
        self.right_color
    }

    fn get_prompt_multiline_color(&self) -> Color {
        Color::Default
    }
}

fn vi_indicator(mode: reedline::PromptViMode) -> &'static str {
    match mode {
        reedline::PromptViMode::Insert => "[I] ",
        reedline::PromptViMode::Normal => "[N] ",
        reedline::PromptViMode::Visual => "[V] ",
    }
}

/// Current git branch from `.git/HEAD` (no subprocess).
pub fn git_branch(cwd: &Path) -> Option<String> {
    let mut dir = Some(cwd);
    while let Some(d) = dir {
        let git = d.join(".git");
        let head = if git.is_dir() {
            std::fs::read_to_string(git.join("HEAD")).ok()
        } else if git.is_file() {
            let text = std::fs::read_to_string(&git).ok()?;
            let gitdir = text.strip_prefix("gitdir:")?.trim();
            let p = d.join(gitdir);
            std::fs::read_to_string(p.join("HEAD")).ok()
        } else {
            None
        };
        if let Some(h) = head {
            let h = h.trim();
            return Some(match h.strip_prefix("ref: refs/heads/") {
                Some(b) => b.to_string(),
                None => h.chars().take(7).collect(),
            });
        }
        dir = d.parent();
    }
    None
}

struct ShellCompleter {
    rt: Arc<tokio::runtime::Runtime>,
    shell: Arc<Mutex<BrushShell>>,
    input_assist: Option<input_assist::InputAssist>,
    state: Arc<Mutex<Option<crate::status::Completion>>>,
}

impl ShellCompleter {
    fn lock(&self) -> MutexGuard<'_, BrushShell> {
        self.shell.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl reedline::Completer for ShellCompleter {
    fn complete(&mut self, line: &str, pos: usize) -> CompletionResult {
        let rt = self.rt.clone();
        let mut sh = self.lock();
        let wd = sh.working_dir().to_path_buf();
        let completion = rt.block_on(sh.complete(line, pos));
        drop(sh);
        if let Some(assist) = &self.input_assist {
            assist.after_completion(&self.shell);
        }
        let c = match completion {
            Ok(completion) => completion,
            Err(error) => {
                *self.state.lock().unwrap_or_else(|error| error.into_inner()) =
                    Some(crate::status::Completion {
                        input: line.to_owned(),
                        cursor: pos,
                        error: Some(format!(
                            "{}: {}",
                            tr!("补全暂不可用", "completion unavailable"),
                            style::visible_text(&error.to_string())
                        )),
                        count: 0,
                    });
                return CompletionResult::fresh(Vec::new());
            }
        };
        let quote = open_quote(line, pos);
        let at_end = pos == line.len();
        let mut seen = HashSet::new();
        let mut out = Vec::new();
        for cand in c.candidates {
            if !seen.insert(cand.clone()) {
                continue;
            }
            let cand = postprocess(cand, &c.options, &wd, at_end, quote);
            out.push(to_suggestion(
                line,
                cand,
                c.insertion_index,
                c.delete_count,
                &c.options,
            ));
        }
        *self.state.lock().unwrap_or_else(|error| error.into_inner()) =
            Some(crate::status::Completion {
                input: line.to_owned(),
                cursor: pos,
                error: None,
                count: out.len(),
            });
        CompletionResult::fresh(out)
    }
}

fn open_quote(line: &str, pos: usize) -> Option<char> {
    let mut q = None;
    let mut esc = false;
    for (i, c) in line.char_indices() {
        if i >= pos {
            break;
        }
        if esc {
            esc = false;
        } else if let Some(open) = q {
            if c == open {
                q = None;
            }
        } else if c == '\\' {
            esc = true;
        } else if c == '\'' || c == '"' {
            q = Some(c);
        }
    }
    q
}

fn postprocess(
    mut cand: String,
    opts: &brush_core::completion::ProcessingOptions,
    wd: &Path,
    at_end: bool,
    quote: Option<char>,
) -> String {
    use brush_core::sys::fs::ends_with_path_separator;
    if opts.treat_as_filenames {
        if !ends_with_path_separator(&cand) {
            let p = Path::new(&cand);
            let abs = if p.is_absolute() {
                p.to_path_buf()
            } else {
                wd.join(p)
            };
            if abs.is_dir() {
                cand.push('/');
            }
        }
        if !opts.no_autoquote_filenames {
            let mode = match quote {
                Some('\'') => brush_core::escape::QuoteMode::SingleQuote,
                Some('"') => brush_core::escape::QuoteMode::DoubleQuote,
                _ => brush_core::escape::QuoteMode::BackslashEscape,
            };
            cand = brush_core::escape::quote_if_needed(&cand, mode).to_string();
        }
    }
    if at_end
        && !opts.no_trailing_space_at_end_of_line
        && (!opts.treat_as_filenames || !ends_with_path_separator(&cand))
    {
        cand.push(' ');
    }
    cand
}

fn to_suggestion(
    line: &str,
    mut cand: String,
    mut start: usize,
    mut delete: usize,
    opts: &brush_core::completion::ProcessingOptions,
) -> Suggestion {
    let mut style = Style::new();
    if opts.treat_as_filenames {
        if brush_core::sys::fs::ends_with_path_separator(&cand) {
            style = style.fg(Color::Green);
        }
        if start + delete <= line.len()
            && let Some(removed) = line.get(start..start + delete)
            && let Some(sep) = brush_core::sys::fs::rfind_path_separator(removed)
            && cand.starts_with(removed)
        {
            cand = cand.split_off(sep + 1);
            start += sep + 1;
            delete -= sep + 1;
        }
    }
    let append_whitespace = cand.ends_with(' ');
    if append_whitespace {
        cand.pop();
    }
    Suggestion {
        value: cand,
        style: Some(style),
        span: reedline::Span {
            start,
            end: start + delete,
        },
        append_whitespace,
        ..Suggestion::default()
    }
}

struct LineValidator {
    shell: Arc<Mutex<BrushShell>>,
    prefix: String,
}

impl reedline::Validator for LineValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        let t = line.trim_start();
        if !self.prefix.is_empty() && t.starts_with(&self.prefix) {
            return ValidationResult::Complete;
        }
        if trigger::apostrophe_prose(line.trim()) {
            return ValidationResult::Complete;
        }
        let sh = self.shell.lock().unwrap_or_else(|e| e.into_inner());
        match sh.parse_string(line.to_owned()) {
            Err(e) if trigger::is_incomplete(&e) => ValidationResult::Incomplete,
            _ => ValidationResult::Complete,
        }
    }
}

fn build_editor(
    shell: &EmbeddedShell,
    cfg: &ReplConfig,
    command_assist: Option<crate::AssistDisplay>,
) -> (Reedline, Option<input_assist::InputAssist>, EditorState) {
    let (rt, sh) = shell.shared();
    let enhanced = if cfg.editing.needs_enhanced_keyboard()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
    {
        match crossterm::terminal::supports_keyboard_enhancement() {
            Ok(enhanced) => enhanced,
            Err(error) => {
                eprintln!("nosh: keyboard capabilities unavailable: {error}");
                false
            }
        }
    } else {
        false
    };
    let (compiled, notices) = cfg
        .editing
        .compile(editing::Capabilities { enhanced }, cfg.trigger.ai_enabled);
    for notice in notices {
        eprintln!("nosh: {notice}");
    }
    let native_mode = compiled.editor();
    let mut state = EditorState {
        completion: Default::default(),
        bindings: compiled
            .hints(
                &native_mode.edit_mode(),
                reedline::PromptInteraction::Editing,
            )
            .clone(),
        editing: Some(compiled),
        _theme_subscription: None,
    };
    let menu = ColumnarMenu::default()
        .with_name("completion_menu")
        .with_marker("")
        .with_columns(10)
        .with_selected_text_style(Color::Blue.bold().reverse())
        .with_selected_match_text_style(Color::Blue.bold().reverse());
    let colors = style::stdout().color;
    let mut hinter = reedline::DefaultHinter::default();
    if colors {
        hinter = hinter.with_style(Style::new().italic().fg(Color::DarkGray));
    }
    let mut editor = Reedline::create()
        .with_ansi_colors(colors)
        .with_history(Box::new(crate::history::ShellHistory { shell: sh.clone() }))
        .with_quick_completions(true)
        .with_contextual_input(true)
        .use_kitty_keyboard_enhancement(enhanced)
        .with_menu_submit_protection(true)
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_validator(Box::new(LineValidator {
            shell: sh.clone(),
            prefix: cfg.trigger.ai_prefix.clone(),
        }))
        .with_highlighter(Box::new(NoHighlight));
    let repaint = editor.repaint_signal();
    state._theme_subscription = Some(
        cfg.status_bar
            .theme
            .on_repaint(Arc::new(move || repaint.request_repaint())),
    );
    let assist = cfg.input_assist.enabled.then(|| {
        let repaint = editor.repaint_signal();
        let columns = crossterm::terminal::size().map_or(80, |(w, _)| usize::from(w));
        input_assist::InputAssist::new(
            cfg.input_assist.worker.clone(),
            shell.input_index(),
            Arc::new(move || repaint.request_repaint()),
            columns,
        )
    });
    editor = editor.with_hinter(match &assist {
        Some(assist) => assist.hinter(hinter),
        None => Box::new(hinter),
    });
    editor = editor.with_completer(Box::new(ShellCompleter {
        rt,
        shell: sh,
        input_assist: assist.clone(),
        state: state.completion.clone(),
    }));
    let edit_mode: Box<dyn reedline::EditMode> = if let Some(assist) = &assist {
        editor = editor.with_highlighter(assist.highlighter());
        assist.edit_mode(native_mode)
    } else {
        native_mode
    };
    editor = if let Some(display) = command_assist {
        let repaint = editor.repaint_signal();
        display.on_repaint(Arc::new(move || repaint.request_repaint()));
        editor.with_edit_mode(Box::new(crate::assist_display::AssistEditMode {
            inner: edit_mode,
            display,
            completion_pending: false,
        }))
    } else {
        editor.with_edit_mode(edit_mode)
    };
    (editor, assist, state)
}

struct NoHighlight;

impl reedline::Highlighter for NoHighlight {
    fn highlight(&self, line: &str, _cursor: usize) -> reedline::StyledText {
        let mut t = reedline::StyledText::new();
        t.push((Style::new(), line.to_string()));
        t
    }
}

fn set_buffer(ed: &mut Reedline, text: &str) {
    ed.replace_buffer(text.to_owned());
}

fn read_plain_prompt(
    prompt: &ReplPrompt,
    draft: &mut String,
    validator: &LineValidator,
    editing: &editing::Compiled,
) -> std::io::Result<Signal> {
    use reedline::Validator;
    use std::io::IsTerminal;
    if !prompt.right.is_empty()
        && std::io::stdin().is_terminal()
        && std::io::stdout().is_terminal()
        && std::io::stderr().is_terminal()
    {
        eprintln!("{}", style::visible_text(&style::strip_ansi(&prompt.right)));
    }
    let mut continuation = false;
    loop {
        let label = if !continuation {
            format!("{}{}", prompt.left, prompt.indicator)
        } else {
            prompt.continuation.clone()
        };
        let label = style::strip_ansi(&label);
        let line = match term::read_plain_line(
            &style::visible_text(&label),
            draft,
            prompt.command_assist.as_ref(),
            editing,
        )? {
            Signal::Success(line) => line,
            signal => return Ok(signal),
        };
        if matches!(validator.validate(&line), ValidationResult::Complete) {
            return Ok(Signal::Success(line));
        }
        draft.push('\n');
        continuation = true;
    }
}

/// Runs the interactive shell until `exit` or Ctrl-D; returns the exit status.
pub fn run(shell: &mut EmbeddedShell, ai: &mut dyn AiHandler, cfg: ReplConfig) -> i32 {
    let _terminal = brush_core::terminal::TerminalControl::acquire().ok();
    shell.start_interactive();
    let (mut editor, input_assist, editor_state) = if style::stdout().ansi
        && std::io::stdin().is_terminal()
    {
        let (editor, assist, state) = build_editor(shell, &cfg, ai.assistance());
        (Some(editor), assist, state)
    } else {
        let (compiled, notices) = cfg.editing.compile(
            editing::Capabilities { enhanced: false },
            cfg.trigger.ai_enabled,
        );
        for notice in notices {
            eprintln!("nosh: {notice}");
        }
        eprintln!(
            "{}",
            tr!(
                "nosh: 基本终端使用简化输入；Vi、历史搜索、撤销/重做和其它高级编辑暂不可用",
                "nosh: basic terminal uses simplified input; Vi, history search, undo/redo and advanced editing are unavailable"
            )
        );
        (
            None,
            None,
            EditorState {
                editing: Some(compiled),
                ..Default::default()
            },
        )
    };
    if input_assist.is_none() || (cfg.input_assist.enabled && cfg.input_assist.worker.is_none()) {
        shell.warm_command_names();
    }
    let validator = LineValidator {
        shell: shell.shared().1,
        prefix: cfg.trigger.ai_prefix.clone(),
    };
    let mut pipeline = Pipeline::new(cfg);
    let status_supported = pipeline.cfg.status_bar.enabled && crate::status::supported();
    let mut ui = TermUi;
    let mut prefill: Option<String> = None;
    let code = loop {
        shell.pre_prompt();
        let mut prompt = ReplPrompt::build(shell, &ai.badge());
        prompt.status_enabled = status_supported && !shell.has_running_jobs();
        prompt.status_feedback = pipeline.cfg.status_bar.enabled;
        prompt.theme = pipeline.cfg.status_bar.theme.clone();
        prompt.editor = editor_state.clone();
        *editor_state
            .completion
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        prompt.failed_exit = pipeline.last_failure().map(|command| command.exit);
        if !pipeline.cfg.trigger.ai_enabled {
            prompt.approval.clear();
            prompt.right = prompt.note.clone().unwrap_or_default();
        }
        prompt.command_assist = ai.assistance();
        if let Some(assist) = &input_assist {
            assist.prepare(
                shell.input_context(&pipeline.trigger_cfg(), &pipeline.cfg.input_abbreviations),
            );
            prompt.input_assist = Some(assist.clone());
        }
        let mut plain_draft = prefill.take().unwrap_or_default();
        if let Some(ed) = editor.as_mut()
            && !plain_draft.is_empty()
        {
            set_buffer(ed, &plain_draft);
        }
        let signal = loop {
            let signal = if let Some(ed) = editor.as_mut() {
                ed.read_line(&prompt)
            } else {
                read_plain_prompt(
                    &prompt,
                    &mut plain_draft,
                    &validator,
                    editor_state
                        .editing
                        .as_ref()
                        .expect("compiled basic-input bindings"),
                )
            };
            let Ok(Signal::HostCommand(command)) = &signal else {
                break signal;
            };
            if editor.is_some() {
                eprintln!();
            }
            match command.as_str() {
                editing::FOCUS_NOTICE => ui.notice(tr!(
                    "nosh: 请先退出或取消当前搜索、菜单、选区或 Vi 待完成操作，再请求 AI",
                    "nosh: exit or cancel search, menu, visual selection or pending Vi input before requesting AI"
                )),
                editing::VI_LIMIT_NOTICE => ui.notice(tr!(
                    "nosh: Vi 操作超限（最多 1024 次重复、64 字符序列）；未修改输入",
                    "nosh: Vi input limit exceeded (1024 repetitions, 64 sequence characters); draft unchanged"
                )),
                editing::EDITOR_NOTICE => ui.notice(tr!(
                    "nosh: 外部编辑器未配置；此动作暂不可用",
                    "nosh: external editor is not configured; action unavailable"
                )),
                editing::PLAIN_NOTICE => ui.notice(tr!(
                    "nosh: 基本终端不支持此编辑动作",
                    "nosh: this editing action is unavailable in a basic terminal"
                )),
                SUGGEST_COMMAND | editing::COMPLETION_AI_COMMAND => {
                    let buf = editor.as_ref().map_or_else(
                        || plain_draft.clone(),
                        |ed| ed.current_buffer_contents().to_owned(),
                    );
                    if command == editing::COMPLETION_AI_COMMAND {
                        let snapshot = editor_state.completion.try_lock();
                        let cursor = editor.as_ref().map(|ed| ed.current_completion_point());
                        let result = snapshot.as_ref().ok().and_then(|snapshot| snapshot.as_ref())
                            .filter(|snapshot| snapshot.input == buf && Some(snapshot.cursor) == cursor);
                        if let Some(error) = result.and_then(|snapshot| snapshot.error.as_ref()) {
                            ui.notice(error);
                            continue;
                        }
                        if result.is_none() {
                            ui.notice(tr!(
                                "nosh: 补全结果暂不可确认；未请求 AI",
                                "nosh: completion result unavailable; AI was not requested"
                            ));
                            continue;
                        }
                    }
                    let needs_model = pipeline.cfg.trigger.ai_enabled && !buf.trim().is_empty();
                    if needs_model && let Some(assist) = &input_assist {
                        assist.suspend();
                    }
                    if let Some(program) = suggest_draft(shell, ai, &pipeline.cfg, &mut ui, &buf) {
                        if let Some(ed) = editor.as_mut() {
                            set_buffer(ed, &program);
                        } else {
                            plain_draft = program;
                        }
                    }
                    if needs_model && let Some(assist) = &input_assist {
                        assist.prepare(shell.input_context(
                            &pipeline.trigger_cfg(),
                            &pipeline.cfg.input_abbreviations,
                        ));
                    }
                }
                _ => ui.notice(&format!("nosh: unknown editor action: {}", style::visible_text(command))),
            }
        };
        if let Some(assist) = &input_assist {
            assist.suspend();
        }
        match signal {
            Ok(Signal::Success(line)) => {
                if !line.trim().is_empty() {
                    shell.add_history(&line);
                }
                match pipeline.process(shell, ai, &mut ui, &line) {
                    LineOutcome::Continue(p) => prefill = p,
                    LineOutcome::Exit(c) => break c,
                }
            }
            Ok(Signal::CtrlC) => shell.set_last_exit_status(130),
            Ok(Signal::CtrlD) => {
                eprintln!("exit");
                break shell.last_exit_status();
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("nosh: {e}");
                break 1;
            }
        }
    };
    shell.end_interactive();
    code
}

fn suggest_draft(
    shell: &mut EmbeddedShell,
    ai: &mut dyn AiHandler,
    cfg: &ReplConfig,
    ui: &mut dyn ReplUi,
    draft: &str,
) -> Option<String> {
    if !cfg.trigger.ai_enabled {
        ui.notice(tr!("nosh: AI 已禁用", "nosh: AI is disabled"));
        return None;
    }
    if draft.trim().is_empty() {
        if let Some(display) = ai.assistance()
            && let Some(crate::Assistance::Command {
                command_id,
                program,
                ..
            }) = display.result()
        {
            if shell
                .recent_commands()
                .last()
                .is_some_and(|command| command.id == command_id)
                && crate::trigger::is_suggestion_program(&program, shell)
            {
                display.invalidate();
                return Some(program);
            }
            ui.notice(tr!(
                "nosh: 建议已过期或无效；保留原输入",
                "nosh: suggestion is stale or invalid; draft unchanged"
            ));
            return None;
        }
        ui.notice(tr!(
            "nosh: 没有可采用的建议；输入请求或使用 ai fix",
            "nosh: no ready suggestion; enter a request or use ai fix"
        ));
        return None;
    }
    if let Some(display) = ai.assistance() {
        display.invalidate();
    }
    let Some(program) = isolate(|| ai.suggest(shell, draft)).flatten() else {
        ui.notice(tr!(
            "nosh: 没有建议；保留原输入",
            "nosh: no suggestion; draft unchanged"
        ));
        return None;
    };
    if !crate::trigger::is_suggestion_program(&program, shell) {
        ui.notice(tr!(
            "nosh: 建议未通过校验；保留原输入",
            "nosh: suggestion failed validation; draft unchanged"
        ));
        return None;
    }
    Some(program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reedline::{EditCommand, KeyModifiers, ReedlineEvent};

    fn status_prompt(custom: bool) -> ReplPrompt {
        let mut keys = reedline::default_emacs_keybindings();
        keys.add_binding(
            KeyModifiers::NONE,
            KeyCode::Tab,
            ReedlineEvent::Menu("completion_menu".into()),
        );
        keys.add_binding(
            KeyModifiers::NONE,
            KeyCode::F(2),
            ReedlineEvent::ExecuteHostCommand(SUGGEST_COMMAND.into()),
        );
        ReplPrompt {
            status_enabled: true,
            status_feedback: true,
            theme: crate::status::ThemeHandle::default(),
            environment: (!custom).then(|| ("~/project".into(), Some("main".into()))),
            approval: "Approval: Auto".into(),
            note: None,
            latest_command: None,
            failed_exit: None,
            editor: EditorState {
                completion: Default::default(),
                bindings: crate::status::Bindings::from_editor(&keys),
                editing: None,
                _theme_subscription: None,
            },
            rendered: RefCell::new(None),
            left: if custom {
                "\x1b[32mcustom\x1b[0m\n$ ".into()
            } else {
                "~/project (main) ".into()
            },
            indicator: if custom { "".into() } else { "> ".into() },
            indicator_color: Color::Red,
            right: "Approval: Auto".into(),
            right_color: Color::DarkGray,
            continuation: "PS2> ".into(),
            input_assist: None,
            command_assist: None,
        }
    }

    #[test]
    fn theme_replacement_keeps_the_editor_and_prompt_interaction_state() {
        let directory = tempfile::tempdir().unwrap();
        let shell = EmbeddedShell::new(crate::ShellOptions {
            working_dir: Some(directory.path().into()),
            ..Default::default()
        })
        .unwrap();
        let config = ReplConfig {
            input_assist: input_assist::Config {
                enabled: false,
                worker: None,
            },
            ..Default::default()
        };
        let themes = config.status_bar.theme.clone();
        let (mut editor, _, state) = build_editor(&shell, &config, None);
        assert!(
            state._theme_subscription.is_some(),
            "theme was not wired to the editor repaint signal"
        );
        let draft = "echo '中文 e\u{301}'";
        editor.run_edit_commands(&[
            EditCommand::InsertString(draft.into()),
            EditCommand::MoveToPosition {
                position: 5,
                select: false,
            },
            EditCommand::MoveToPosition {
                position: 9,
                select: true,
            },
        ]);
        let cursor = editor.current_insertion_point();
        let selection = editor.current_selection();
        let mut prompt = status_prompt(false);
        prompt.theme = themes.clone();
        for interaction in [
            reedline::PromptInteraction::Editing,
            reedline::PromptInteraction::Menu {
                name: "completion_menu",
                count: 10,
                provisional: false,
            },
            reedline::PromptInteraction::HistorySearch {
                term: "echo",
                has_match: true,
            },
        ] {
            themes.replace(crate::status::Theme::default());
            let snapshot = || PromptContext {
                buffer: draft,
                cursor,
                completion_cursor: cursor,
                selection,
                columns: 120,
                rows: 24,
                edit_mode: PromptEditMode::Emacs,
                interaction,
            };
            prompt.update_context(snapshot());
            let plain = style::strip_ansi(&prompt.render_prompt_left());
            let theme = crate::status::Theme {
                operation: crate::status::ColorPair::new([238, 243, 248], [30, 64, 83], 231, 24)
                    .unwrap(),
                ..Default::default()
            };
            assert!(
                themes.replace(theme),
                "each interaction must exercise an actual theme change"
            );
            prompt.update_context(snapshot());
            assert_eq!(style::strip_ansi(&prompt.render_prompt_left()), plain);
            assert_eq!(editor.current_buffer_contents(), draft);
            assert_eq!(editor.current_insertion_point(), cursor);
            assert_eq!(editor.current_selection(), selection);
            assert_eq!(editor.prompt_edit_mode(), PromptEditMode::Emacs);
        }
        editor.run_edit_commands(&[
            EditCommand::MoveToEnd { select: false },
            EditCommand::InsertString(" appended".into()),
        ]);
        themes.replace(crate::status::Theme::default());
        editor.run_edit_commands(&[EditCommand::Undo]);
        assert_eq!(
            editor.current_buffer_contents(),
            draft,
            "theme replacement lost the undo history"
        );
    }

    fn prompt_context(columns: u16, rows: u16) -> PromptContext<'static> {
        PromptContext {
            buffer: "echo ok",
            cursor: 7,
            completion_cursor: 7,
            selection: None,
            columns,
            rows,
            edit_mode: PromptEditMode::Emacs,
            interaction: reedline::PromptInteraction::Editing,
        }
    }

    #[test]
    fn inline_prompt_moves_only_default_environment_and_preserves_indicators() {
        for custom in [false, true] {
            let prompt = status_prompt(custom);
            prompt.update_context(prompt_context(180, 24));
            let left = prompt.render_prompt_left();
            if custom {
                assert!(left.ends_with("\x1b[32mcustom\x1b[0m\n$ "), "{left}");
                assert!(!left.contains("~/project"));
            } else {
                assert!(left.ends_with('\n'), "{left}");
                assert_eq!(left.matches("~/project").count(), 1, "{left}");
            }
            assert!(prompt.render_prompt_right().is_empty());
            assert_eq!(prompt.get_indicator_color(), Color::Red);
            assert_eq!(prompt.render_prompt_multiline_indicator(), "PS2> ");
        }
    }

    #[test]
    fn decoration_disabled_or_unusable_restores_original_prompt() {
        for custom in [false, true] {
            let mut prompt = status_prompt(custom);
            prompt.status_enabled = false;
            prompt.update_context(prompt_context(100, 24));
            assert_eq!(prompt.render_prompt_left(), prompt.left);
            assert_eq!(prompt.render_prompt_right(), prompt.right);
            prompt.status_enabled = true;
            prompt.update_context(prompt_context(100, 2));
            assert_eq!(prompt.render_prompt_left(), prompt.left);
            assert_eq!(prompt.render_prompt_right(), prompt.right);
        }
    }

    #[test]
    fn omitted_mode_does_not_return_as_a_right_badge() {
        let prompt = status_prompt(false);
        prompt.update_context(prompt_context(35, 24));
        assert!(!prompt.render_prompt_left().contains("Approval:"));
        assert!(prompt.render_prompt_right().is_empty());
    }

    #[test]
    fn unrendered_note_is_not_lost_with_the_mode() {
        let mut prompt = status_prompt(false);
        prompt.note = Some("important context ".repeat(16));
        prompt.update_context(prompt_context(55, 24));
        assert_eq!(
            prompt.render_prompt_right(),
            prompt.note.as_deref().unwrap()
        );
    }

    #[test]
    fn command_assistance_shares_one_status_row_and_keeps_acceptance_explicit() {
        let mut prompt = status_prompt(false);
        let display = crate::AssistDisplay::default();
        let version = display.invalidate();
        assert!(display.publish(
            version,
            Some(crate::Assistance::Command {
                command_id: 19,
                intent: "next".into(),
                program: "printf safe".into(),
            })
        ));
        prompt.command_assist = Some(display.clone());
        prompt.latest_command = Some(19);
        let mut context = prompt_context(180, 24);
        context.buffer = "";
        context.cursor = 0;
        prompt.update_context(context);
        let left = prompt.render_prompt_left();
        assert!(
            left.contains("next: printf safe") && left.contains("F2"),
            "{left}"
        );
        assert_eq!(left.matches('\n').count(), 1);
        display.invalidate();
        let mut context = prompt_context(180, 24);
        context.buffer = "";
        context.cursor = 0;
        prompt.update_context(context);
        assert!(!prompt.render_prompt_left().contains("printf safe"));
        assert!(!prompt.render_prompt_left().contains("F2"));
    }

    #[test]
    fn stable_status_stays_above_input_across_feedback_and_interaction_changes() {
        let mut prompt = status_prompt(false);
        for width in [32, 48, 80, 120, 160] {
            let mut expected_environment = None;
            for (index, buffer) in ["", "echo", "gti status", "git status", ""]
                .into_iter()
                .enumerate()
            {
                *prompt.editor.completion.lock().unwrap() =
                    (index == 2).then(|| crate::status::Completion {
                        input: buffer.into(),
                        cursor: buffer.len(),
                        error: Some("completion failed ".repeat(40)),
                        count: 0,
                    });
                prompt.approval = if index % 2 == 0 {
                    "Approval: Auto".into()
                } else {
                    String::new()
                };
                let mut context = prompt_context(width, 24);
                context.buffer = buffer;
                context.cursor = buffer.len();
                prompt.update_context(context);
                let left = style::strip_ansi(&prompt.render_prompt_left());
                assert_eq!(left.matches('\n').count(), 1, "{width}/{index}: {left:?}");
                assert!(left.ends_with('\n') && !left.ends_with(&prompt.left));
                assert_eq!(
                    style::width(left.lines().next().unwrap()),
                    usize::from(width - 1)
                );
                let environment_column = left.find("~/project");
                if let Some(previous) = expected_environment {
                    assert_eq!(previous, environment_column);
                }
                expected_environment = Some(environment_column);
                assert!(prompt.render_prompt_right().is_empty());
            }
        }
    }

    #[test]
    fn unquoting() {
        assert_eq!(unquote("\"find big files\""), "find big files");
        assert_eq!(unquote("'x'"), "x");
        assert_eq!(unquote("plain words"), "plain words");
    }

    #[test]
    fn quote_detection() {
        assert_eq!(open_quote("echo 'ab", 8), Some('\''));
        assert_eq!(open_quote("echo 'ab' c", 11), None);
    }

    #[test]
    fn branch_from_head() {
        let dir = std::env::temp_dir().join(format!("nosh-git-{}", std::process::id()));
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::write(dir.join(".git/HEAD"), "ref: refs/heads/feature/x\n").unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        assert_eq!(git_branch(&dir.join("sub")).as_deref(), Some("feature/x"));
        let _ = std::fs::remove_dir_all(dir);
    }
}
