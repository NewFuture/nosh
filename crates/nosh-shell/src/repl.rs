//! Interactive REPL: the reedline editor plus the per-line pipeline of §4.2.

use std::borrow::Cow;
use std::cell::RefCell;
use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};

use nosh_platform::tr;
use nu_ansi_term::Color;
use reedline::{
    KeyCode, Prompt, PromptContext, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus,
    Reedline, ReedlineMenu, Signal, ValidationResult,
};

use crate::UserOutput;
use crate::backend::{BrushShell, EmbeddedShell, UserCommand};
use crate::inline_commands::{self, Command};
use crate::theme::{FrameTheme, Role, Theme, ThemedHinter};
use crate::trigger::{self, Action, Trigger, TriggerConfig};
use crate::{editing, input_assist, style, term};

/// Reads only presentation configuration at a new prompt, never on an input event.
#[derive(Clone)]
pub struct ThemeLoader(pub Arc<dyn Fn() -> Result<LoadedTheme, String> + Send + Sync>);

#[derive(Clone, Copy, Debug)]
pub struct LoadedTheme {
    pub palette: Theme,
    pub builtin: Option<crate::theme::Builtin>,
}

impl std::fmt::Debug for ThemeLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThemeLoader")
    }
}

#[derive(Clone)]
pub struct ThemeCommand(pub Arc<ThemeCommandCallback>);

pub type ThemeCommandCallback =
    dyn Fn(&EmbeddedShell, Option<&str>) -> Result<String, String> + Send + Sync;

impl std::fmt::Debug for ThemeCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ThemeCommand")
    }
}

#[derive(Clone)]
pub struct EnvironmentLoader(pub Arc<EnvironmentCallback>);

pub type EnvironmentCallback =
    dyn Fn(&EmbeddedShell) -> Result<Option<crate::theme::Fragment>, String> + Send + Sync;

impl std::fmt::Debug for EnvironmentLoader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("EnvironmentLoader")
    }
}

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
    pub current_mode: Option<inline_commands::ApprovalMode>,
    pub context: Option<ContextUsage>,
    pub yolo: bool,
    pub note: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextUsage {
    pub used: usize,
    pub max: usize,
}

pub trait AiHandler {
    fn handle(&mut self, shell: &mut EmbeddedShell, req: AiRequest) -> AiOutcome;
    fn command(
        &mut self,
        shell: &mut EmbeddedShell,
        command: crate::ManagementCommand,
    ) -> AiOutcome;
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

    fn command(&mut self, s: &mut EmbeddedShell, _: crate::ManagementCommand) -> AiOutcome {
        self.handle(
            s,
            AiRequest {
                trigger: Trigger::Hash,
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
    pub terminal_integration: crate::terminal_integration::Mode,
    pub status_bar: crate::status::Config,
    pub theme_loader: Option<ThemeLoader>,
    pub theme_command: Option<ThemeCommand>,
    pub environment_loader: Option<EnvironmentLoader>,
    pub trigger: TriggerConfig,
    pub on_failure: OnFailure,
    pub command_assist: bool,
    pub editing: editing::Config,
    pub input_assist: input_assist::Config,
    pub input_abbreviations: input_assist::Abbreviations,
    pub completion: crate::completion::Config,
}

impl Default for ReplConfig {
    fn default() -> Self {
        Self {
            terminal_integration: Default::default(),
            status_bar: Default::default(),
            theme_loader: None,
            theme_command: None,
            environment_loader: None,
            trigger: Default::default(),
            on_failure: Default::default(),
            command_assist: true,
            editing: Default::default(),
            input_assist: Default::default(),
            input_abbreviations: Default::default(),
            completion: Default::default(),
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
#[derive(Default)]
pub struct TermUi {
    pub theme: crate::theme::ThemeHandle,
}

impl ReplUi for TermUi {
    fn guard(&mut self, _line: &str) -> GuardChoice {
        eprint!(
            "{} ",
            self.theme.snapshot().styles(style::stderr()).yellow(style::glyph(
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

/// Decides what to do with each input line and runs it.
pub struct Pipeline {
    pub cfg: ReplConfig,
    terminal: crate::terminal_integration::Session,
    auto_paused: bool,
    last_failure: Option<UserCommand>,
}

impl Pipeline {
    fn styles(&self) -> crate::theme::Styles {
        self.cfg.status_bar.theme.snapshot().styles(style::stderr())
    }

    pub fn new(cfg: ReplConfig) -> Self {
        Self {
            cfg,
            terminal: Default::default(),
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
        let outcome = self.process_line(shell, ai, ui, line);
        self.terminal.finish_input();
        outcome
    }

    fn process_line(
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
        match trigger::classify(line, shell, &tc) {
            Action::Empty => LineOutcome::Continue(None),
            Action::Execute => self.execute(shell, ai, ui, line),
            Action::Ai { trigger, text } => ask(&self.terminal, shell, ai, trigger, text, None),
            Action::Correct {
                corrected,
                from,
                to,
            } => {
                ui.notice(&format!(
                    "{} {from} → {to}  {}",
                    self.styles().cyan("nosh:"),
                    self.styles()
                        .dim(tr!("（回车执行）", "(press Enter to run)"))
                ));
                LineOutcome::Continue(Some(corrected))
            }
            Action::Guard => match ui.guard(line) {
                GuardChoice::Ai => ask(
                    &self.terminal,
                    shell,
                    ai,
                    Trigger::Hash,
                    line.trim().to_string(),
                    None,
                ),
                GuardChoice::Run => self.execute(shell, ai, ui, line),
                GuardChoice::Cancel => LineOutcome::Continue(Some(line.trim().to_string())),
            },
            Action::Inline(command) => self.command(shell, ai, ui, command),
            Action::InvalidInline(error) => {
                ui.notice(&format!("nosh: {}", error.message(&tc.ai_prefix)));
                LineOutcome::Continue(None)
            }
        }
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
                &self.terminal,
                shell,
                ai,
                Trigger::Failed { exit: cmd.exit },
                text,
                Some(cmd),
            ),
            None => {
                let entry = format!(
                    "{} <task>",
                    style::visible_text(&self.cfg.trigger.ai_prefix)
                );
                ui.notice(&self.styles().dim(&tr!(
                    format!("nosh: 没有失败的命令；用 {entry} 描述任务"),
                    format!("nosh: no failed command; use {entry} to describe a task")
                )));
                LineOutcome::Continue(None)
            }
        }
    }

    fn command(
        &mut self,
        shell: &mut EmbeddedShell,
        ai: &mut dyn AiHandler,
        ui: &mut dyn ReplUi,
        command: Command,
    ) -> LineOutcome {
        match command {
            Command::Help => {
                ui.notice(&inline_commands::help(&self.cfg.trigger.ai_prefix));
                LineOutcome::Continue(None)
            }
            Command::Theme(name) => {
                let result = match &self.cfg.theme_command {
                    Some(command) => (command.0)(shell, name.as_deref()),
                    None => Err(tr!(
                        "当前宿主未提供主题选择",
                        "theme selection is unavailable in this host"
                    )
                    .into()),
                };
                match result {
                    Ok(message) => ui.notice(&message),
                    Err(error) => ui.notice(&format!("nosh: {}", style::visible_text(&error))),
                }
                LineOutcome::Continue(None)
            }
            Command::Fix(text) => self.fix_with_text(shell, ai, ui, text),
            Command::Auto(enabled) => {
                if let Some(enabled) = enabled {
                    self.auto_paused = !enabled;
                }
                ui.notice(&format!(
                    "auto: {}",
                    if self.auto_paused { "off" } else { "on" }
                ));
                LineOutcome::Continue(None)
            }
            Command::Manage(command) => {
                let _region = self.terminal.begin();
                let out = isolate(|| ai.command(shell, command)).unwrap_or_default();
                LineOutcome::Continue(out.prefill)
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
        let region = self.terminal.begin();
        let run = shell.run_user_line(line);
        region.finish(run.exit_code);
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
        let fix_hint = if self.cfg.trigger.ai_prefix.is_empty() {
            String::new()
        } else {
            format!(
                " {} {}fix",
                style::glyph("·", "|"),
                style::visible_text(&self.cfg.trigger.ai_prefix)
            )
        };
        let failure_notice = format!(
            "{} exit {}{fix_hint}",
            style::glyph("✗", "x"),
            run.exit_code
        );
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
                ui.notice(&self.styles().dim(&failure_notice));
                return LineOutcome::Continue(None);
            }
        }
        let auto = ai.assistance().is_none()
            && !self.auto_paused
            && (self.cfg.on_failure == OnFailure::Auto || trigger::contains_cjk(line));
        match self.cfg.on_failure {
            OnFailure::Off => LineOutcome::Continue(None),
            _ if auto => ask(
                &self.terminal,
                shell,
                ai,
                Trigger::Failed {
                    exit: run.exit_code,
                },
                String::new(),
                cmd,
            ),
            _ => {
                ui.notice(&self.styles().dim(&failure_notice));
                LineOutcome::Continue(None)
            }
        }
    }
}

fn ask(
    terminal: &crate::terminal_integration::Session,
    shell: &mut EmbeddedShell,
    ai: &mut dyn AiHandler,
    trigger: Trigger,
    text: String,
    failed: Option<UserCommand>,
) -> LineOutcome {
    let _region = terminal.begin();
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
                style::visible_text(&format!(
                    "{}: {msg}",
                    tr!("nosh: AI 子系统出错", "nosh: AI subsystem failed")
                ))
            );
            None
        }
    }
}

pub(crate) const SUGGEST_COMMAND: &str = "__nosh_suggest__";
const PROJECT_ENV_READY: &str = "__nosh_project_env_ready__";
pub(crate) const INLINE_HISTORY_INDICATOR: &str = "? ";

#[derive(Clone, Default)]
struct EditorState {
    completion: Arc<Mutex<Option<crate::status::Completion>>>,
    bindings: crate::status::Bindings,
    editing: Option<Arc<editing::Compiled>>,
    _theme_subscription: Option<crate::status::ThemeSubscription>,
    completion_controller: Option<crate::completion::Completion>,
    frame_theme: FrameTheme,
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
    external_environment: Option<crate::theme::Fragment>,
    approval: String,
    note: Option<String>,
    latest_command: Option<u64>,
    failed_exit: Option<i32>,
    last_result: Option<(i32, std::time::Duration)>,
    context_usage: Option<ContextUsage>,
    editor: EditorState,
    rendered: RefCell<Option<RenderedPrompt>>,
    left: String,
    indicator: String,
    indicator_role: Role,
    right: String,
    right_role: Role,
    continuation: String,
    input_assist: Option<input_assist::InputAssist>,
    command_assist: Option<crate::AssistDisplay>,
    inline_prefix: Option<String>,
}

impl ReplPrompt {
    fn refresh_environment(&mut self, shell: &EmbeddedShell, badge: &Badge) {
        let mut prompt = Self::build(shell, badge);
        if prompt.environment.is_some() {
            prompt.external_environment = self.external_environment.clone();
        }
        *self = Self {
            status_enabled: self.status_enabled,
            status_feedback: self.status_feedback,
            theme: self.theme.clone(),
            editor: self.editor.clone(),
            failed_exit: self.failed_exit,
            input_assist: self.input_assist.clone(),
            command_assist: self.command_assist.clone(),
            inline_prefix: self.inline_prefix.clone(),
            ..prompt
        };
    }

    fn fallback_left(&self) -> String {
        if let Some(environment) = &self.external_environment {
            format!(
                "{} ",
                environment.render(4096, self.editor.frame_theme.styles())
            )
        } else if let Some((cwd, branch)) = &self.environment {
            let styles = self.editor.frame_theme.styles();
            format!(
                "{}{} ",
                styles.blue_bold(cwd),
                branch
                    .as_ref()
                    .map(|b| styles.dim(&format!(" ({b})")))
                    .unwrap_or_default()
            )
        } else {
            self.left.clone()
        }
    }

    fn build(shell: &EmbeddedShell, badge: &Badge) -> Self {
        let mut badge = badge.clone();
        if let Some(environment) = shell.project_env_label() {
            badge.note = Some(match badge.note {
                Some(note) => format!("{note} · {environment}"),
                None => environment,
            });
        }
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
                format!("{left}{branch} "),
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
            external_environment: None,
            approval: badge.mode.clone(),
            note: badge.note.clone(),
            latest_command: shell.recent_commands().last().map(|command| command.id),
            failed_exit: None,
            last_result: shell
                .recent_commands()
                .last()
                .map(|command| (command.exit, command.duration)),
            context_usage: badge.context,
            editor: EditorState::default(),
            rendered: RefCell::new(None),
            left,
            indicator,
            indicator_role: if ok { Role::Positive } else { Role::Error },
            right,
            right_role: if badge.yolo {
                Role::Warning
            } else {
                Role::Muted
            },
            continuation: shell.continuation_prompt(),
            input_assist: None,
            command_assist: None,
            inline_prefix: None,
        }
    }
}

impl Prompt for ReplPrompt {
    fn begin_paint(&self) {
        self.editor.frame_theme.latch(self.theme.snapshot());
    }

    fn update_context(&self, context: PromptContext<'_>) {
        let styles = self.editor.frame_theme.styles();
        let theme = styles.theme;
        let command_input = self
            .inline_prefix
            .as_ref()
            .is_some_and(|prefix| inline_commands::is_command(context.buffer, prefix, true));
        let mut feedback = self
            .input_assist
            .as_ref()
            .and_then(|assist| assist.feedback(context.buffer));
        let assistance = self
            .command_assist
            .as_ref()
            .filter(|_| !command_input)
            .and_then(|assist| assist.result());
        let bindings = self
            .editor
            .editing
            .as_ref()
            .map_or(&self.editor.bindings, |maps| {
                maps.hints(&context.edit_mode, context.interaction)
            });
        let command_bindings = command_input.then(|| bindings.clone().without_suggestions());
        let bindings = command_bindings.as_ref().unwrap_or(bindings);
        let completion = self.editor.completion.try_lock();
        let unavailable = completion
            .as_ref()
            .err()
            .map(|_| crate::status::Completion {
                input: context.buffer.to_owned(),
                cursor: context.completion_cursor,
                error: Some(tr!("补全状态暂不可用", "Completion state unavailable").into()),
                count: 0,
                phase: crate::completion::Phase::Unavailable,
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
                external_environment: self.external_environment.as_ref(),
                feedback,
                completion,
                assistance: assistance.as_ref(),
                latest_command: self.latest_command,
                failed_exit: self.failed_exit,
                last_result: self.last_result,
                context_usage: self.context_usage,
                approval: &self.approval,
                note: self.note.as_deref(),
                bindings,
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
            let base = self.fallback_left();
            let mut left = if base.starts_with('\n') {
                format!(" {base}")
            } else {
                base
            };
            let command_status =
                if matches!(context.interaction, reedline::PromptInteraction::Editing) {
                    crate::assist_display::status_text(assistance.as_ref(), bindings.suggest_key())
                } else {
                    String::new()
                };
            if !command_status.is_empty() {
                left = format!("{}\n{left}", styles.dim(&command_status));
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
                left = format!("{}\n{left}", styles.dim(&status));
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
        let base = self.fallback_left();
        let styles = self.editor.frame_theme.styles();
        let left: Cow<'_, str> = if base.starts_with('\n') {
            format!(" {base}").into()
        } else {
            base.into()
        };
        let left = if let Some(assist) = &self.command_assist {
            let status = assist.status(None);
            if status.is_empty() {
                left
            } else {
                Cow::Owned(format!("{}\n{left}", styles.dim(&status)))
            }
        } else {
            left
        };
        if let Some(assist) = &self.input_assist {
            let status = assist.status();
            if !status.is_empty() {
                return format!("{}\n{left}", styles.dim(&status)).into();
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
            return INLINE_HISTORY_INDICATOR.into();
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
        self.editor
            .frame_theme
            .styles()
            .style(self.indicator_role)
            .foreground
            .unwrap_or(Color::Default)
    }

    fn get_prompt_right_color(&self) -> Color {
        self.editor
            .frame_theme
            .styles()
            .style(self.right_role)
            .foreground
            .unwrap_or(Color::Default)
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

#[cfg(test)]
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

struct LineValidator {
    shell: Arc<Mutex<BrushShell>>,
    prefix: String,
    ai_enabled: bool,
}

impl reedline::Validator for LineValidator {
    fn validate(&self, line: &str) -> ValidationResult {
        if !matches!(
            inline_commands::parse(line, &self.prefix, self.ai_enabled),
            inline_commands::Input::Shell
        ) {
            return ValidationResult::Complete;
        }
        if self.ai_enabled && trigger::apostrophe_prose(line.trim()) {
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
    semantic_markers: bool,
) -> (Reedline, Option<input_assist::InputAssist>, EditorState) {
    let (_, sh) = shell.shared();
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
        completion_controller: None,
        frame_theme: FrameTheme::new(cfg.status_bar.theme.snapshot().styles(style::stdout())),
    };
    let menu = |name| crate::theme::completion_menu(name, state.frame_theme.clone());
    let colors =
        style::stdout().color && crate::status::color_depth() != crate::status::ColorDepth::Plain;
    let hinter = reedline::DefaultHinter::default();
    let mut editor = Reedline::create()
        .with_semantic_markers(semantic_markers.then(reedline::Osc133Markers::boxed))
        .with_ansi_colors(colors)
        .with_history(Box::new(crate::history::ShellHistory { shell: sh.clone() }))
        .with_quick_completions(true)
        .with_partial_completions(true)
        .with_contextual_input(true)
        .use_kitty_keyboard_enhancement(enhanced)
        .with_menu_submit_protection(true)
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu(
            "completion_menu",
        ))))
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu(
            crate::completion::INLINE_MENU,
        ))))
        .with_validator(Box::new(LineValidator {
            shell: sh.clone(),
            prefix: cfg.trigger.ai_prefix.clone(),
            ai_enabled: cfg.trigger.ai_enabled,
        }))
        .with_highlighter(Box::new(NoHighlight(state.frame_theme.clone())));
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
    let inner = match &assist {
        Some(assist) => assist.hinter(hinter),
        None => Box::new(hinter) as Box<dyn reedline::Hinter>,
    };
    editor = editor.with_hinter(Box::new(ThemedHinter {
        inner,
        frame: state.frame_theme.clone(),
    }));
    let repaint = editor.repaint_signal();
    let mut completion_config = cfg.completion.clone();
    completion_config.abbreviations = cfg.input_abbreviations.clone();
    completion_config.inline_prefix = cfg
        .trigger
        .ai_enabled
        .then(|| cfg.trigger.ai_prefix.clone())
        .filter(|prefix| !prefix.is_empty());
    let completion = crate::completion::Completion::new(
        completion_config,
        state.completion.clone(),
        Arc::new(move || repaint.request_repaint()),
    );
    state.completion_controller = Some(completion.clone());
    editor = editor.with_completer(Box::new(completion));
    let edit_mode: Box<dyn reedline::EditMode> = if let Some(assist) = &assist {
        editor = editor.with_highlighter(assist.highlighter_with_theme(state.frame_theme.clone()));
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

struct NoHighlight(FrameTheme);

impl reedline::Highlighter for NoHighlight {
    fn highlight(&self, line: &str, _cursor: usize) -> reedline::StyledText {
        let mut t = reedline::StyledText::new();
        t.push((self.0.styles().style(Role::Text), line.to_string()));
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
    let terminal = crate::terminal_integration::Session::detect(shell, cfg.terminal_integration);
    let (mut editor, input_assist, editor_state) = if style::stdout().ansi
        && std::io::stdin().is_terminal()
    {
        let (editor, assist, state) =
            build_editor(shell, &cfg, ai.assistance(), terminal.enabled());
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
    if let Some(ed) = editor.as_mut() {
        let signal = ed.repaint_signal();
        shell.set_project_env_wakeup(Arc::new(move || {
            signal.request_host_command(PROJECT_ENV_READY.into());
        }));
    }
    if input_assist.is_none() || (cfg.input_assist.enabled && cfg.input_assist.worker.is_none()) {
        shell.warm_command_names();
    }
    let validator = LineValidator {
        shell: shell.shared().1,
        prefix: cfg.trigger.ai_prefix.clone(),
        ai_enabled: cfg.trigger.ai_enabled,
    };
    let mut pipeline = Pipeline::new(cfg);
    pipeline.terminal = terminal;
    let status_supported = pipeline.cfg.status_bar.enabled && crate::status::supported();
    let mut ui = TermUi {
        theme: pipeline.cfg.status_bar.theme.clone(),
    };
    let mut prefill: Option<String> = None;
    let mut theme_error = None;
    let mut environment_error = None;
    let mut current_theme = None;
    let code = loop {
        if let Some(loader) = &pipeline.cfg.theme_loader {
            match (loader.0)() {
                Ok(theme) => {
                    theme_error = None;
                    pipeline.cfg.status_bar.theme.replace(theme.palette);
                    current_theme = theme.builtin;
                }
                Err(error) => {
                    if theme_error.as_ref() != Some(&error) {
                        eprintln!("nosh: theme unchanged: {}", style::visible_text(&error));
                    }
                    theme_error = Some(error);
                }
            }
        }
        shell.pre_prompt();
        shell.begin_project_env();
        pipeline.terminal.cwd(&shell.cwd());
        let badge = ai.badge();
        let mut prompt = ReplPrompt::build(shell, &badge);
        if prompt.environment.is_some()
            && let Some(loader) = &pipeline.cfg.environment_loader
        {
            match (loader.0)(shell) {
                Ok(environment) => {
                    prompt.external_environment = environment;
                    environment_error = None;
                }
                Err(error) => {
                    if environment_error.as_ref() != Some(&error) {
                        eprintln!(
                            "nosh: Starship environment unavailable; using native context: {}",
                            style::visible_text(&error)
                        );
                    }
                    environment_error = Some(error);
                }
            }
        }
        prompt.status_enabled = status_supported && !shell.has_running_jobs();
        prompt.status_feedback = pipeline.cfg.status_bar.enabled;
        prompt.theme = pipeline.cfg.status_bar.theme.clone();
        prompt.editor = editor_state.clone();
        prompt.inline_prefix = pipeline
            .cfg
            .trigger
            .ai_enabled
            .then(|| pipeline.cfg.trigger.ai_prefix.clone())
            .filter(|prefix| !prefix.is_empty());
        if let Some(completion) = &editor_state.completion_controller {
            completion.begin_prompt(inline_commands::Current {
                theme: current_theme,
                mode: badge.current_mode,
            });
            completion.prepare(shell);
        }
        *editor_state
            .completion
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        prompt.failed_exit = pipeline.last_failure().map(|command| command.exit);
        if !pipeline.cfg.trigger.ai_enabled {
            prompt.approval.clear();
            prompt.context_usage = None;
            prompt.right = prompt.note.clone().unwrap_or_default();
        }
        prompt.command_assist = ai.assistance().filter(|_| pipeline.cfg.trigger.ai_enabled);
        if let Some(assist) = &input_assist {
            if shell.project_env_loading() {
                assist.suspend();
            } else {
                assist.prepare(
                    shell.input_context(&pipeline.trigger_cfg(), &pipeline.cfg.input_abbreviations),
                );
            }
            prompt.input_assist = Some(assist.clone());
        }
        let mut plain_draft = prefill.take().unwrap_or_default();
        if let Some(ed) = editor.as_mut()
            && !plain_draft.is_empty()
        {
            set_buffer(ed, &plain_draft);
        }
        let mut input_interrupts;
        let signal = loop {
            input_interrupts = shell.interrupts().count();
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
            if command == PROJECT_ENV_READY {
                if shell.update_project_env_for_editor() {
                    prompt.refresh_environment(shell, &ai.badge());
                    if !pipeline.cfg.trigger.ai_enabled {
                        prompt.approval.clear();
                        prompt.context_usage = None;
                        prompt.right = prompt.note.clone().unwrap_or_default();
                    }
                    if let Some(completion) = &editor_state.completion_controller {
                        completion.prepare(shell);
                    }
                    if let Ok(mut state) = editor_state.completion.try_lock() {
                        *state = None;
                    }
                    if let Some(assist) = &input_assist {
                        assist.prepare(shell.input_context(
                            &pipeline.trigger_cfg(),
                            &pipeline.cfg.input_abbreviations,
                        ));
                    }
                }
                continue;
            }
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
                    if inline_commands::is_command(
                        &buf,
                        &pipeline.cfg.trigger.ai_prefix,
                        pipeline.cfg.trigger.ai_enabled,
                    ) {
                        let _ = suggest_draft(shell, ai, &pipeline.cfg, &mut ui, &buf);
                        continue;
                    }
                    if command == editing::COMPLETION_AI_COMMAND {
                        let snapshot = editor_state.completion.try_lock();
                        let cursor = editor.as_ref().map(|ed| ed.current_completion_point());
                        let result = snapshot.as_ref().ok().and_then(|snapshot| snapshot.as_ref())
                            .filter(|snapshot| snapshot.input == buf && Some(snapshot.cursor) == cursor);
                        if let Some(error) = result.and_then(|snapshot| snapshot.error.as_ref()) {
                            ui.notice(error);
                            continue;
                        }
                        if !result.is_some_and(|snapshot| {
                            snapshot.phase == crate::completion::Phase::Complete
                                && snapshot.count == 0
                        }) {
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
                    if needs_model {
                        if let Some(completion) = &editor_state.completion_controller {
                            completion.prepare(shell);
                        }
                        if let Ok(mut completion) = editor_state.completion.try_lock() {
                            *completion = None;
                        }
                    }
                    if needs_model && let Some(assist) = &input_assist
                        && !shell.project_env_loading()
                    {
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
                // A failed refresh is reported by the backend, but must not
                // prevent a human from fixing or leaving the project.
                let waiting_for_environment = shell.project_env_loading();
                let _environment = shell.finish_project_env();
                if waiting_for_environment && shell.interrupts().count() != input_interrupts {
                    pipeline.terminal.cancel_input();
                    shell.set_last_exit_status(130);
                    prefill = Some(line);
                    continue;
                }
                if !line.trim().is_empty() {
                    shell.add_history(&line);
                }
                match pipeline.process(shell, ai, &mut ui, &line) {
                    LineOutcome::Continue(p) => prefill = p,
                    LineOutcome::Exit(c) => break c,
                }
            }
            Ok(Signal::CtrlC) => {
                shell.cancel_project_env();
                pipeline.terminal.cancel_input();
                shell.set_last_exit_status(130);
            }
            Ok(Signal::CtrlD) => {
                shell.cancel_project_env();
                pipeline.terminal.cancel_input();
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
    shell.cancel_project_env();
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
    let input = inline_commands::parse(draft, &cfg.trigger.ai_prefix, true);
    let request = match &input {
        inline_commands::Input::Command(_) | inline_commands::Input::Error(_) => {
            if let Some(display) = ai.assistance() {
                display.invalidate();
            }
            ui.notice(tr!(
                "nosh: 内置命令不使用 AI 建议；请使用 Tab 补全或回车提交",
                "nosh: inline commands do not use AI suggestions; use Tab or Enter"
            ));
            return None;
        }
        inline_commands::Input::Task(text) => *text,
        inline_commands::Input::Shell => draft,
    };
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
            display.invalidate();
            ui.notice(tr!(
                "nosh: 建议已过期或无效；保留原输入",
                "nosh: suggestion is stale or invalid; draft unchanged"
            ));
            return None;
        }
        let message = if cfg.trigger.ai_prefix.is_empty() {
            tr!(
                "nosh: 没有可采用的建议；请输入请求",
                "nosh: no ready suggestion; enter a request"
            )
            .to_owned()
        } else {
            let fix = format!("{}fix", style::visible_text(&cfg.trigger.ai_prefix));
            tr!(
                format!("nosh: 没有可采用的建议；输入请求或使用 {fix}"),
                format!("nosh: no ready suggestion; enter a request or use {fix}")
            )
        };
        ui.notice(&message);
        return None;
    }
    if let Some(display) = ai.assistance() {
        display.invalidate();
    }
    let Some(program) = isolate(|| ai.suggest(shell, request)).flatten() else {
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

    #[test]
    fn rejected_background_candidates_are_invalidated_without_a_model_request() {
        struct CandidateAi {
            display: crate::AssistDisplay,
            requests: usize,
            suggestion: Option<String>,
        }
        impl AiHandler for CandidateAi {
            fn handle(&mut self, _: &mut EmbeddedShell, _: AiRequest) -> AiOutcome {
                self.requests += 1;
                AiOutcome::default()
            }
            fn command(&mut self, _: &mut EmbeddedShell, _: crate::ManagementCommand) -> AiOutcome {
                self.requests += 1;
                AiOutcome::default()
            }
            fn suggest(&mut self, _: &mut EmbeddedShell, text: &str) -> Option<String> {
                self.requests += 1;
                self.suggestion = Some(text.into());
                None
            }
            fn badge(&self) -> Badge {
                Badge::default()
            }
            fn assistance(&self) -> Option<crate::AssistDisplay> {
                Some(self.display.clone())
            }
        }
        struct Notices(Vec<String>);
        impl ReplUi for Notices {
            fn guard(&mut self, _: &str) -> GuardChoice {
                GuardChoice::Cancel
            }
            fn notice(&mut self, message: &str) {
                self.0.push(message.to_owned());
            }
        }
        let directory = tempfile::tempdir().unwrap();
        let mut shell = EmbeddedShell::new(crate::ShellOptions {
            working_dir: Some(directory.path().into()),
            ..Default::default()
        })
        .unwrap();
        assert_eq!(shell.run_user_line("true").exit_code, 0);
        let latest = shell.recent_commands().last().unwrap().id;
        let mut ai = CandidateAi {
            display: crate::AssistDisplay::default(),
            requests: 0,
            suggestion: None,
        };
        let mut ui = Notices(Vec::new());
        for (command_id, program) in [(latest + 1, "echo stale"), (latest, "echo '")] {
            let version = ai.display.invalidate();
            assert!(ai.display.publish(
                version,
                Some(crate::Assistance::Command {
                    command_id,
                    intent: "next".into(),
                    program: program.into(),
                })
            ));
            assert!(!ai.display.status(Some("F2")).is_empty());
            assert_eq!(
                suggest_draft(&mut shell, &mut ai, &ReplConfig::default(), &mut ui, " "),
                None
            );
            assert!(ai.display.result().is_none());
            assert!(ai.display.status(Some("F2")).is_empty());
            assert!(
                !ai.display
                    .publish(version, Some(crate::Assistance::Message("late".into())))
            );
            assert_eq!(
                suggest_draft(&mut shell, &mut ai, &ReplConfig::default(), &mut ui, ""),
                None
            );
        }
        assert_eq!(ai.requests, 0);
        assert_eq!(ui.0.len(), 4);
        for draft in [
            "#",
            "#help",
            "#fix",
            "#mode auto",
            "#mode bad",
            "#out",
            "#out 1",
            "#theme",
            "#theme soft-light",
            "#theme soft-light extra",
            "#unknown",
            "#找文件",
        ] {
            let version = ai.display.invalidate();
            assert!(ai.display.publish(
                version,
                Some(crate::Assistance::Command {
                    command_id: latest,
                    intent: "next".into(),
                    program: "echo background".into(),
                })
            ));
            assert_eq!(
                suggest_draft(&mut shell, &mut ai, &ReplConfig::default(), &mut ui, draft),
                None
            );
            assert!(ai.display.result().is_none(), "{draft}");
            assert_eq!(ai.requests, 0, "{draft}");
        }
        let no_prefix = ReplConfig {
            trigger: TriggerConfig {
                ai_prefix: String::new(),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(
            suggest_draft(&mut shell, &mut ai, &no_prefix, &mut ui, ""),
            None
        );
        assert!(!ui.0.last().unwrap().contains("fix"));
        assert_eq!(ai.requests, 0);
        suggest_draft(
            &mut shell,
            &mut ai,
            &ReplConfig::default(),
            &mut ui,
            "# 描述任务",
        );
        assert_eq!(ai.suggestion.as_deref(), Some("描述任务"));
        assert_eq!(ai.requests, 1);
    }

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
            external_environment: None,
            approval: "Approval: Auto".into(),
            note: None,
            latest_command: None,
            failed_exit: None,
            last_result: None,
            context_usage: None,
            editor: EditorState {
                completion: Default::default(),
                bindings: crate::status::Bindings::from_editor(&keys),
                editing: None,
                _theme_subscription: None,
                completion_controller: None,
                frame_theme: FrameTheme::test(),
            },
            rendered: RefCell::new(None),
            left: if custom {
                "\x1b[32mcustom\x1b[0m\n$ ".into()
            } else {
                "~/project (main) ".into()
            },
            indicator: if custom { "".into() } else { "> ".into() },
            indicator_role: Role::Error,
            right: "Approval: Auto".into(),
            right_role: Role::Muted,
            continuation: "PS2> ".into(),
            input_assist: None,
            command_assist: None,
            inline_prefix: Some("#".into()),
        }
    }

    #[test]
    fn project_environment_refresh_preserves_theme_cache_and_inline_prefix() {
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        shell.run_user_line("unset PS1");
        let mut prompt = status_prompt(false);
        let theme = crate::theme::Builtin::Light.palette();
        prompt.theme.replace(theme);
        prompt.inline_prefix = Some("?".into());
        prompt.external_environment = Some(crate::theme::Fragment::parse("CACHED_ENV"));
        prompt.refresh_environment(&shell, &Badge::default());
        assert_eq!(prompt.theme.snapshot(), theme);
        assert_eq!(prompt.inline_prefix.as_deref(), Some("?"));
        assert_eq!(
            prompt.external_environment.as_ref().unwrap().text(),
            "CACHED_ENV"
        );

        shell.run_user_line("PS1='custom> '");
        prompt.refresh_environment(&shell, &Badge::default());
        assert!(prompt.external_environment.is_none());
        assert_eq!(prompt.fallback_left(), "custom> ");
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
        let (mut editor, _, state) = build_editor(&shell, &config, None, false);
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
            prompt.begin_paint();
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
            prompt.begin_paint();
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

    #[test]
    fn theme_snapshot_is_shared_from_the_first_highlight_through_the_entire_paint() {
        use reedline::Highlighter;

        let prompt = status_prompt(false);
        let light = crate::theme::Builtin::Light.palette();
        let dark = crate::theme::Builtin::Dark.palette();
        prompt.theme.replace(light);
        prompt.begin_paint();
        let highlighter = NoHighlight(prompt.editor.frame_theme.clone());
        let first = highlighter.highlight("draft", 5);
        assert_eq!(first.buffer[0].0.foreground, Some(Color::Rgb(76, 79, 105)));

        prompt.theme.replace(dark);
        prompt.update_context(prompt_context(120, 24));
        assert_eq!(prompt.editor.frame_theme.styles().theme, light);
        let [r, g, b] = light.text_color(prompt.indicator_role).foreground_rgb();
        assert_eq!(prompt.get_indicator_color(), Color::Rgb(r, g, b));
        assert_eq!(
            highlighter.highlight("draft", 5).buffer[0].0,
            first.buffer[0].0
        );

        prompt.begin_paint();
        prompt.update_context(prompt_context(120, 24));
        assert_eq!(prompt.editor.frame_theme.styles().theme, dark);
        assert_eq!(
            highlighter.highlight("draft", 5).buffer[0].0.foreground,
            Some(Color::Rgb(205, 214, 244))
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
            assert_eq!(prompt.get_indicator_color(), Color::Rgb(243, 139, 168));
            assert_eq!(prompt.render_prompt_multiline_indicator(), "PS2> ");
        }
    }

    #[test]
    fn decoration_disabled_or_unusable_restores_original_prompt() {
        for custom in [false, true] {
            let mut prompt = status_prompt(custom);
            prompt.status_enabled = false;
            prompt.update_context(prompt_context(100, 24));
            if custom {
                assert_eq!(prompt.render_prompt_left(), prompt.left);
            } else {
                assert_eq!(style::strip_ansi(&prompt.render_prompt_left()), prompt.left);
            }
            assert_eq!(prompt.render_prompt_right(), prompt.right);
            prompt.status_enabled = true;
            prompt.update_context(prompt_context(100, 2));
            if custom {
                assert_eq!(prompt.render_prompt_left(), prompt.left);
            } else {
                assert_eq!(style::strip_ansi(&prompt.render_prompt_left()), prompt.left);
            }
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
                        phase: crate::completion::Phase::Unavailable,
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
    fn help_uses_only_latest_input_syntax() {
        let help = inline_commands::help("?");
        assert!(help.contains("?help") && help.contains("?fix") && help.contains("?theme"));
        assert!(help.starts_with("  ? <task>"));
        assert!(!help.contains("ai ") && !help.contains("?next"));
    }

    #[test]
    fn next_text_has_no_special_input_completion_rule() {
        use reedline::Validator;

        let shell = EmbeddedShell::new(Default::default()).unwrap();
        let validator = LineValidator {
            shell: shell.shared().1,
            prefix: "#".into(),
            ai_enabled: true,
        };
        for line in [
            "ai next 'unfinished",
            "ai next $(unfinished",
            "ai next inspect &&",
            "echo 'unfinished",
            "ai 'next unfinished",
        ] {
            assert!(matches!(
                validator.validate(line),
                ValidationResult::Incomplete
            ));
        }
        assert!(!inline_commands::help("#").contains("#next"));
    }

    #[test]
    fn inline_validation_obeys_disabled_ai_and_literal_shell_boundaries() {
        use reedline::Validator;
        let shell = EmbeddedShell::new(Default::default()).unwrap();
        let mut validator = LineValidator {
            shell: shell.shared().1,
            prefix: "?".into(),
            ai_enabled: true,
        };
        for line in ["?mode bad", "?fix what's \"wrong", "? explain \"unfinished"] {
            assert!(matches!(
                validator.validate(line),
                ValidationResult::Complete
            ));
        }
        assert!(matches!(
            validator.validate("echo '?mode \"unfinished"),
            ValidationResult::Incomplete
        ));
        validator.ai_enabled = false;
        assert!(matches!(
            validator.validate("? explain \"unfinished"),
            ValidationResult::Incomplete
        ));
        assert!(matches!(
            validator.validate("what's unfinished"),
            ValidationResult::Incomplete
        ));
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
