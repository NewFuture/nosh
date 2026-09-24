//! Interactive REPL: the reedline editor plus the per-line pipeline of §4.2.

use std::borrow::Cow;
use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use nosh_hub::tr;
use nu_ansi_term::{Color, Style};
use reedline::{
    ColumnarMenu, CompletionResult, EditCommand, Emacs, KeyCode, KeyModifiers, MenuBuilder, Prompt,
    PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline, ReedlineEvent,
    ReedlineMenu, Signal, Suggestion, ValidationResult,
};

use crate::backend::{BrushShell, EmbeddedShell, UserCommand};
use crate::trigger::{self, Action, Trigger, TriggerConfig};
use crate::{style, term};

/// A request for the AI, produced by the pipeline.
#[derive(Debug, Clone)]
pub struct AiRequest {
    pub trigger: Trigger,
    pub text: String,
    /// The failed command, for `trigger=failed`.
    pub failed: Option<UserCommand>,
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
    /// Ctrl+G: rewrite natural language in the input line into a command.
    fn suggest(&mut self, shell: &mut EmbeddedShell, line: &str) -> Option<String>;
    fn badge(&self) -> Badge;
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

#[derive(Debug, Clone, Default)]
pub struct ReplConfig {
    pub trigger: TriggerConfig,
    pub on_failure: OnFailure,
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
            style::yellow(tr!(
                "看起来像自然语言：↵ 交给 AI · Ctrl+E 仍按命令执行 · Esc 取消",
                "Looks like natural language: ↵ ask AI · Ctrl+E run as command · Esc cancel"
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
    "mode", "think", "clear", "ctx", "status", "out", "history", "private", "undo", "model",
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

    /// Asks the AI about the last failed command (`ai fix`, bare `#`, Ctrl+G on an empty line).
    pub fn fix(
        &mut self,
        shell: &mut EmbeddedShell,
        ai: &mut dyn AiHandler,
        ui: &mut dyn ReplUi,
    ) -> LineOutcome {
        match self.last_failure.clone() {
            Some(cmd) => ask(
                shell,
                ai,
                Trigger::Failed { exit: cmd.exit },
                String::new(),
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
            Some("fix") if !quoted && words.len() == 1 => self.fix(shell, ai, ui),
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
        let run = shell.run_user_line(line);
        if run.exit_shell {
            return LineOutcome::Exit(run.exit_code);
        }
        if run.exit_code == 0 {
            self.last_failure = None;
            return LineOutcome::Continue(None);
        }
        let tc = self.trigger_cfg();
        if !tc.ai_enabled || !trigger::failure_is_notable(line, run.exit_code) {
            return LineOutcome::Continue(None);
        }
        let cmd = shell.recent_commands().last().cloned();
        self.last_failure = cmd.clone();
        let auto = !self.auto_paused
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
                ui.notice(&style::dim(&tr!(
                    format!("✗ exit {code} · Ctrl+G 或 # 交给 AI"),
                    format!("✗ exit {code} · Ctrl+G or # to ask AI")
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
    let req = AiRequest {
        trigger,
        text,
        failed,
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
        ("fix", "修复上一条失败的命令", "fix the last failed command"),
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

const SUGGEST_COMMAND: &str = "__nosh_suggest__";

struct ReplPrompt {
    left: String,
    indicator: String,
    indicator_color: Color,
    right: String,
    right_color: Color,
    continuation: String,
}

impl ReplPrompt {
    fn build(shell: &EmbeddedShell, badge: &Badge) -> Self {
        let (left, custom) = shell.prompt();
        let ok = shell.last_exit_status() == 0;
        let (left, indicator) = if custom {
            (left, String::new())
        } else {
            let branch = git_branch(&shell.cwd())
                .map(|b| format!(" ({b})"))
                .unwrap_or_default();
            (
                format!("{}{} ", style::blue_bold(&left), style::dim(&branch)),
                "❯ ".to_string(),
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
        }
    }
}

impl Prompt for ReplPrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        if self.left.starts_with('\n') {
            format!(" {}", self.left).into()
        } else {
            self.left.as_str().into()
        }
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        self.right.as_str().into()
    }

    fn render_prompt_indicator(&self, _: PromptEditMode) -> Cow<'_, str> {
        self.indicator.as_str().into()
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        self.continuation.as_str().into()
    }

    fn render_prompt_history_search_indicator(&self, hs: PromptHistorySearch) -> Cow<'_, str> {
        match hs.status {
            PromptHistorySearchStatus::Passing if hs.term.is_empty() => {
                "(reverse-i-search) ".into()
            }
            PromptHistorySearchStatus::Passing => {
                format!("(reverse-i-search: {}) ", hs.term).into()
            }
            PromptHistorySearchStatus::Failing => {
                format!("(failing reverse-i-search: {}) ", hs.term).into()
            }
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
        let Ok(c) = rt.block_on(sh.complete(line, pos)) else {
            return CompletionResult::fresh(Vec::new());
        };
        drop(sh);
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

fn build_editor(shell: &EmbeddedShell, cfg: &ReplConfig) -> Reedline {
    let (rt, sh) = shell.shared();
    let mut kb = reedline::default_emacs_keybindings();
    kb.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu("completion_menu".into()),
            ReedlineEvent::MenuNext,
            ReedlineEvent::Edit(vec![EditCommand::Complete]),
        ]),
    );
    kb.add_binding(
        KeyModifiers::SHIFT,
        KeyCode::BackTab,
        ReedlineEvent::MenuPrevious,
    );
    kb.add_binding(
        KeyModifiers::ALT,
        KeyCode::Enter,
        ReedlineEvent::Edit(vec![EditCommand::InsertNewline]),
    );
    if cfg.trigger.ai_enabled {
        kb.add_binding(
            KeyModifiers::CONTROL,
            KeyCode::Char('g'),
            ReedlineEvent::ExecuteHostCommand(SUGGEST_COMMAND.into()),
        );
    }
    let menu = ColumnarMenu::default()
        .with_name("completion_menu")
        .with_marker("")
        .with_columns(10)
        .with_selected_text_style(Color::Blue.bold().reverse())
        .with_selected_match_text_style(Color::Blue.bold().reverse());
    let colors = style::enabled();
    let mut hinter = reedline::DefaultHinter::default();
    if colors {
        hinter = hinter.with_style(Style::new().italic().fg(Color::DarkGray));
    }
    Reedline::create()
        .with_ansi_colors(colors)
        .with_history(Box::new(crate::history::ShellHistory { shell: sh.clone() }))
        .with_completer(Box::new(ShellCompleter {
            rt,
            shell: sh.clone(),
        }))
        .with_quick_completions(true)
        .with_menu(ReedlineMenu::EngineCompleter(Box::new(menu)))
        .with_validator(Box::new(LineValidator {
            shell: sh,
            prefix: cfg.trigger.ai_prefix.clone(),
        }))
        .with_hinter(Box::new(hinter))
        .with_highlighter(Box::new(NoHighlight))
        .with_edit_mode(Box::new(Emacs::new(kb)))
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
    ed.run_edit_commands(&[
        EditCommand::Clear,
        EditCommand::InsertString(text.to_string()),
    ]);
}

/// Runs the interactive shell until `exit` or Ctrl-D; returns the exit status.
pub fn run(shell: &mut EmbeddedShell, ai: &mut dyn AiHandler, cfg: ReplConfig) -> i32 {
    let _terminal = brush_core::terminal::TerminalControl::acquire().ok();
    shell.start_interactive();
    shell.warm_command_names();
    let mut editor = build_editor(shell, &cfg);
    let mut pipeline = Pipeline::new(cfg);
    let mut ui = TermUi;
    let mut prefill: Option<String> = None;
    let code = loop {
        shell.pre_prompt();
        if let Some(p) = prefill.take() {
            set_buffer(&mut editor, &p);
        }
        let prompt = ReplPrompt::build(shell, &ai.badge());
        match editor.read_line(&prompt) {
            Ok(Signal::Success(line)) => {
                if !line.trim().is_empty() {
                    shell.add_history(&line);
                }
                match pipeline.process(shell, ai, &mut ui, &line) {
                    LineOutcome::Continue(p) => prefill = p,
                    LineOutcome::Exit(c) => break c,
                }
            }
            Ok(Signal::HostCommand(cmd)) if cmd == SUGGEST_COMMAND => {
                let buf = editor.current_buffer_contents().to_string();
                eprintln!();
                if buf.trim().is_empty() {
                    set_buffer(&mut editor, "");
                    match pipeline.fix(shell, ai, &mut ui) {
                        LineOutcome::Continue(p) => prefill = p,
                        LineOutcome::Exit(c) => break c,
                    }
                } else {
                    match isolate(|| ai.suggest(shell, &buf)).flatten() {
                        Some(cmd) => set_buffer(&mut editor, &cmd),
                        None => {
                            ui.notice(&style::dim(tr!("nosh: 没有建议", "nosh: no suggestion")))
                        }
                    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
