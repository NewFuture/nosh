//! [`ShellAi`]: the REPL's AI handler. Loads the engine on first use and
//! runs tasks in the shared session.

use nosh_engine::ChatEngine;
use nosh_permissions::ApprovalMode;
use nosh_platform::tr;
use nosh_shell::inline_commands::ApprovalMode as CommandApprovalMode;
use nosh_shell::{
    AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell, ManagementCommand, SuggestionOutcome,
    style,
};
use std::sync::{Arc, Mutex};

use crate::agent::{Agent, AgentConfig};
use crate::approval::ApprovalChannel;
use crate::assist_worker::{EngineState, Worker, assist_failure};
use crate::prompt::{Environment, TaskInput};
use crate::ui::{TermUi, approval_label};

pub struct LoadedEngine {
    pub engine: Box<dyn ChatEngine>,
    /// Shown by `#status`, e.g. model id and file.
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    Foreground,
    /// Use installed models only, without terminal input or output.
    Background,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EngineLoadError {
    #[error("{0}")]
    NotReady(String),
    #[error("{0}")]
    Cancelled(String),
    #[error("{0}")]
    Failed(String),
}

impl From<String> for EngineLoadError {
    fn from(reason: String) -> Self {
        Self::Failed(reason)
    }
}

impl From<&str> for EngineLoadError {
    fn from(reason: &str) -> Self {
        Self::Failed(reason.into())
    }
}

impl EngineLoadError {
    pub fn exit_code(&self) -> i32 {
        if matches!(self, Self::Cancelled(_)) {
            130
        } else {
            2
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelAvailability {
    NotLoaded,
    Ready(String),
    NotReady(String),
    Failed(String),
}

impl ModelAvailability {
    pub(crate) fn loading_error(&self) -> Option<EngineLoadError> {
        match self {
            Self::NotReady(reason) => Some(EngineLoadError::NotReady(reason.clone())),
            Self::Failed(reason) => Some(EngineLoadError::Failed(reason.clone())),
            _ => None,
        }
    }
}

/// Loads the model under the caller's interaction policy.
pub type EngineLoader = Box<dyn FnMut(LoadMode) -> Result<LoadedEngine, EngineLoadError> + Send>;

pub struct ShellAi {
    theme: nosh_shell::theme::ThemeHandle,
    foreground: Option<EngineState>,
    background: Option<Worker>,
    display: nosh_shell::AssistDisplay,
    availability: Arc<Mutex<ModelAvailability>>,
    engine_hooked: bool,
    cfg: AgentConfig,
    approval: Box<dyn ApprovalChannel>,
}

impl ShellAi {
    pub fn new(loader: EngineLoader, cfg: AgentConfig, approval: Box<dyn ApprovalChannel>) -> Self {
        let availability = Arc::new(Mutex::new(ModelAvailability::NotLoaded));
        Self {
            theme: Default::default(),
            foreground: Some(EngineState {
                theme: Default::default(),
                loader,
                agent: None,
                availability: availability.clone(),
            }),
            background: None,
            display: Default::default(),
            availability,
            engine_hooked: false,
            cfg,
            approval,
        }
    }

    pub fn with_theme(mut self, theme: nosh_shell::theme::ThemeHandle) -> Self {
        if let Some(state) = &mut self.foreground {
            state.theme = theme.clone();
        }
        self.theme = theme;
        self
    }

    pub fn mode(&self) -> ApprovalMode {
        self.cfg.mode
    }

    pub fn availability(&self) -> ModelAvailability {
        if self.background.as_ref().is_some_and(Worker::is_finished) {
            return ModelAvailability::Failed(
                "command assistance worker stopped; engine unavailable".into(),
            );
        }
        self.availability
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn reclaim(&mut self) -> Result<(), String> {
        self.display.invalidate();
        if let Some(worker) = self.background.take() {
            match worker.reclaim() {
                Ok(state) => self.foreground = Some(state),
                Err(error) => {
                    *self
                        .availability
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        ModelAvailability::Failed(error.clone());
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn agent(
        &mut self,
        shell: &EmbeddedShell,
        retry_loading: bool,
    ) -> Result<&mut Agent, EngineLoadError> {
        self.reclaim()?;
        if !retry_loading && let Some(error) = self.availability().loading_error() {
            return Err(error);
        }
        let state = self
            .foreground
            .as_mut()
            .ok_or_else(|| EngineLoadError::Failed("inference engine is unavailable".into()))?;
        if state.agent.is_none() {
            state.load(LoadMode::Foreground, &self.cfg, || {
                Environment::detect(shell)
            })?;
        }
        if !self.engine_hooked
            && let Some(agent) = &mut state.agent
        {
            let cancel = agent.engine_mut().cancel_handle();
            shell.interrupts().on_interrupt(move || cancel.cancel());
            self.engine_hooked = true;
        }
        Ok(state.agent.as_mut().expect("loaded agent"))
    }

    fn set_mode(&mut self, mode: ApprovalMode) {
        self.cfg.mode = mode;
        if let Some(a) = self
            .foreground
            .as_mut()
            .and_then(|state| state.agent.as_mut())
        {
            a.cfg.mode = mode;
        }
        if mode == ApprovalMode::Yolo {
            eprintln!(
                "{}",
                self.theme
                    .snapshot()
                    .styles(style::stderr())
                    .red_bold(&yolo_warning())
            );
        }
    }
}

pub fn yolo_warning() -> String {
    tr!(
        "YOLO：未被有效规则禁止的操作免逐次审批，包括高风险操作。用户 deny 始终优先；内置禁止仅可由有效用户白名单覆盖。不会绕过密码或外部认证。",
        "YOLO: non-prohibited operations run without per-call approval, including high-risk operations. User deny always wins; only a valid user allow rule overrides built-in prohibitions. Passwords and external authentication are not bypassed."
    )
    .to_string()
}

fn say(msg: &str) {
    eprintln!("{msg}");
}

impl AiHandler for ShellAi {
    fn handle(&mut self, shell: &mut EmbeddedShell, req: AiRequest) -> AiOutcome {
        if let Some(error) = &self.cfg.rules_error {
            eprintln!("nosh: AI execution blocked by invalid safety configuration: {error}");
            return AiOutcome {
                prefill: None,
                exit_code: 2,
            };
        }
        let show_think = self.cfg.thinking;
        let ints = shell.interrupts().count();
        let retry_loading = !matches!(
            req.trigger,
            nosh_shell::Trigger::ParseError | nosh_shell::Trigger::NotFound
        );
        if let Err(error) = self.agent(shell, retry_loading) {
            let cancelled = shell.interrupts().count() != ints;
            eprintln!(
                "{}",
                self.theme.snapshot().styles(style::stderr()).red(&format!(
                    "nosh: {}",
                    style::visible_text(&error.to_string())
                ))
            );
            return AiOutcome {
                prefill: None,
                exit_code: if cancelled { 130 } else { error.exit_code() },
            };
        }
        // Ctrl-C while the model was downloading or loading: stop here.
        if shell.interrupts().count() > ints {
            eprintln!(
                "{}",
                self.theme
                    .snapshot()
                    .styles(style::stderr())
                    .dim(tr!("已取消", "cancelled"))
            );
            return AiOutcome {
                prefill: None,
                exit_code: 130,
            };
        }
        let (Some(agent), approval) = (
            self.foreground
                .as_mut()
                .and_then(|state| state.agent.as_mut()),
            self.approval.as_mut(),
        ) else {
            return AiOutcome::default();
        };
        let mut ui = TermUi::with_theme(false, self.theme.clone());
        ui.show_think = show_think;
        let input = TaskInput {
            trigger: req.trigger,
            text: req.text,
            failed: req.failed,
            user_output: req.user_output,
            attachment: None,
        };
        let out = agent.run_task(shell, input, approval, &mut ui);
        AiOutcome {
            prefill: out.proposed,
            exit_code: out.status.exit_code(),
        }
    }

    fn command(&mut self, _: &mut EmbeddedShell, command: ManagementCommand) -> AiOutcome {
        if matches!(command, ManagementCommand::Status | ManagementCommand::Ctx) {
            let context = self.badge().context;
            if matches!(command, ManagementCommand::Status) {
                let model = match self.availability() {
                    ModelAvailability::NotLoaded => tr!(
                        "未加载（首次使用时加载）",
                        "not loaded (loads on first use)"
                    )
                    .to_owned(),
                    ModelAvailability::Ready(description) => description,
                    ModelAvailability::NotReady(reason) => {
                        format!("{}: {reason}", tr!("未就绪", "not ready"))
                    }
                    ModelAvailability::Failed(reason) => {
                        format!("{}: {reason}", tr!("加载失败", "load failed"))
                    }
                };
                say(&format!("model: {}", style::visible_text(&model)));
                say(&approval_label(self.cfg.mode));
                say(&format!(
                    "think: {}",
                    if self.cfg.thinking { "on" } else { "off" }
                ));
            }
            match context {
                Some(usage) => {
                    let percent = if matches!(command, ManagementCommand::Ctx) {
                        format!(" ({}%)", usage.used * 100 / usage.max.max(1))
                    } else {
                        String::new()
                    };
                    say(&format!(
                        "context: {} / {} tokens{percent}",
                        usage.used, usage.max
                    ));
                }
                None if matches!(command, ManagementCommand::Ctx) => {
                    say(tr!("还没有对话", "no conversation yet"))
                }
                None => {}
            }
            return AiOutcome::default();
        }
        if let Err(error) = self.reclaim() {
            eprintln!("nosh: {error}");
            return AiOutcome {
                prefill: None,
                exit_code: 2,
            };
        }
        match command {
            ManagementCommand::Mode(mode) => match mode {
                Some(mode) => {
                    let m = match mode {
                        CommandApprovalMode::Confirm => ApprovalMode::Confirm,
                        CommandApprovalMode::Auto => ApprovalMode::Auto,
                        CommandApprovalMode::Yolo => ApprovalMode::Yolo,
                    };
                    self.set_mode(m);
                    say(&approval_label(m));
                }
                None => say(&approval_label(self.cfg.mode)),
            },
            ManagementCommand::Think(enabled) => {
                if let Some(enabled) = enabled {
                    self.cfg.thinking = enabled;
                }
                if let Some(a) = self
                    .foreground
                    .as_mut()
                    .and_then(|state| state.agent.as_mut())
                    && a.cfg.thinking != self.cfg.thinking
                {
                    a.cfg.thinking = self.cfg.thinking;
                    a.reset_conversation();
                }
                say(&format!(
                    "think: {}",
                    if self.cfg.thinking { "on" } else { "off" }
                ));
            }
            ManagementCommand::Clear => {
                if let Some(a) = self
                    .foreground
                    .as_mut()
                    .and_then(|state| state.agent.as_mut())
                {
                    a.reset_conversation();
                }
                say(tr!("已开始新对话", "started a new conversation"));
            }
            ManagementCommand::Ctx | ManagementCommand::Status => unreachable!("read-only command"),
        }
        AiOutcome::default()
    }

    fn suggest(&mut self, shell: &mut EmbeddedShell, line: &str) -> SuggestionOutcome {
        use crate::command_assist::{AssistRequest, AssistResult, Intent};
        use crate::ui::AgentUi;
        let started = std::time::Instant::now();
        let cfg = self.cfg.clone();
        let mut ui = TermUi::with_theme(false, self.theme.clone());
        let request =
            match AssistRequest::capture(shell, &cfg, Intent::Generate, line.into(), None, None) {
                Ok(request) => request,
                Err(error) => {
                    return SuggestionOutcome::Failed(error.to_string());
                }
            };
        let interrupts = shell.interrupts().count();
        let loaded = self.agent(shell, true);
        if shell.interrupts().count() != interrupts {
            return SuggestionOutcome::Cancelled;
        }
        let agent = match loaded {
            Ok(agent) => agent,
            Err(EngineLoadError::NotReady(reason)) => return SuggestionOutcome::NotReady(reason),
            Err(EngineLoadError::Cancelled(_)) => return SuggestionOutcome::Cancelled,
            Err(EngineLoadError::Failed(reason)) => return SuggestionOutcome::Failed(reason),
        };
        let cancel = agent.engine_mut().cancel_handle();
        cancel.reset();
        ui.assistance();
        let result =
            crate::command_assist::run(agent.engine_mut(), &request, &cfg, &cancel, |_| true);
        ui.pause();
        if cancel.is_cancelled() || shell.interrupts().count() != interrupts {
            return SuggestionOutcome::Cancelled;
        }
        let outcome = match result {
            Ok(outcome) => outcome,
            Err(crate::command_assist::AssistError::Cancelled) => {
                return SuggestionOutcome::Cancelled;
            }
            Err(error) => return SuggestionOutcome::Failed(error.to_string()),
        };
        let u = outcome.usage;
        self.display.reset_failures();
        ui.finish(&crate::ui::TaskSummary {
            status: "completed".into(),
            steps: outcome.steps,
            secs: started.elapsed().as_secs_f64(),
            prompt_tokens: u.prompt_tokens,
            cached_tokens: u.cached_tokens,
            completion_tokens: u.completion_tokens,
            ttft_secs: u.ttft_secs,
            context_used: u.context_used,
            context_max: u.context_max,
            prefill_tps: u.prefill_tps(),
            decode_tps: u.decode_tps(),
            note: None,
        });
        match outcome.result {
            AssistResult::Command(program) => SuggestionOutcome::Command(program),
            AssistResult::NoSuggestion => SuggestionOutcome::NoSuggestion,
        }
    }

    fn badge(&self) -> Badge {
        Badge {
            mode: approval_label(self.cfg.mode),
            current_mode: Some(match self.cfg.mode {
                ApprovalMode::Confirm => CommandApprovalMode::Confirm,
                ApprovalMode::Auto => CommandApprovalMode::Auto,
                ApprovalMode::Yolo => CommandApprovalMode::Yolo,
            }),
            context: self
                .background
                .as_ref()
                .map_or_else(
                    || {
                        self.foreground
                            .as_ref()
                            .and_then(|state| state.agent.as_ref())
                            .and_then(Agent::context_usage)
                    },
                    Worker::context_usage,
                )
                .map(|(used, max)| nosh_shell::repl::ContextUsage { used, max }),
            yolo: self.cfg.mode == ApprovalMode::Yolo,
            note: None,
        }
    }

    fn assistance(&self) -> Option<nosh_shell::AssistDisplay> {
        Some(self.display.clone())
    }

    fn automatic_requests_available(&self) -> bool {
        self.availability().loading_error().is_none()
    }

    fn retry_assistance(&mut self) -> Result<(), String> {
        if self.background.as_ref().is_some_and(Worker::is_finished) {
            self.reclaim()?;
        }
        if self.foreground.is_none() && self.background.is_none() {
            return Err("inference engine is unavailable; restart nosh".into());
        }
        let mut availability = self
            .availability
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if availability.loading_error().is_some() {
            *availability = ModelAvailability::NotLoaded;
        }
        drop(availability);
        self.display.reset_failures();
        Ok(())
    }

    fn after_command(
        &mut self,
        shell: &EmbeddedShell,
        command: nosh_shell::UserCommand,
        output: Option<nosh_shell::UserOutput>,
    ) {
        use crate::command_assist::{AssistRequest, Intent};
        if !self.automatic_requests_available() {
            return;
        }
        let intent = if command.exit == 0 {
            Intent::Next
        } else {
            Intent::Fix
        };
        let mut request = match AssistRequest::capture(
            shell,
            &self.cfg,
            intent,
            String::new(),
            Some(command),
            output,
        ) {
            Ok(request) => request,
            Err(error) => {
                let version = self.display.invalidate();
                self.display.publish(version, assist_failure(&error));
                return;
            }
        };
        request.background = true;
        if self.background.is_none() {
            let Some(state) = self.foreground.take() else {
                let version = self.display.invalidate();
                self.display.publish(
                    version,
                    Some(nosh_shell::Assistance::Failure(
                        tr!("AI: 推理引擎不可用", "AI: inference engine unavailable").into(),
                    )),
                );
                return;
            };
            self.background = Some(Worker::start(state, self.display.clone()));
        }
        self.background
            .as_ref()
            .expect("assistance worker")
            .submit(request, self.cfg.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosh_shell::Trigger;

    #[test]
    fn local_management_commands_do_not_load_an_engine() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let loads = Arc::new(AtomicUsize::new(0));
        let observed = loads.clone();
        let mut ai = ShellAi::new(
            Box::new(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Err("management must not load a model".into())
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        for command in [
            ManagementCommand::Mode(None),
            ManagementCommand::Mode(Some(CommandApprovalMode::Confirm)),
            ManagementCommand::Think(Some(true)),
            ManagementCommand::Think(None),
            ManagementCommand::Clear,
            ManagementCommand::Ctx,
            ManagementCommand::Status,
        ] {
            ai.command(&mut shell, command);
            assert!(ai.badge().context.is_none());
        }
        assert_eq!(loads.load(Ordering::SeqCst), 0);
        assert_eq!(ai.mode(), ApprovalMode::Confirm);
        assert!(ai.cfg.thinking);
        assert!(ai.foreground.as_ref().unwrap().agent.is_none());
        assert_eq!(ai.availability(), ModelAvailability::NotLoaded);
    }

    #[test]
    fn suggestion_load_results_preserve_readiness_failure_and_cancellation() {
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        for (error, outcome, availability) in [
            (
                EngineLoadError::NotReady("model missing".into()),
                SuggestionOutcome::NotReady("model missing".into()),
                ModelAvailability::NotReady("model missing".into()),
            ),
            (
                EngineLoadError::Failed("invalid weights".into()),
                SuggestionOutcome::Failed("invalid weights".into()),
                ModelAvailability::Failed("invalid weights".into()),
            ),
            (
                EngineLoadError::Cancelled("download cancelled".into()),
                SuggestionOutcome::Cancelled,
                ModelAvailability::NotLoaded,
            ),
        ] {
            let mut ai = ShellAi::new(
                Box::new(move |_| Err(error.clone())),
                AgentConfig::default(),
                Box::new(crate::NoTerminal),
            );
            assert_eq!(ai.suggest(&mut shell, "describe task"), outcome);
            assert_eq!(ai.availability(), availability);
            assert!(ai.foreground.as_ref().unwrap().agent.is_none());
        }
    }

    #[test]
    fn loading_errors_do_not_probe_the_agent_environment() {
        for mode in [LoadMode::Foreground, LoadMode::Background] {
            let mut ai = ShellAi::new(
                Box::new(|_| Err(EngineLoadError::NotReady("model missing".into()))),
                AgentConfig::default(),
                Box::new(crate::NoTerminal),
            );
            let state = ai.foreground.as_mut().unwrap();
            assert_eq!(
                state.load(mode, &ai.cfg, || panic!(
                    "unready models must not probe PATH"
                )),
                Err(EngineLoadError::NotReady("model missing".into()))
            );
        }
    }

    #[test]
    fn no_suggestion_and_a_command_are_distinct_and_reuse_the_loaded_engine() {
        let mut engine = Some(nosh_engine::MockChatEngine::new(vec![
            vec![nosh_engine::mock::text("[None]")],
            vec![nosh_engine::mock::text("echo ready")],
        ]));
        let mut ai = ShellAi::new(
            Box::new(move |_| {
                Ok(LoadedEngine {
                    engine: Box::new(engine.take().expect("only one load")),
                    description: "fixture".into(),
                })
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        assert_eq!(
            ai.suggest(&mut shell, "first request"),
            SuggestionOutcome::NoSuggestion
        );
        assert_eq!(
            ai.suggest(&mut shell, "second request"),
            SuggestionOutcome::Command("echo ready".into())
        );
        assert_eq!(
            ai.availability(),
            ModelAvailability::Ready("fixture".into())
        );
        assert!(shell.recent_commands().is_empty());
    }

    fn wait_for_assistance(ai: &ShellAi) {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(3);
        while matches!(ai.display.result(), Some(nosh_shell::Assistance::Pending)) {
            assert!(Instant::now() < deadline, "assistance did not settle");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn automatic_no_suggestion_is_quiet_without_disabling_generation() {
        let engine = nosh_engine::MockChatEngine::with_responder(|_| {
            vec![nosh_engine::mock::text("[None]")]
        });
        let received = engine.received();
        let mut engine = Some(engine);
        let mut ai = ShellAi::new(
            Box::new(move |_| {
                Ok(LoadedEngine {
                    engine: Box::new(engine.take().expect("one shared engine")),
                    description: "fixture".into(),
                })
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        for expected in 1..=2 {
            shell.run_user_line("true");
            ai.after_command(
                &shell,
                shell.recent_commands().last().unwrap().clone(),
                None,
            );
            wait_for_assistance(&ai);
            assert!(ai.display.result().is_none());
            assert_eq!(received.lock().unwrap().len(), expected);
        }
    }

    #[test]
    fn automatic_load_failures_pause_until_an_explicit_retry() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        for first_error in [
            EngineLoadError::NotReady("model missing".into()),
            EngineLoadError::Failed("invalid model".into()),
        ] {
            let loads = Arc::new(AtomicUsize::new(0));
            let observed = loads.clone();
            let mut ai = ShellAi::new(
                Box::new(move |mode| {
                    assert_eq!(mode, LoadMode::Background);
                    if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                        return Err(first_error.clone());
                    }
                    Ok(LoadedEngine {
                        engine: Box::new(nosh_engine::MockChatEngine::with_responder(|_| {
                            vec![nosh_engine::mock::text("echo ready")]
                        })),
                        description: "fixture".into(),
                    })
                }),
                AgentConfig::default(),
                Box::new(crate::NoTerminal),
            );
            let mut shell = EmbeddedShell::new(Default::default()).unwrap();
            shell.run_user_line("true");
            ai.after_command(
                &shell,
                shell.recent_commands().last().unwrap().clone(),
                None,
            );
            wait_for_assistance(&ai);
            assert!(ai.availability().loading_error().is_some());
            assert!(!ai.automatic_requests_available());
            let version = ai.display.invalidate();
            for _ in 0..4 {
                shell.run_user_line("true");
                ai.after_command(
                    &shell,
                    shell.recent_commands().last().unwrap().clone(),
                    None,
                );
            }
            assert_eq!(loads.load(Ordering::SeqCst), 1);
            assert!(ai.display.result().is_none());
            assert!(
                ai.display.publish(version, None),
                "paused commands queued work"
            );
            ai.retry_assistance().unwrap();
            assert_eq!(ai.availability(), ModelAvailability::NotLoaded);
            assert!(ai.automatic_requests_available());
            ai.after_command(
                &shell,
                shell.recent_commands().last().unwrap().clone(),
                None,
            );
            wait_for_assistance(&ai);
            assert_eq!(loads.load(Ordering::SeqCst), 2);
            assert_eq!(
                ai.availability(),
                ModelAvailability::Ready("fixture".into())
            );
            assert_eq!(
                ai.suggest(&mut shell, "foreground request"),
                SuggestionOutcome::Command("echo ready".into())
            );
            assert_eq!(loads.load(Ordering::SeqCst), 2);
        }
    }

    #[test]
    fn implicit_requests_cannot_retry_a_failure_discovered_during_reclaim() {
        use std::sync::mpsc;
        use std::time::Duration;

        for (error, trigger) in [
            (
                EngineLoadError::NotReady("model missing".into()),
                Trigger::NotFound,
            ),
            (
                EngineLoadError::Failed("invalid model".into()),
                Trigger::ParseError,
            ),
        ] {
            let (started, loading) = mpsc::channel();
            let (release, released) = mpsc::channel();
            let modes = Arc::new(Mutex::new(Vec::new()));
            let observed = modes.clone();
            let failure = error.clone();
            let mut ai = ShellAi::new(
                Box::new(move |mode| {
                    observed.lock().unwrap().push(mode);
                    started.send(()).unwrap();
                    released.recv_timeout(Duration::from_secs(3)).unwrap();
                    Err(failure.clone())
                }),
                AgentConfig::default(),
                Box::new(crate::NoTerminal),
            );
            let mut shell = EmbeddedShell::new(Default::default()).unwrap();
            shell.run_user_line("true");
            ai.after_command(
                &shell,
                shell.recent_commands().last().unwrap().clone(),
                None,
            );
            loading.recv_timeout(Duration::from_secs(3)).unwrap();
            let version = ai.display.invalidate();
            ai.display
                .on_cancel(version, Arc::new(move || release.send(()).unwrap()));
            assert!(ai.automatic_requests_available());

            let request = AiRequest {
                trigger,
                text: "ordinary input".into(),
                failed: None,
                user_output: None,
            };
            for _ in 0..2 {
                assert_eq!(ai.handle(&mut shell, request.clone()).exit_code, 2);
                assert_eq!(*modes.lock().unwrap(), [LoadMode::Background]);
                assert_eq!(ai.availability().loading_error(), Some(error.clone()));
            }
        }
    }

    #[test]
    fn successful_foreground_loading_restores_automatic_assistance() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let loads = Arc::new(AtomicUsize::new(0));
        let observed = loads.clone();
        let mut ai = ShellAi::new(
            Box::new(move |mode| {
                if observed.fetch_add(1, Ordering::SeqCst) == 0 {
                    assert_eq!(mode, LoadMode::Background);
                    return Err(EngineLoadError::NotReady("model missing".into()));
                }
                assert_eq!(mode, LoadMode::Foreground);
                Ok(LoadedEngine {
                    engine: Box::new(nosh_engine::MockChatEngine::with_responder(|_| {
                        vec![nosh_engine::mock::text("echo ready")]
                    })),
                    description: "fixture".into(),
                })
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        shell.run_user_line("true");
        ai.after_command(
            &shell,
            shell.recent_commands().last().unwrap().clone(),
            None,
        );
        wait_for_assistance(&ai);
        assert_eq!(
            ai.suggest(&mut shell, "foreground request"),
            SuggestionOutcome::Command("echo ready".into())
        );
        shell.run_user_line("true");
        ai.after_command(
            &shell,
            shell.recent_commands().last().unwrap().clone(),
            None,
        );
        wait_for_assistance(&ai);
        assert_eq!(loads.load(Ordering::SeqCst), 2);
        assert!(matches!(
            ai.display.result(),
            Some(nosh_shell::Assistance::Command { .. })
        ));
    }

    #[test]
    fn status_and_context_do_not_wait_for_background_loading() {
        use std::sync::mpsc;
        use std::time::Duration;
        let (started, loading) = mpsc::channel();
        let (release, released) = mpsc::channel();
        let mut ai = ShellAi::new(
            Box::new(move |_| {
                started.send(()).unwrap();
                released.recv_timeout(Duration::from_secs(3)).unwrap();
                Err(EngineLoadError::NotReady("model missing".into()))
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        shell.run_user_line("true");
        ai.after_command(
            &shell,
            shell.recent_commands().last().unwrap().clone(),
            None,
        );
        loading.recv_timeout(Duration::from_secs(3)).unwrap();
        ai.command(&mut shell, ManagementCommand::Status);
        ai.command(&mut shell, ManagementCommand::Ctx);
        assert!(ai.background.is_some());
        release
            .send(())
            .expect("status must not have waited for the loader");
        ai.reclaim().unwrap();
        assert_eq!(
            ai.availability(),
            ModelAvailability::NotReady("model missing".into())
        );
    }

    #[test]
    fn mode_selection_survives_loading_tasks_and_conversation_resets() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let mut ai = ShellAi::new(
            Box::new(|_| {
                Ok(LoadedEngine {
                    engine: Box::new(nosh_engine::MockChatEngine::with_responder(|_| {
                        vec![nosh_engine::mock::text("done")]
                    })),
                    description: "fixture".into(),
                })
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        assert_eq!(ai.mode(), ApprovalMode::Auto);
        for (mode, command_mode) in [
            (ApprovalMode::Confirm, CommandApprovalMode::Confirm),
            (ApprovalMode::Yolo, CommandApprovalMode::Yolo),
            (ApprovalMode::Auto, CommandApprovalMode::Auto),
        ] {
            ai.command(&mut shell, ManagementCommand::Mode(Some(command_mode)));
            assert_eq!(ai.badge().current_mode, Some(command_mode));
            for _ in 0..2 {
                ai.handle(
                    &mut shell,
                    AiRequest {
                        trigger: Trigger::Hash,
                        text: "fixture".into(),
                        failed: None,
                        user_output: None,
                    },
                );
                assert_eq!(ai.mode(), mode);
                assert_eq!(
                    ai.foreground
                        .as_ref()
                        .unwrap()
                        .agent
                        .as_ref()
                        .unwrap()
                        .cfg
                        .mode,
                    mode
                );
                ai.command(&mut shell, ManagementCommand::Clear);
            }
        }
    }

    #[test]
    fn automatic_loading_cannot_fall_back_to_the_foreground_policy() {
        use std::sync::{Arc, Mutex};
        use std::time::{Duration, Instant};
        let modes = Arc::new(Mutex::new(Vec::new()));
        let observed = modes.clone();
        let mut ai = ShellAi::new(
            Box::new(move |mode| {
                observed.lock().unwrap().push(mode);
                Err(EngineLoadError::NotReady("model not installed".into()))
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        shell.run_user_line("true");
        ai.after_command(
            &shell,
            shell.recent_commands().last().unwrap().clone(),
            None,
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(
            ai.display.result(),
            None | Some(nosh_shell::Assistance::Pending)
        ) && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(ai.display.result(), Some(nosh_shell::Assistance::Failure(text)) if text.contains("model not installed"))
        );
        assert_eq!(*modes.lock().unwrap(), [LoadMode::Background]);
        assert!(ai.agent(&shell, true).is_err());
        assert_eq!(
            *modes.lock().unwrap(),
            [LoadMode::Background, LoadMode::Foreground]
        );
    }

    #[test]
    fn badge_context_uses_the_existing_conversation_even_while_the_worker_owns_it() {
        let mut shell = EmbeddedShell::new(Default::default()).unwrap();
        let mut ai = ShellAi::new(
            Box::new(|_| {
                Ok(LoadedEngine {
                    engine: Box::new(nosh_engine::MockChatEngine::new(vec![
                        vec![nosh_engine::mock::text("done")],
                        vec![nosh_engine::mock::text("[None]")],
                    ])),
                    description: "fixture".into(),
                })
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        assert!(ai.badge().context.is_none());
        ai.handle(
            &mut shell,
            AiRequest {
                trigger: Trigger::Hash,
                text: "fixture".into(),
                failed: None,
                user_output: None,
            },
        );
        let usage = ai.badge().context.expect("existing conversation");
        let actual = ai
            .foreground
            .as_ref()
            .unwrap()
            .agent
            .as_ref()
            .unwrap()
            .context_usage()
            .unwrap();
        assert_eq!((usage.used, usage.max), actual);
        shell.run_user_line("true");
        ai.after_command(
            &shell,
            shell.recent_commands().last().unwrap().clone(),
            None,
        );
        assert!(ai.foreground.is_none());
        assert_eq!(ai.badge().context, Some(usage));
        ai.command(&mut shell, ManagementCommand::Clear);
        assert!(ai.badge().context.is_none());
    }
}
