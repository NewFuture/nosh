//! [`ShellAi`]: the REPL's AI handler. Loads the engine on first use and
//! runs tasks in the shared session.

use nosh_hub::tr;
use nosh_llm::ChatEngine;
use nosh_permissions::ApprovalMode;
use nosh_shell::{AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell, Trigger, style};

use crate::agent::{Agent, AgentConfig};
use crate::approval::ApprovalChannel;
use crate::assist_worker::{EngineState, Worker};
use crate::prompt::{Environment, TaskInput};
use crate::tools::ToolSet;
use crate::ui::{TermUi, approval_label};

pub struct LoadedEngine {
    pub engine: Box<dyn ChatEngine>,
    /// Shown by `ai status`, e.g. model id and file.
    pub description: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoadMode {
    Foreground,
    /// Use installed models only, without terminal input or output.
    Background,
}

/// Loads the model under the caller's interaction policy.
pub type EngineLoader = Box<dyn FnMut(LoadMode) -> Result<LoadedEngine, String> + Send>;

pub struct ShellAi {
    foreground: Option<EngineState>,
    background: Option<Worker>,
    display: nosh_shell::AssistDisplay,
    engine_hooked: bool,
    cfg: AgentConfig,
    approval: Box<dyn ApprovalChannel>,
}

impl ShellAi {
    pub fn new(loader: EngineLoader, cfg: AgentConfig, approval: Box<dyn ApprovalChannel>) -> Self {
        Self {
            foreground: Some(EngineState {
                loader,
                agent: None,
                description: None,
            }),
            background: None,
            display: Default::default(),
            engine_hooked: false,
            cfg,
            approval,
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        self.cfg.mode
    }

    fn reclaim(&mut self) -> Result<(), String> {
        self.display.invalidate();
        if let Some(worker) = self.background.take() {
            self.foreground = Some(worker.reclaim()?);
        }
        Ok(())
    }

    fn agent(&mut self, shell: &EmbeddedShell) -> Option<&mut Agent> {
        if let Err(error) = self.reclaim() {
            eprintln!("nosh: {error}");
            return None;
        }
        let Some(state) = self.foreground.as_mut() else {
            eprintln!("nosh: inference engine is unavailable");
            return None;
        };
        if state.agent.is_none() {
            match (state.loader)(LoadMode::Foreground) {
                Ok(l) => {
                    state.description = Some(l.description);
                    state.agent = Some(Agent::new(
                        l.engine,
                        self.cfg.clone(),
                        Environment::detect(shell),
                        ToolSet::Full,
                    ));
                }
                Err(e) => {
                    eprintln!("{}", style::red(&format!("nosh: {e}")));
                    return None;
                }
            }
        }
        if !self.engine_hooked
            && let Some(agent) = &mut state.agent
        {
            let cancel = agent.engine_mut().cancel_handle();
            shell.interrupts().on_interrupt(move || cancel.cancel());
            self.engine_hooked = true;
        }
        state.agent.as_mut()
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
            eprintln!("{}", style::red_bold(&yolo_warning()));
        }
    }

    fn assist(
        &mut self,
        shell: &EmbeddedShell,
        intent: crate::command_assist::Intent,
        text: String,
        command: Option<nosh_shell::UserCommand>,
        output: Option<nosh_shell::UserOutput>,
    ) -> AiOutcome {
        use crate::command_assist::{AssistRequest, AssistResult};
        use crate::ui::AgentUi;
        let started = std::time::Instant::now();
        let cfg = self.cfg.clone();
        let request = match AssistRequest::capture(shell, &cfg, intent, text, command, output) {
            Ok(request) => request,
            Err(error) => {
                eprintln!("nosh: {error}");
                return AiOutcome {
                    prefill: None,
                    exit_code: 2,
                };
            }
        };
        let interrupts = shell.interrupts().count();
        let Some(agent) = self.agent(shell) else {
            return AiOutcome {
                prefill: None,
                exit_code: 2,
            };
        };
        if shell.interrupts().count() != interrupts {
            return AiOutcome {
                prefill: None,
                exit_code: 130,
            };
        }
        let cancel = agent.engine_mut().cancel_handle();
        cancel.reset();
        match crate::command_assist::run(agent.engine_mut(), &request, &cfg, &cancel, |_| true) {
            Ok(outcome) => {
                let mut ui = TermUi::new(false);
                let prefill = match outcome.result {
                    AssistResult::Command(program) => Some(program),
                    AssistResult::Clarify(question) => {
                        ui.text(&question);
                        None
                    }
                    AssistResult::NoSuggestion => None,
                };
                let u = outcome.usage;
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
                AiOutcome {
                    prefill,
                    exit_code: 0,
                }
            }
            Err(error) => {
                let exit_code = if matches!(error, crate::command_assist::AssistError::Cancelled) {
                    130
                } else {
                    2
                };
                eprintln!("nosh: {error}");
                AiOutcome {
                    prefill: None,
                    exit_code,
                }
            }
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
        if matches!(req.trigger, Trigger::Failed { .. }) && req.text.trim().is_empty() {
            return self.assist(
                shell,
                crate::command_assist::Intent::Fix,
                req.text,
                req.failed,
                req.user_output,
            );
        }
        if let Some(error) = &self.cfg.rules_error {
            eprintln!("nosh: AI execution blocked by invalid safety configuration: {error}");
            return AiOutcome {
                prefill: None,
                exit_code: 2,
            };
        }
        let show_think = self.cfg.thinking;
        let ints = shell.interrupts().count();
        if self.agent(shell).is_none() {
            return AiOutcome {
                prefill: None,
                exit_code: 2,
            };
        }
        // Ctrl-C while the model was downloading or loading: stop here.
        if shell.interrupts().count() > ints {
            eprintln!("{}", style::dim(tr!("已取消", "cancelled")));
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
        let mut ui = TermUi::new(false);
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

    fn builtin(&mut self, shell: &mut EmbeddedShell, args: &[String]) -> AiOutcome {
        if let Err(error) = self.reclaim() {
            eprintln!("nosh: {error}");
            return AiOutcome {
                prefill: None,
                exit_code: 2,
            };
        }
        let sub = args.first().map(String::as_str).unwrap_or("");
        let arg = args.get(1).map(String::as_str);
        if sub == "next" {
            let command = shell
                .recent_commands()
                .last()
                .filter(|command| command.exit == 0)
                .cloned();
            return self.assist(
                shell,
                crate::command_assist::Intent::Next,
                arg.unwrap_or("").into(),
                command,
                None,
            );
        }
        let state = self.foreground.as_ref();
        let agent = state.and_then(|state| state.agent.as_ref());
        match sub {
            "mode" => match arg.map(ApprovalMode::parse) {
                Some(Some(m)) => {
                    self.set_mode(m);
                    say(&approval_label(m));
                }
                Some(None) => say("usage: ai mode confirm|auto|yolo"),
                None => say(&approval_label(self.cfg.mode)),
            },
            "think" => {
                match arg {
                    Some("on") => self.cfg.thinking = true,
                    Some("off") => self.cfg.thinking = false,
                    _ => {}
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
            "clear" => {
                if let Some(a) = self
                    .foreground
                    .as_mut()
                    .and_then(|state| state.agent.as_mut())
                {
                    a.reset_conversation();
                }
                say(tr!("已开始新对话", "started a new conversation"));
            }
            "ctx" => match agent.and_then(Agent::context_usage) {
                Some((used, max)) => say(&format!(
                    "context: {used} / {max} tokens ({}%)",
                    used * 100 / max.max(1)
                )),
                None => say(tr!("还没有对话", "no conversation yet")),
            },
            "status" => {
                say(&format!(
                    "model: {}",
                    state
                        .and_then(|state| state.description.as_deref())
                        .unwrap_or(tr!(
                            "未加载（首次使用时加载）",
                            "not loaded (loads on first use)"
                        ))
                ));
                say(&approval_label(self.cfg.mode));
                say(&format!(
                    "think: {}",
                    if self.cfg.thinking { "on" } else { "off" }
                ));
                if let Some((used, max)) = agent.and_then(Agent::context_usage) {
                    say(&format!("context: {used} / {max} tokens"));
                }
            }
            "out" => {
                let Some(agent) = agent else {
                    say(tr!("还没有 agent 命令", "no agent commands yet"));
                    return AiOutcome::default();
                };
                let id = arg
                    .and_then(|a| a.trim_start_matches('#').parse().ok())
                    .or_else(|| agent.last_output_id());
                match id.and_then(|i| agent.output(i)) {
                    Some(o) => {
                        eprintln!("{}", style::dim(&format!("$ {}", o.command)));
                        print!("{}", o.text);
                        if !o.text.ends_with('\n') {
                            println!();
                        }
                    }
                    None => say(tr!("没有这个编号的输出", "no output with that number")),
                }
            }
            _ => say(tr!(
                "这个 MVP 版本还不支持该命令",
                "not available in this version"
            )),
        }
        let _ = shell;
        AiOutcome::default()
    }

    fn suggest(&mut self, shell: &mut EmbeddedShell, line: &str) -> Option<String> {
        self.assist(
            shell,
            crate::command_assist::Intent::Generate,
            line.into(),
            None,
            None,
        )
        .prefill
    }

    fn badge(&self) -> Badge {
        Badge {
            mode: approval_label(self.cfg.mode),
            yolo: self.cfg.mode == ApprovalMode::Yolo,
            note: None,
        }
    }

    fn assistance(&self) -> Option<nosh_shell::AssistDisplay> {
        Some(self.display.clone())
    }

    fn after_command(
        &mut self,
        shell: &EmbeddedShell,
        command: nosh_shell::UserCommand,
        output: Option<nosh_shell::UserOutput>,
    ) {
        use crate::command_assist::{AssistRequest, Intent};
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
                self.display.publish(
                    version,
                    Some(nosh_shell::Assistance::Message(format!("nosh: {error}"))),
                );
                return;
            }
        };
        request.background = true;
        if self.background.is_none() {
            let Some(state) = self.foreground.take() else {
                let version = self.display.invalidate();
                self.display.publish(
                    version,
                    Some(nosh_shell::Assistance::Message(
                        "nosh: inference engine unavailable".into(),
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

    #[test]
    fn mode_selection_survives_loading_tasks_and_conversation_resets() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let mut ai = ShellAi::new(
            Box::new(|_| {
                Ok(LoadedEngine {
                    engine: Box::new(nosh_llm::MockChatEngine::with_responder(|_| {
                        vec![nosh_llm::mock::text("done")]
                    })),
                    description: "fixture".into(),
                })
            }),
            AgentConfig::default(),
            Box::new(crate::NoTerminal),
        );
        assert_eq!(ai.mode(), ApprovalMode::Auto);
        for mode in [
            ApprovalMode::Confirm,
            ApprovalMode::Yolo,
            ApprovalMode::Auto,
        ] {
            ai.builtin(&mut shell, &["mode".into(), mode.as_str().into()]);
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
                ai.builtin(&mut shell, &["clear".into()]);
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
                Err("model not installed".into())
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
        while ai.display.result().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(ai.display.result(), Some(nosh_shell::Assistance::Message(text)) if text.contains("model not installed"))
        );
        assert_eq!(*modes.lock().unwrap(), [LoadMode::Background]);
        assert!(ai.agent(&shell).is_none());
        assert_eq!(
            *modes.lock().unwrap(),
            [LoadMode::Background, LoadMode::Foreground]
        );
    }
}
