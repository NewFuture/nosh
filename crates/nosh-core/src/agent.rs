//! The task loop (design §5.3): task message → model step → tool calls, each
//! risk-assessed and approved as needed → results back to the model, until
//! it answers, the step limit is hit, or the user cancels.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use nosh_hub::tr;
use nosh_llm::{
    ChatEngine, LlmError, Message, SamplingParams, SessionId, SessionSpec, StepOutcome, StopReason,
    ToolCall, Usage,
};
use nosh_permissions::{
    ApprovalMode, Context, Decision, Finding, PathClass, Risk, RiskReport, SessionAllowList,
    UserRules, assess_command, classify_path, decide,
};
use nosh_shell::{AgentExecOpts, EmbeddedShell};

use crate::approval::{ApprovalChannel, ApprovalRequest, ApprovalResponse};
use crate::prompt::{self, Environment, TaskInput};
use crate::tools::{self, ToolSet};
use crate::ui::{AgentUi, TaskSummary, UiSink};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub mode: ApprovalMode,
    pub rules: UserRules,
    /// Extra protected paths (already expanded).
    pub protected: Vec<PathBuf>,
    pub max_steps: usize,
    pub command_timeout: Duration,
    pub thinking: bool,
    pub restore_cwd: bool,
    /// Start a new conversation after this much idle time.
    pub idle_reset: Duration,
    pub sampling: SamplingParams,
    pub max_new_tokens: usize,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            mode: ApprovalMode::Confirm,
            rules: UserRules::default(),
            protected: Vec::new(),
            max_steps: 10,
            command_timeout: Duration::from_secs(60),
            thinking: false,
            restore_cwd: false,
            idle_reset: Duration::from_secs(30 * 60),
            sampling: SamplingParams::default(),
            max_new_tokens: 1024,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TaskStatus {
    #[default]
    Completed,
    /// Step limit reached, or it could not continue after a denial.
    Incomplete,
    Cancelled,
    Failed,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Incomplete => "incomplete",
            Self::Cancelled => "cancelled",
            Self::Failed => "failed",
        }
    }

    /// `nosh -a` exit codes (design §4.5).
    pub fn exit_code(self) -> i32 {
        match self {
            Self::Completed => 0,
            Self::Incomplete => 1,
            Self::Failed => 2,
            Self::Cancelled => 130,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct TaskOutcome {
    pub status: TaskStatus,
    pub steps: usize,
    /// Text of the last model turn.
    pub answer: String,
    pub proposed: Option<String>,
    pub commands_run: usize,
    pub denied: usize,
    pub usage: Usage,
    pub error: Option<String>,
}

/// Full output of an agent command, for `ai out <id>`.
#[derive(Debug, Clone)]
pub struct OutputRecord {
    pub id: usize,
    pub command: String,
    pub text: String,
}

enum Exec {
    Result(String),
    Denied(String),
    Proposed(String, String),
    Aborted(String),
}

pub struct Agent {
    engine: Box<dyn ChatEngine>,
    pub cfg: AgentConfig,
    env: Environment,
    tools: ToolSet,
    sid: Option<SessionId>,
    last_task: Option<Instant>,
    allow: SessionAllowList,
    /// Tool results owed to the conversation from an interrupted turn.
    carry: Vec<Message>,
    outputs: Vec<OutputRecord>,
    next_output: usize,
    notes_seen: HashSet<PathBuf>,
    hooked: bool,
}

const SUMMARIZE: &str = "[system] Step limit reached. Do not call any more tools. Summarize what you found in the user's language and suggest the next step.";

impl Agent {
    pub fn new(
        engine: Box<dyn ChatEngine>,
        cfg: AgentConfig,
        env: Environment,
        tools: ToolSet,
    ) -> Self {
        Self {
            engine,
            cfg,
            env,
            tools,
            sid: None,
            last_task: None,
            allow: SessionAllowList::default(),
            carry: Vec::new(),
            outputs: Vec::new(),
            next_output: 1,
            notes_seen: HashSet::new(),
            hooked: false,
        }
    }

    pub fn engine_mut(&mut self) -> &mut dyn ChatEngine {
        self.engine.as_mut()
    }

    pub fn environment(&self) -> &Environment {
        &self.env
    }

    /// Starts a new conversation (`ai clear`, idle timeout, config change).
    pub fn reset_conversation(&mut self) {
        if let Some(sid) = self.sid.take() {
            self.engine.close(sid);
        }
        self.carry.clear();
        self.notes_seen.clear();
    }

    pub fn context_usage(&self) -> Option<(usize, usize)> {
        self.sid.map(|s| self.engine.context_usage(s))
    }

    pub fn output(&self, id: usize) -> Option<&OutputRecord> {
        self.outputs.iter().find(|o| o.id == id)
    }

    pub fn last_output_id(&self) -> Option<usize> {
        self.outputs.last().map(|o| o.id)
    }

    fn spec(&self) -> SessionSpec {
        let system = match self.tools {
            ToolSet::Suggest => prompt::suggest_system_prompt(&self.env),
            _ => prompt::system_prompt(&self.env),
        };
        SessionSpec {
            system,
            tools: tools::specs(self.tools),
            thinking: self.cfg.thinking,
            sampling: self.cfg.sampling,
            max_new_tokens: self.cfg.max_new_tokens,
        }
    }

    fn ensure_session(&mut self) -> Result<SessionId, LlmError> {
        if self.sid.is_some()
            && self
                .last_task
                .is_some_and(|t| t.elapsed() > self.cfg.idle_reset)
        {
            self.reset_conversation();
        }
        if let Some(sid) = self.sid {
            let (used, max) = self.engine.context_usage(sid);
            if used * 100 > max * 85 {
                self.engine.compact_tool_results(sid, 0);
                let (used, _) = self.engine.context_usage(sid);
                if used * 100 > max * 60 {
                    self.reset_conversation();
                }
            }
        }
        match self.sid {
            Some(s) => Ok(s),
            None => {
                let spec = self.spec();
                let sid = self.engine.open(spec)?;
                self.sid = Some(sid);
                Ok(sid)
            }
        }
    }

    fn perm_context(&self, shell: &EmbeddedShell) -> Context {
        let mut ctx = Context::new(shell.cwd(), shell.workspace());
        if let Some(h) = shell.home() {
            ctx = ctx.with_home(h);
        }
        ctx.aliases = shell.aliases();
        ctx.functions = shell.functions();
        ctx.protected = self.cfg.protected.clone();
        ctx
    }

    fn step(
        &mut self,
        sid: SessionId,
        msgs: Vec<Message>,
        ui: &mut dyn AgentUi,
    ) -> Result<StepOutcome, LlmError> {
        let mut sink = |ev: nosh_llm::Event| match ev {
            nosh_llm::Event::Text(t) => ui.text(&t),
            nosh_llm::Event::Think(t) => ui.think(&t),
            nosh_llm::Event::Prefill { done, total } => ui.prefill(done, total),
            _ => {}
        };
        let keep = self.engine.message_count(sid);
        match self.engine.step(sid, msgs.clone(), &mut sink) {
            Err(LlmError::ContextFull { .. }) => {
                // Drop the failed append, shorten old tool output and retry once.
                let _ = self.engine.rewind(sid, keep);
                self.engine.compact_tool_results(sid, 0);
                self.engine.step(sid, msgs, &mut sink)
            }
            r => r,
        }
    }

    /// Runs one task to completion in the shared shell session.
    pub fn run_task(
        &mut self,
        shell: &mut EmbeddedShell,
        input: TaskInput,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
    ) -> TaskOutcome {
        let started = Instant::now();
        let ints = shell.interrupts();
        let cancel = self.engine.cancel_handle();
        if !self.hooked {
            let c = cancel.clone();
            ints.on_interrupt(move || c.cancel());
            self.hooked = true;
        }
        let int0 = ints.count();
        let mut out = TaskOutcome::default();
        let sid = match self.ensure_session() {
            Ok(s) => s,
            Err(e) => {
                ui.error(&e.to_string());
                out.status = TaskStatus::Failed;
                out.error = Some(e.to_string());
                return out;
            }
        };
        let cwd0 = shell.cwd();
        let notes = prompt::project_notes(&cwd0)
            .filter(|(p, _)| self.notes_seen.insert(p.clone()))
            .map(|(_, t)| t);
        let mut pending = std::mem::take(&mut self.carry);
        pending.push(Message::User(prompt::task_message(
            shell,
            &input,
            notes.as_deref(),
        )));
        let mut errors: HashMap<String, usize> = HashMap::new();
        let mut summarizing = false;
        loop {
            if ints.count() >= int0 + 2 {
                out.status = TaskStatus::Cancelled;
                self.carry = pending;
                break;
            }
            if out.steps >= self.cfg.max_steps && !summarizing {
                summarizing = true;
                pending.push(Message::User(SUMMARIZE.into()));
            }
            cancel.reset();
            out.steps += 1;
            let step = match self.step(sid, std::mem::take(&mut pending), ui) {
                Ok(s) => s,
                Err(e) => {
                    ui.error(&e.to_string());
                    out.status = TaskStatus::Failed;
                    out.error = Some(e.to_string());
                    break;
                }
            };
            add_usage(&mut out.usage, &step.usage);
            out.answer = step.text.trim().to_string();
            if step.stop == StopReason::Cancelled {
                out.status = TaskStatus::Cancelled;
                self.carry = step
                    .tool_calls
                    .iter()
                    .map(|_| Message::Tool("[cancelled by the user]".into()))
                    .collect();
                break;
            }
            if summarizing {
                self.carry = step
                    .tool_calls
                    .iter()
                    .map(|_| Message::Tool("[skipped: step limit reached]".into()))
                    .collect();
                out.status = TaskStatus::Incomplete;
                break;
            }
            if step.tool_calls.is_empty() && step.errors.is_empty() {
                out.status = if step.stop == StopReason::MaxTokens {
                    TaskStatus::Incomplete
                } else {
                    TaskStatus::Completed
                };
                break;
            }
            let mut denied = false;
            let mut aborted = false;
            let mut proposed_only = step.tool_calls.len() == 1 && step.errors.is_empty();
            for call in &step.tool_calls {
                if denied || aborted {
                    pending.push(Message::Tool(
                        "[skipped] an earlier call in this turn was denied or cancelled".into(),
                    ));
                    continue;
                }
                match self.exec_call(shell, call, approval, ui, int0) {
                    Exec::Result(t) => {
                        proposed_only = false;
                        if call.name == "run_command" {
                            out.commands_run += 1;
                        }
                        pending.push(Message::Tool(t));
                    }
                    Exec::Denied(t) => {
                        proposed_only = false;
                        denied = true;
                        out.denied += 1;
                        pending.push(Message::Tool(t));
                    }
                    Exec::Proposed(cmd, t) => {
                        out.proposed = Some(cmd);
                        pending.push(Message::Tool(t));
                    }
                    Exec::Aborted(t) => {
                        aborted = true;
                        pending.push(Message::Tool(t));
                    }
                }
            }
            let mut fatal = None;
            for e in &step.errors {
                let key = format!("{:?}:{}", e.kind, e.tool.as_deref().unwrap_or(""));
                let n = errors.entry(key).or_insert(0);
                *n += 1;
                if *n > 2 {
                    fatal = Some(format!("the model kept producing invalid tool calls: {e}"));
                }
                pending.push(Message::Tool(format!(
                    "error: {e}. Fix the tool call and try again."
                )));
            }
            if aborted {
                out.status = TaskStatus::Cancelled;
                self.carry = pending;
                break;
            }
            if let Some(f) = fatal {
                ui.error(&f);
                out.status = TaskStatus::Failed;
                out.error = Some(f);
                self.carry = pending;
                break;
            }
            if proposed_only && out.proposed.is_some() {
                // The command is in the user's hands now.
                out.status = TaskStatus::Completed;
                self.carry = pending;
                break;
            }
        }
        if out.status == TaskStatus::Completed && out.denied > 0 && out.commands_run == 0 {
            out.status = TaskStatus::Incomplete;
        }
        let cwd1 = shell.cwd();
        if cwd1 != cwd0 {
            if self.cfg.restore_cwd {
                let cmd = format!(
                    "cd -- '{}'",
                    cwd0.display().to_string().replace('\'', "'\\''")
                );
                let _ = shell.run_agent_command(
                    &cmd,
                    &AgentExecOpts::default(),
                    &mut nosh_shell::NullSink,
                );
            } else {
                ui.notice(&format!("cwd → {}", cwd1.display()));
            }
        }
        if out.status == TaskStatus::Cancelled {
            ui.notice(tr!("已取消", "cancelled"));
        }
        let tps = out.usage.decode_tps();
        ui.finish(&TaskSummary {
            status: out.status.as_str().into(),
            steps: out.steps,
            secs: started.elapsed().as_secs_f64(),
            prompt_tokens: out.usage.prompt_tokens,
            completion_tokens: out.usage.completion_tokens,
            decode_tps: if out.usage.completion_tokens > 0 {
                tps
            } else {
                0.0
            },
            note: match out.status {
                TaskStatus::Incomplete if out.steps > self.cfg.max_steps => {
                    Some(tr!("达到步数上限", "step limit reached").into())
                }
                _ => None,
            },
        });
        self.last_task = Some(Instant::now());
        out
    }

    fn exec_call(
        &mut self,
        shell: &mut EmbeddedShell,
        call: &ToolCall,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
        int0: u64,
    ) -> Exec {
        let allowed: Vec<String> = tools::specs(self.tools)
            .into_iter()
            .map(|s| s.name)
            .collect();
        if !allowed.contains(&call.name) {
            return Exec::Result(format!(
                "error: unknown tool '{}'; available tools: {}",
                call.name,
                allowed.join(", ")
            ));
        }
        match call.name.as_str() {
            "run_command" => self.run_command(shell, call, approval, ui, int0),
            "read_file" | "list_dir" => self.read_tool(shell, call, approval, ui),
            "propose_command" => {
                let Some(cmd) = call
                    .str_arg("command")
                    .map(str::trim)
                    .filter(|c| !c.is_empty())
                else {
                    return Exec::Result("error: missing required parameter 'command'".into());
                };
                ui.proposed(cmd, call.str_arg("explanation"));
                Exec::Proposed(
                    cmd.to_string(),
                    "[proposed] The command was placed in the user's input line; the user will review and run it.".into(),
                )
            }
            _ => Exec::Result(format!("error: unknown tool '{}'", call.name)),
        }
    }

    fn ask(
        &mut self,
        approval: &mut dyn ApprovalChannel,
        req: ApprovalRequest,
        ui: &mut dyn AgentUi,
    ) -> ApprovalResponse {
        ui.pause();
        approval.request(&req)
    }

    fn run_command(
        &mut self,
        shell: &mut EmbeddedShell,
        call: &ToolCall,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
        int0: u64,
    ) -> Exec {
        let Some(original) = call
            .str_arg("command")
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            return Exec::Result("error: missing required parameter 'command'".into());
        };
        let timeout = call
            .int_arg("timeout_sec")
            .map(|t| Duration::from_secs(t.clamp(1, 600) as u64))
            .unwrap_or(self.cfg.command_timeout);
        let mut command = original.to_string();
        let mut edited = false;
        for _ in 0..4 {
            let report = assess_command(&command, &self.perm_context(shell));
            let risk = report.risk();
            let label = match decide(
                &report,
                &command,
                self.cfg.mode,
                &self.cfg.rules,
                &self.allow,
            ) {
                Decision::Deny { reason } => {
                    ui.tool_start(
                        "run_command",
                        &command,
                        Some(risk),
                        &format!("{risk} · denied"),
                    );
                    ui.tool_end(&reason);
                    return Exec::Denied(format!(
                        "[denied by policy] {reason}. Do not retry this command; explain to the user or use a different approach."
                    ));
                }
                Decision::Allow => {
                    if risk == Risk::Safe {
                        format!("{risk} · auto")
                    } else {
                        format!("{risk} · allowed ({})", self.cfg.mode.as_str())
                    }
                }
                Decision::Ask { strong } => {
                    let req = ApprovalRequest {
                        tool: "run_command".into(),
                        command: command.clone(),
                        risk,
                        reasons: report.top_reasons().iter().map(|s| s.to_string()).collect(),
                        strong,
                        can_grant: !strong && risk <= Risk::Mutating,
                    };
                    match self.ask(approval, req, ui) {
                        ApprovalResponse::Approve => format!("{risk} · approved"),
                        ApprovalResponse::ApproveSimilar => {
                            self.allow.grant(&report);
                            format!("{risk} · approved (similar allowed)")
                        }
                        ApprovalResponse::Edit(c) => {
                            command = c.trim().to_string();
                            edited = true;
                            continue;
                        }
                        ApprovalResponse::Deny { reason } => {
                            let why = reason.map(|r| format!(" Reason: {r}.")).unwrap_or_default();
                            return Exec::Denied(format!(
                                "[denied by user] The user did not allow this command.{why} Do not run it again; adjust the plan or explain."
                            ));
                        }
                    }
                }
            };
            return self.execute(shell, &command, &report, &label, timeout, edited, ui, int0);
        }
        Exec::Denied("[denied] too many edits".into())
    }

    #[allow(clippy::too_many_arguments)]
    fn execute(
        &mut self,
        shell: &mut EmbeddedShell,
        command: &str,
        report: &RiskReport,
        label: &str,
        timeout: Duration,
        edited: bool,
        ui: &mut dyn AgentUi,
        int0: u64,
    ) -> Exec {
        let to_run = report
            .rewritten
            .clone()
            .unwrap_or_else(|| command.to_string());
        ui.tool_start("run_command", &to_run, Some(report.risk()), label);
        let opts = AgentExecOpts {
            timeout,
            ..AgentExecOpts::default()
        };
        let r = match shell.run_agent_command(&to_run, &opts, &mut UiSink(ui)) {
            Ok(r) => r,
            Err(e) => {
                ui.tool_end(&format!("error: {e}"));
                return Exec::Result(format!("error: failed to run the command: {e}"));
            }
        };
        let id = self.next_output;
        self.next_output += 1;
        let big = r.truncated || r.stdout.len() + r.stderr.len() > tools::OUTPUT_CHARS;
        let log = if big {
            tools::save_output(id, &to_run, &r)
        } else {
            None
        };
        self.outputs.push(OutputRecord {
            id,
            command: to_run.clone(),
            text: format!("{}{}", r.stdout, r.stderr),
        });
        if self.outputs.len() > 20 {
            self.outputs.remove(0);
        }
        let mut summary = format!("exit {} · {:.2}s", r.exit_code, r.duration.as_secs_f64());
        if r.timed_out {
            summary.push_str(" · timed out");
        }
        if r.interrupted {
            summary.push_str(" · interrupted");
        }
        if !r.stdout.is_empty() || !r.stderr.is_empty() {
            summary.push_str(&format!(" · ai out {id}"));
        }
        ui.tool_end(&summary);
        let mut text = tools::format_command_result(&r, log.as_deref());
        if edited {
            text = format!("[note] the user edited the command to: {to_run}\n{text}");
        }
        if shell.interrupts().count() >= int0 + 2 {
            return Exec::Aborted(text);
        }
        Exec::Result(text)
    }

    fn read_tool(
        &mut self,
        shell: &mut EmbeddedShell,
        call: &ToolCall,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
    ) -> Exec {
        let cwd = shell.cwd();
        let path = tools::tool_path(call, &cwd);
        let ctx = self.perm_context(shell);
        let detail = path.display().to_string();
        let mut risk = Risk::Safe;
        let mut label = format!("{} · auto", Risk::Safe);
        if let PathClass::Protected(what) = classify_path(&path, &ctx) {
            risk = Risk::Mutating;
            let why = format!("reads a protected path ({what})");
            let report = RiskReport {
                findings: vec![Finding {
                    risk,
                    reason: why.clone(),
                }],
                reads_protected: true,
                ..RiskReport::default()
            };
            let shown = format!("{} {detail}", call.name);
            match decide(&report, &shown, self.cfg.mode, &self.cfg.rules, &self.allow) {
                Decision::Allow => label = format!("{risk} · allowed"),
                Decision::Deny { reason } => {
                    return Exec::Denied(format!("[denied by policy] {reason}"));
                }
                Decision::Ask { strong } => {
                    let req = ApprovalRequest {
                        tool: call.name.clone(),
                        command: shown,
                        risk,
                        reasons: vec![why],
                        strong,
                        can_grant: false,
                    };
                    match self.ask(approval, req, ui) {
                        ApprovalResponse::Approve | ApprovalResponse::ApproveSimilar => {
                            label = format!("{risk} · approved");
                        }
                        _ => {
                            return Exec::Denied(
                                "[denied by user] The user did not allow reading this path.".into(),
                            );
                        }
                    }
                }
            }
        }
        ui.tool_start(&call.name, &detail, Some(risk), &label);
        let r = if call.name == "read_file" {
            tools::read_file(call, &cwd)
        } else {
            tools::list_dir(call, &cwd)
        };
        match r {
            Ok(t) => {
                ui.tool_end(&format!("{} lines", t.lines().count().saturating_sub(1)));
                Exec::Result(t)
            }
            Err(e) => {
                ui.tool_end(&format!("error: {e}"));
                Exec::Result(format!("error: {e}"))
            }
        }
    }
}

fn add_usage(total: &mut Usage, u: &Usage) {
    total.prompt_tokens += u.prompt_tokens;
    total.cached_tokens += u.cached_tokens;
    total.completion_tokens += u.completion_tokens;
    total.prefill_secs += u.prefill_secs;
    total.decode_secs += u.decode_secs;
    if total.ttft_secs == 0.0 {
        total.ttft_secs = u.ttft_secs;
    }
    total.context_used = u.context_used;
    total.context_max = u.context_max;
}
