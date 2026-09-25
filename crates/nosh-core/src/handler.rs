//! [`ShellAi`]: the REPL's AI handler. Loads the engine on first use and
//! runs tasks in the shared session.

use nosh_hub::tr;
use nosh_llm::ChatEngine;
use nosh_permissions::ApprovalMode;
use nosh_shell::{AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell, Trigger, style};

use crate::agent::{Agent, AgentConfig};
use crate::approval::ApprovalChannel;
use crate::prompt::{Environment, TaskInput};
use crate::tools::ToolSet;
use crate::ui::TermUi;

pub struct LoadedEngine {
    pub engine: Box<dyn ChatEngine>,
    /// Shown by `ai status`, e.g. model id and file.
    pub description: String,
}

/// Loads (and if needed downloads) the model; errors are user-facing text.
pub type EngineLoader = Box<dyn FnMut() -> Result<LoadedEngine, String>>;

pub struct ShellAi {
    loader: EngineLoader,
    cfg: AgentConfig,
    agent: Option<Agent>,
    description: Option<String>,
    approval: Box<dyn ApprovalChannel>,
}

impl ShellAi {
    pub fn new(loader: EngineLoader, cfg: AgentConfig, approval: Box<dyn ApprovalChannel>) -> Self {
        Self {
            loader,
            cfg,
            agent: None,
            description: None,
            approval,
        }
    }

    pub fn mode(&self) -> ApprovalMode {
        self.cfg.mode
    }

    fn agent(&mut self, shell: &EmbeddedShell) -> Option<&mut Agent> {
        if self.agent.is_none() {
            match (self.loader)() {
                Ok(l) => {
                    self.description = Some(l.description);
                    self.agent = Some(Agent::new(
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
        self.agent.as_mut()
    }

    fn set_mode(&mut self, mode: ApprovalMode) {
        self.cfg.mode = mode;
        if let Some(a) = &mut self.agent {
            a.cfg.mode = mode;
        }
        if mode == ApprovalMode::Yolo {
            eprintln!("{}", style::red_bold(&yolo_warning()));
        }
    }
}

pub fn yolo_warning() -> String {
    tr!(
        "YOLO 模式：Mutating 命令不再询问，Dangerous 仍需确认，Forbidden 始终拒绝。风险自负。",
        "YOLO mode: Mutating commands run without asking; Dangerous still asks; Forbidden is always denied. Use at your own risk."
    )
    .to_string()
}

fn say(msg: &str) {
    eprintln!("{msg}");
}

impl AiHandler for ShellAi {
    fn handle(&mut self, shell: &mut EmbeddedShell, req: AiRequest) -> AiOutcome {
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
        let (Some(agent), approval) = (self.agent.as_mut(), self.approval.as_mut()) else {
            return AiOutcome::default();
        };
        let mut ui = TermUi::new(false);
        ui.show_think = show_think;
        let input = TaskInput {
            trigger: req.trigger,
            text: req.text,
            failed: req.failed,
            attachment: None,
        };
        let out = agent.run_task(shell, input, approval, &mut ui);
        AiOutcome {
            prefill: out.proposed,
            exit_code: out.status.exit_code(),
        }
    }

    fn builtin(&mut self, shell: &mut EmbeddedShell, args: &[String]) -> AiOutcome {
        let sub = args.first().map(String::as_str).unwrap_or("");
        let arg = args.get(1).map(String::as_str);
        match sub {
            "mode" => match arg.map(ApprovalMode::parse) {
                Some(Some(m)) => {
                    self.set_mode(m);
                    say(&format!("mode: {}", m.as_str()));
                }
                Some(None) => say("usage: ai mode confirm|auto|yolo"),
                None => say(&format!("mode: {}", self.cfg.mode.as_str())),
            },
            "think" => {
                match arg {
                    Some("on") => self.cfg.thinking = true,
                    Some("off") => self.cfg.thinking = false,
                    _ => {}
                }
                if let Some(a) = &mut self.agent
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
                if let Some(a) = &mut self.agent {
                    a.reset_conversation();
                }
                say(tr!("已开始新对话", "started a new conversation"));
            }
            "ctx" => match self.agent.as_ref().and_then(Agent::context_usage) {
                Some((used, max)) => say(&format!(
                    "context: {used} / {max} tokens ({}%)",
                    used * 100 / max.max(1)
                )),
                None => say(tr!("还没有对话", "no conversation yet")),
            },
            "status" => {
                say(&format!(
                    "model: {}",
                    self.description.as_deref().unwrap_or(tr!(
                        "未加载（首次使用时加载）",
                        "not loaded (loads on first use)"
                    ))
                ));
                say(&format!("mode: {}", self.cfg.mode.as_str()));
                say(&format!(
                    "think: {}",
                    if self.cfg.thinking { "on" } else { "off" }
                ));
                if let Some((used, max)) = self.agent.as_ref().and_then(Agent::context_usage) {
                    say(&format!("context: {used} / {max} tokens"));
                }
            }
            "out" => {
                let Some(agent) = &self.agent else {
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
        let sampling = self.cfg.sampling;
        let ints = shell.interrupts().count();
        let agent = self.agent(shell)?;
        if shell.interrupts().count() > ints {
            return None;
        }
        let env = agent.environment().clone();
        let animated = style::stderr().ansi;
        let status = format!(
            "{} {}",
            style::glyph("…", "..."),
            tr!("生成命令中", "suggesting")
        );
        if animated {
            let status = style::clip_line(
                &status,
                nosh_shell::term::stderr_columns()
                    .unwrap_or(80)
                    .saturating_sub(1),
                0,
                "",
            );
            eprint!("{}", style::dim(&status));
        } else {
            eprintln!("{status}");
        }
        let r = crate::suggest::suggest(
            agent.engine_mut(),
            &env,
            shell,
            line,
            Trigger::Builtin,
            sampling,
        );
        if animated {
            eprint!("\r\x1b[K");
        }
        match r {
            Ok(Some(s)) => Some(s.command),
            Ok(None) => {
                eprintln!(
                    "{}",
                    tr!(
                        "nosh: 没有完整有效的命令建议",
                        "nosh: no complete valid command suggestion"
                    )
                );
                None
            }
            Err(e) => {
                eprintln!("nosh: {e}");
                None
            }
        }
    }

    fn badge(&self) -> Badge {
        Badge {
            mode: match self.cfg.mode {
                ApprovalMode::Yolo => "YOLO".into(),
                m => m.as_str().into(),
            },
            yolo: self.cfg.mode == ApprovalMode::Yolo,
            note: None,
        }
    }
}
