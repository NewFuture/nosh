//! The task loop (design §5.3): task message → model step → tool calls, each
//! risk-assessed and approved as needed → results back to the model, until
//! it answers, the step limit is hit, or the user cancels.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nosh_hub::tr;
use nosh_llm::{
    ChatEngine, LlmError, Message, SamplingParams, SessionId, SessionSpec, StepOutcome, StopReason,
    ToolCall, Usage,
};
use nosh_permissions::{
    ApprovalMode, Context, Decision, Risk, RiskReport, SessionAllowList, UserRules, assess_command,
    assess_read, evaluate,
};
use nosh_shell::{AgentExecOpts, EmbeddedShell};

use crate::approval::{ApprovalChannel, ApprovalRequest, ApprovalResponse};
use crate::prompt::{self, Environment, TaskInput};
use crate::tools::{self, BuiltinTool, NoRedact, Redactor, ToolSet};
use crate::ui::{Activity, AgentUi, TaskSummary, UiSink};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub mode: ApprovalMode,
    pub rules: UserRules,
    pub rules_error: Option<String>,
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
            mode: ApprovalMode::default(),
            rules: UserRules::default(),
            rules_error: None,
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
    pub permission: String,
    pub previous_cwd: PathBuf,
    pub previous_variables: Vec<(String, Option<String>, bool)>,
}

enum Exec {
    Result(String),
    CommandResult(String),
    Denied(String),
    Handoff(String, String),
    Aborted(String),
}

enum Authorization {
    Allowed {
        label: String,
        manual: bool,
        grant: bool,
    },
    Edit(String),
    Denied(String),
}

type CommandRunner = fn(
    &mut EmbeddedShell,
    &str,
    &AgentExecOpts,
    &mut dyn nosh_shell::OutputSink,
) -> Result<nosh_shell::CommandResult, nosh_shell::ShellError>;

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
    user_outputs_seen: HashSet<u64>,
    hooked: bool,
    /// Filters what the agent writes to disk (see [`tools::Redactor`]).
    redactor: Arc<dyn Redactor>,
    command_runner: CommandRunner,
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
            user_outputs_seen: HashSet::new(),
            hooked: false,
            redactor: Arc::new(NoRedact),
            command_runner: EmbeddedShell::run_agent_command,
        }
    }

    /// Replaces the filter for what the agent writes to disk; the local agent
    /// is trusted and keeps [`NoRedact`].
    pub fn with_redactor(mut self, redactor: Arc<dyn Redactor>) -> Self {
        self.redactor = redactor;
        self
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
        self.user_outputs_seen.clear();
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
                self.engine.compact_tool_results(sid, 0)?;
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
        ctx.home = shell.home().or_else(|| ctx.user_home.clone());
        ctx.variables.clear();
        ctx.variables_complete = true;
        ctx.exported.clear();
        ctx.aliases = shell.aliases();
        ctx.functions = shell.functions();
        for (name, value, exported) in shell.scalar_vars() {
            if exported {
                ctx.exported.insert(name.clone());
            }
            ctx.variables.insert(name, value);
        }
        if let Some(pwd) = shell.var("PWD") {
            ctx.variables.insert("PWD".into(), pwd);
        }
        ctx.readonly_variables = shell.readonly_variable_names();
        ctx.unknown_variables = shell
            .variable_names()
            .into_iter()
            .filter(|name| !ctx.variables.contains_key(name))
            .collect();
        ctx.execution_variables = shell.agent_environment();
        ctx.protected = self
            .cfg
            .protected
            .iter()
            .map(|path| ctx.resolve_workspace(&path.to_string_lossy()))
            .collect();
        // nosh's own settings and state wherever they are (macOS keeps them
        // under ~/Library/Application Support; NOSH_HOME, XDG_CONFIG_HOME),
        // besides the XDG defaults the analysis always protects.
        for path in [nosh_hub::paths::config_dir(), nosh_hub::paths::state_dir()] {
            ctx.protected
                .push(std::path::absolute(&path).unwrap_or(path));
        }
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
                if let Err(error) = self.engine.rewind(sid, keep) {
                    self.reset_conversation();
                    self.retain_tool_messages(&msgs);
                    return Err(error);
                }
                if let Err(error) = self.engine.compact_tool_results(sid, 0) {
                    // Rewind removed these results, but their tools have already run.
                    self.retain_tool_messages(&msgs);
                    return Err(error);
                }
                let retry_keep = self.engine.message_count(sid);
                match self.engine.step(sid, msgs.clone(), &mut sink) {
                    Ok(step) => Ok(step),
                    Err(error) => {
                        self.rollback_failed_step(sid, retry_keep, &msgs)?;
                        Err(error)
                    }
                }
            }
            Err(error) => {
                self.rollback_failed_step(sid, keep, &msgs)?;
                Err(error)
            }
            Ok(step) => Ok(step),
        }
    }

    fn retain_tool_messages(&mut self, messages: &[Message]) {
        self.carry.extend(
            messages
                .iter()
                .filter(|message| matches!(message, Message::Tool(_)))
                .cloned(),
        );
    }

    fn rollback_failed_step(
        &mut self,
        sid: SessionId,
        keep: usize,
        messages: &[Message],
    ) -> Result<(), LlmError> {
        if let Err(error) = self.engine.rewind(sid, keep) {
            self.reset_conversation();
            self.retain_tool_messages(messages);
            return Err(error);
        }
        self.retain_tool_messages(messages);
        Ok(())
    }

    /// Runs one task to completion in the shared shell session.
    pub fn run_task(
        &mut self,
        shell: &mut EmbeddedShell,
        mut input: TaskInput,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
    ) -> TaskOutcome {
        if let Some(error) = &self.cfg.rules_error {
            let error = format!("AI execution blocked by invalid safety configuration: {error}");
            ui.error(&error);
            return TaskOutcome {
                status: TaskStatus::Failed,
                error: Some(error),
                ..TaskOutcome::default()
            };
        }
        ui.state(self.cfg.mode, Activity::Thinking);
        let started = Instant::now();
        let ints = shell.interrupts();
        let cancel = self.engine.cancel_handle();
        if !self.hooked {
            let c = cancel.clone();
            ints.on_interrupt(move || c.cancel());
            self.hooked = true;
        }
        let mut out = TaskOutcome::default();
        // Keep executed results outside the history that automatic resets discard.
        let mut pending = std::mem::take(&mut self.carry);
        let sid = match self.ensure_session() {
            Ok(s) => s,
            Err(e) => {
                self.carry = pending;
                ui.error(&e.to_string());
                out.status = TaskStatus::Failed;
                out.error = Some(e.to_string());
                return out;
            }
        };
        if input
            .user_output
            .as_ref()
            .is_some_and(|output| self.user_outputs_seen.contains(&output.command_id))
        {
            input.user_output = None;
        }
        let mut attached_output = input.user_output.as_ref().map(|output| output.command_id);
        let cwd0 = shell.cwd();
        let notes =
            prompt::project_notes(&cwd0).filter(|(path, _)| !self.notes_seen.contains(path));
        let mut attached_notes = notes.as_ref().map(|(path, _)| path.clone());
        pending.push(Message::User(prompt::task_message(
            shell,
            &input,
            notes.as_ref().map(|(_, text)| text.as_str()),
        )));
        let mut errors: HashMap<String, usize> = HashMap::new();
        let mut summarizing = false;
        loop {
            ui.state(self.cfg.mode, Activity::Thinking);
            if out.steps >= self.cfg.max_steps && !summarizing {
                summarizing = true;
                pending.push(Message::User(SUMMARIZE.into()));
            }
            cancel.reset();
            out.steps += 1;
            let step = match self.step(sid, std::mem::take(&mut pending), ui) {
                Ok(s) => {
                    if let Some(command_id) = attached_output.take() {
                        self.user_outputs_seen.insert(command_id);
                    }
                    if let Some(path) = attached_notes.take() {
                        self.notes_seen.insert(path);
                    }
                    s
                }
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
            let mut handed_off = false;
            for call in &step.tool_calls {
                if denied || aborted || handed_off {
                    pending.push(Message::Tool(
                        "[skipped] an earlier call was denied, cancelled or handed to the user"
                            .into(),
                    ));
                    continue;
                }
                match self.exec_call(shell, call, approval, ui) {
                    Exec::Result(t) => {
                        pending.push(Message::Tool(t));
                    }
                    Exec::CommandResult(t) => {
                        out.commands_run += 1;
                        pending.push(Message::Tool(t));
                    }
                    Exec::Denied(t) => {
                        denied = true;
                        out.denied += 1;
                        pending.push(Message::Tool(t));
                    }
                    Exec::Handoff(cmd, t) => {
                        handed_off = true;
                        out.commands_run += 1;
                        out.proposed = Some(cmd);
                        pending.push(Message::Tool(t));
                    }
                    Exec::Aborted(t) => {
                        aborted = true;
                        out.commands_run += 1;
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
            if handed_off {
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
            ui.notice(tr!(
                "任务已取消；已经发生的副作用不会自动撤销",
                "Task cancelled; effects that already occurred are not undone."
            ));
        }
        let tps = out.usage.decode_tps();
        let u = &out.usage;
        ui.finish(&TaskSummary {
            status: out.status.as_str().into(),
            steps: out.steps,
            secs: started.elapsed().as_secs_f64(),
            prompt_tokens: u.prompt_tokens,
            cached_tokens: u.cached_tokens,
            completion_tokens: u.completion_tokens,
            prefill_tps: if u.prompt_tokens > 0 {
                u.prefill_tps()
            } else {
                0.0
            },
            decode_tps: if u.completion_tokens > 0 { tps } else { 0.0 },
            ttft_secs: u.ttft_secs,
            context_used: u.context_used,
            context_max: u.context_max,
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
    ) -> Exec {
        let Some(tool) = self.tools.resolve(&call.name) else {
            let allowed: Vec<_> = self.tools.tools().iter().map(|t| t.name()).collect();
            let error = format!(
                "error: unknown tool '{}'; available tools: {}",
                call.name,
                allowed.join(", ")
            );
            ui.error(&error);
            return Exec::Result(error);
        };
        match tool {
            BuiltinTool::RunCommand => self.run_command(shell, call, approval, ui),
            BuiltinTool::ReadFile => self.read_tool(shell, call, approval, ui, tools::read_file),
            BuiltinTool::ListDir => self.read_tool(shell, call, approval, ui, tools::list_dir),
        }
    }

    fn authorize(
        &mut self,
        tool: &str,
        detail: &str,
        report: &RiskReport,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
    ) -> Authorization {
        let policy = evaluate(report, self.cfg.mode, &self.cfg.rules, &self.allow);
        let source = policy.source.label();
        let label = format!("{} · {source}", report.risk());
        let denied = |ui: &mut dyn AgentUi, reason: String| {
            ui.notice(&format!("{tool}: {detail}"));
            ui.error(&reason);
            Authorization::Denied(format!(
                "{reason} Do not retry or bypass this decision; explain it or choose a permitted alternative."
            ))
        };
        match policy.decision {
            Decision::Deny { reason } => denied(ui, format!("[denied by policy] {reason}")),
            Decision::Allow => Authorization::Allowed {
                label,
                manual: false,
                grant: false,
            },
            Decision::Ask { strong } => {
                ui.state(self.cfg.mode, Activity::Waiting);
                let can_grant =
                    !strong && tool == "run_command" && SessionAllowList::can_grant(report);
                let mut reasons = vec![source];
                reasons.extend(report.top_reasons().into_iter().map(str::to_string));
                let req = ApprovalRequest {
                    tool: tool.into(),
                    command: detail.into(),
                    risk: if strong {
                        report.risk().max(Risk::Dangerous)
                    } else {
                        report.risk()
                    },
                    reasons,
                    strong,
                    can_grant,
                    can_edit: tool == "run_command",
                    mode: self.cfg.mode,
                };
                ui.pause();
                match approval.request(&req) {
                    ApprovalResponse::Approve => Authorization::Allowed {
                        label: format!("{} · approved once", report.risk()),
                        manual: true,
                        grant: false,
                    },
                    ApprovalResponse::ApproveSimilar if can_grant => Authorization::Allowed {
                        label: format!("{} · approved (same operation and scope)", report.risk()),
                        manual: true,
                        grant: true,
                    },
                    ApprovalResponse::Edit(command) if tool == "run_command" => {
                        Authorization::Edit(command)
                    }
                    ApprovalResponse::Deny { reason } => denied(
                        ui,
                        format!(
                            "[denied by user] Pending call was not approved; no command was run. {}",
                            reason.unwrap_or_default()
                        ),
                    ),
                    ApprovalResponse::Unavailable { reason } => {
                        denied(ui, format!("[approval unavailable] {reason}"))
                    }
                    _ => denied(
                        ui,
                        "[denied] approval response exceeds the offered authorization scope".into(),
                    ),
                }
            }
        }
    }

    fn run_command(
        &mut self,
        shell: &mut EmbeddedShell,
        call: &ToolCall,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
    ) -> Exec {
        let Some(original) = call
            .str_arg("command")
            .map(str::trim)
            .filter(|c| !c.is_empty())
        else {
            ui.error("missing required parameter 'command'");
            return Exec::Result("error: missing required parameter 'command'".into());
        };
        let timeout = call
            .int_arg("timeout_sec")
            .map(|t| Duration::from_secs(t.clamp(1, 600) as u64))
            .unwrap_or(self.cfg.command_timeout);
        let mut command = original.to_string();
        let mut edited = false;
        for _ in 0..4 {
            if command.is_empty() || command.contains('\0') {
                ui.error("invalid empty command or NUL byte");
                return Exec::Result("error: invalid command".into());
            }
            let mut ctx = self.perm_context(shell);
            ctx.timeout = timeout;
            let report = prepared_command(&command, &ctx);
            if let Some(error) = &report.syntax_error {
                ui.error(&format!("invalid command syntax: {error}"));
                return Exec::Result(format!("error: invalid command syntax: {error}"));
            }
            let shown = report.rewritten.as_deref().unwrap_or(&command);
            let label = match self.authorize("run_command", shown, &report, approval, ui) {
                Authorization::Allowed {
                    label,
                    manual,
                    grant,
                } => {
                    if manual {
                        let mut fresh = self.perm_context(shell);
                        fresh.timeout = timeout;
                        if prepared_command(&command, &fresh) != report {
                            ui.notice("The operation or its scope changed while awaiting approval; reassessing.");
                            continue;
                        }
                    }
                    if grant && !self.allow.grant(&report) {
                        ui.error("session authorization is no longer valid");
                        return Exec::Denied(
                            "[denied] session authorization is no longer valid".into(),
                        );
                    }
                    label
                }
                Authorization::Edit(c) => {
                    command = c.trim().to_string();
                    edited = true;
                    continue;
                }
                Authorization::Denied(reason) => return Exec::Denied(reason),
            };
            return self.execute(shell, &command, &report, &label, timeout, edited, ui);
        }
        ui.error("too many command edits or scope changes; the pending call was not executed");
        Exec::Denied("[denied] too many edits or scope changes".into())
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
    ) -> Exec {
        let to_run = report
            .rewritten
            .clone()
            .unwrap_or_else(|| command.to_string());
        ui.state(self.cfg.mode, Activity::Running);
        ui.tool_start("run_command", &to_run, Some(report.risk()), label);
        let opts = AgentExecOpts {
            timeout,
            ..AgentExecOpts::default()
        };
        // Ctrl-C interrupts the command; pressing it again during the same
        // command aborts the task.
        let ints_before = shell.interrupts().count();
        let r = match (self.command_runner)(shell, &to_run, &opts, &mut UiSink(ui)) {
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
            tools::save_output(id, &to_run, &r, self.redactor.as_ref())
        } else {
            None
        };
        self.outputs.push(OutputRecord {
            id,
            command: to_run.clone(),
            text: format!("{}{}", r.stdout, r.stderr),
            permission: label.to_string(),
            previous_cwd: report.context.cwd.clone(),
            previous_variables: report
                .operations
                .iter()
                .flat_map(|op| &op.variables)
                .filter(|(name, _)| !report.context.unknown_variables.contains(name))
                .map(|(name, _)| {
                    (
                        name.clone(),
                        report.context.variables.get(name).cloned(),
                        report.context.exported.contains(name),
                    )
                })
                .collect(),
        });
        if self.outputs.len() > 20 {
            self.outputs.remove(0);
        }
        let mut summary = format!("exit {} · {:.2}s", r.exit_code, r.duration.as_secs_f64());
        if r.timed_out {
            summary.push_str(" · timed out");
        }
        if r.interrupted {
            summary.push_str(" · command interrupted; prior effects are not undone");
        }
        if !r.stdout.is_empty() || !r.stderr.is_empty() {
            summary.push_str(&format!(" · ai out {id}"));
        }
        ui.tool_end(&summary);
        let mut text = tools::format_command_result(&r, log.as_deref());
        if edited {
            text = format!("[note] the user edited the command to: {to_run}\n{text}");
        }
        if shell.interrupts().count() >= ints_before + 2 {
            return Exec::Aborted(text);
        }
        if needs_handoff(&r, report.rewritten.is_some()) {
            ui.state(self.cfg.mode, Activity::NeedsUser);
            text.push_str("\n[handoff] returned the original command to the user; earlier parts of this shell program may already have run; do not retry it");
            ui.proposed(command, Some(tr!(
                "命令在等待终端或密码时已停止，但复合命令前面的部分可能已经执行；请检查当前状态和整条命令后再自行运行，不会自动重试。",
                "Command stopped while waiting for a terminal or password, but earlier parts of this shell program may already have run. Check the current state and the entire command before running it yourself; it will not be retried automatically."
            )));
            return Exec::Handoff(command.to_string(), text);
        }
        Exec::CommandResult(text)
    }

    fn read_tool(
        &mut self,
        shell: &mut EmbeddedShell,
        call: &ToolCall,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
        read: fn(&ToolCall, &Path) -> Result<String, String>,
    ) -> Exec {
        for _ in 0..4 {
            let ctx = self.perm_context(shell);
            let prepared = match tools::prepare_read(call, &ctx) {
                Ok(call) => call,
                Err(error) => {
                    ui.error(&error);
                    return Exec::Result(format!("error: {error}"));
                }
            };
            let path = tools::tool_path(&prepared, &ctx.cwd);
            let depth = prepared.int_arg("depth").map(|d| d as usize);
            let report = assess_read(&call.name, &path, depth, &ctx);
            let detail = format!("{} {}", call.name, path.display());
            let label = match self.authorize(&call.name, &detail, &report, approval, ui) {
                Authorization::Allowed { label, manual, .. } => {
                    if manual {
                        let fresh = self.perm_context(shell);
                        if fresh != ctx || assess_read(&call.name, &path, depth, &fresh) != report {
                            ui.notice(
                                "The read scope changed while awaiting approval; reassessing.",
                            );
                            continue;
                        }
                    }
                    label
                }
                Authorization::Denied(reason) => return Exec::Denied(reason),
                Authorization::Edit(_) => unreachable!("read approvals cannot edit calls"),
            };
            ui.state(self.cfg.mode, Activity::Running);
            ui.tool_start(
                &call.name,
                &path.display().to_string(),
                Some(report.risk()),
                &label,
            );
            return match read(&prepared, &ctx.cwd) {
                Ok(text) => {
                    ui.tool_end(&format!("{} lines", text.lines().count().saturating_sub(1)));
                    Exec::Result(text)
                }
                Err(error) => {
                    ui.tool_end(&format!("error: {error}"));
                    Exec::Result(format!("error: {error}"))
                }
            };
        }
        ui.error("read scope did not remain stable; the pending call was not executed");
        Exec::Denied("[denied] unstable read scope".into())
    }
}

fn prepared_command(command: &str, ctx: &Context) -> RiskReport {
    let report = assess_command(command, ctx);
    if let Some(rewritten) = report.rewritten {
        let mut report = assess_command(&rewritten, ctx);
        report.rewritten = Some(rewritten);
        report
    } else {
        report
    }
}

fn needs_handoff(r: &nosh_shell::CommandResult, sudo_rewritten: bool) -> bool {
    r.needed_terminal
        || (sudo_rewritten
            && r.stderr.lines().any(|line| {
                matches!(line.trim(),
                    "sudo: a password is required" |
                    "sudo: a terminal is required to read the password; either use the -S option to read from standard input or configure an askpass helper")
            }))
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

#[cfg(test)]
mod tests {
    use super::*;
    use nosh_llm::{CancelHandle, Event};
    use std::sync::Mutex;

    struct RecoveryEngine {
        fail: Option<&'static str>,
        calls: Arc<Mutex<Vec<&'static str>>>,
        messages: usize,
        append_failed: bool,
    }

    impl RecoveryEngine {
        fn record(&self, operation: &'static str) -> Result<(), LlmError> {
            self.calls.lock().unwrap().push(operation);
            if self.fail == Some(operation) {
                Err(LlmError::Config(format!("{operation} failed")))
            } else {
                Ok(())
            }
        }
    }

    impl ChatEngine for RecoveryEngine {
        fn open(&mut self, _spec: SessionSpec) -> Result<SessionId, LlmError> {
            self.record("open")?;
            Ok(1)
        }

        fn step(
            &mut self,
            _sid: SessionId,
            append: Vec<Message>,
            _sink: &mut dyn FnMut(Event),
        ) -> Result<StepOutcome, LlmError> {
            self.record("step")?;
            self.messages += append.len();
            if self.fail == Some("append") {
                self.fail = None;
                self.append_failed = true;
                return Err(LlmError::Config("append failed".into()));
            }
            if self.append_failed {
                return Ok(StepOutcome {
                    text: "done".into(),
                    think: String::new(),
                    tool_calls: Vec::new(),
                    errors: Vec::new(),
                    stop: StopReason::EndOfTurn,
                    usage: Usage::default(),
                });
            }
            Err(LlmError::ContextFull {
                used: 100,
                max: 100,
            })
        }

        fn rewind(&mut self, _sid: SessionId, keep: usize) -> Result<(), LlmError> {
            self.record("rewind")?;
            self.messages = keep;
            Ok(())
        }

        fn compact_tool_results(
            &mut self,
            _sid: SessionId,
            _keep_recent: usize,
        ) -> Result<usize, LlmError> {
            self.record("compact")?;
            Ok(0)
        }

        fn message_count(&self, _sid: SessionId) -> usize {
            self.messages
        }

        fn context_usage(&self, _sid: SessionId) -> (usize, usize) {
            (90, 100)
        }

        fn cancel_handle(&self) -> CancelHandle {
            CancelHandle::default()
        }

        fn close(&mut self, _sid: SessionId) {
            self.calls.lock().unwrap().push("close");
        }
    }

    fn recovery_agent(fail: Option<&'static str>) -> (Agent, Arc<Mutex<Vec<&'static str>>>) {
        let calls = Arc::default();
        let agent = Agent::new(
            Box::new(RecoveryEngine {
                fail,
                calls: Arc::clone(&calls),
                messages: 1,
                append_failed: false,
            }),
            AgentConfig::default(),
            Environment {
                os: "Linux".into(),
                arch: "x86_64".into(),
                user: "test".into(),
                available: vec![],
            },
            ToolSet::Full,
        );
        (agent, calls)
    }

    #[test]
    fn context_recovery_stops_at_the_first_error() {
        for (failure, expected) in [
            ("rewind", vec!["step", "rewind"]),
            ("compact", vec!["step", "rewind", "compact"]),
        ] {
            let (mut agent, calls) = recovery_agent(Some(failure));
            let error = agent
                .step(1, vec![], &mut crate::RecordUi::default())
                .unwrap_err();
            assert_eq!(error.to_string(), format!("{failure} failed"));
            assert_eq!(*calls.lock().unwrap(), expected);
        }
    }

    #[test]
    fn non_context_error_rolls_back_the_appended_messages() {
        let (mut agent, calls) = recovery_agent(Some("append"));
        let error = agent
            .step(
                1,
                vec![Message::User("evidence".into())],
                &mut crate::RecordUi::default(),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "append failed");
        assert_eq!(agent.engine.message_count(1), 1);
        assert_eq!(*calls.lock().unwrap(), ["step", "rewind"]);
    }

    #[test]
    fn failed_append_does_not_suppress_project_notes() {
        let directory = tempfile::tempdir().unwrap();
        let notes = directory.path().join("NOSH.md");
        std::fs::write(&notes, "project instructions").unwrap();
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
            working_dir: Some(directory.path().to_path_buf()),
            ..nosh_shell::ShellOptions::default()
        })
        .unwrap();
        let (mut agent, _) = recovery_agent(Some("append"));
        let mut ui = crate::RecordUi::default();

        let first = agent.run_task(
            &mut shell,
            TaskInput::new(nosh_shell::Trigger::Hash, "first"),
            &mut crate::Scripted::new([]),
            &mut ui,
        );
        assert_eq!(first.status, TaskStatus::Failed);
        assert!(!agent.notes_seen.contains(&notes));

        let second = agent.run_task(
            &mut shell,
            TaskInput::new(nosh_shell::Trigger::Hash, "second"),
            &mut crate::Scripted::new([]),
            &mut ui,
        );
        assert_eq!(second.status, TaskStatus::Completed);
        assert!(agent.notes_seen.contains(&notes));
    }

    #[test]
    fn compaction_failure_carries_results_without_replaying_user_messages() {
        let (mut agent, calls) = recovery_agent(Some("compact"));
        let first = Message::Tool("first executed result".into());
        let second = Message::Tool("second executed result".into());
        let error = agent
            .step(
                1,
                vec![
                    first.clone(),
                    Message::User("failed task".into()),
                    second.clone(),
                    Message::User(SUMMARIZE.into()),
                ],
                &mut crate::RecordUi::default(),
            )
            .unwrap_err();
        assert_eq!(error.to_string(), "compact failed");
        assert_eq!(agent.carry, [first, second]);
        assert_eq!(*calls.lock().unwrap(), ["step", "rewind", "compact"]);
    }

    #[test]
    fn context_recovery_only_retries_once() {
        let (mut agent, calls) = recovery_agent(None);
        assert!(matches!(
            agent.step(1, vec![], &mut crate::RecordUi::default()),
            Err(LlmError::ContextFull { .. })
        ));
        assert_eq!(
            *calls.lock().unwrap(),
            ["step", "rewind", "compact", "step", "rewind"]
        );
    }

    #[test]
    fn pre_task_compaction_failure_does_not_reset_the_conversation() {
        let (mut agent, calls) = recovery_agent(Some("compact"));
        agent.sid = Some(1);
        assert_eq!(
            agent.ensure_session().unwrap_err().to_string(),
            "compact failed"
        );
        assert_eq!(agent.sid, Some(1));
        assert_eq!(*calls.lock().unwrap(), ["compact"]);
    }

    #[test]
    fn session_setup_errors_preserve_carried_results() {
        for (failure, expected_calls) in [
            ("open", vec!["close", "open"]),
            ("compact", vec!["compact"]),
        ] {
            let (mut agent, calls) = recovery_agent(Some(failure));
            agent.sid = Some(1);
            if failure == "open" {
                agent.cfg.idle_reset = Duration::ZERO;
                agent.last_task = Some(Instant::now() - Duration::from_secs(1));
            }
            let result = Message::Tool("already executed".into());
            agent.carry.push(result.clone());
            let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
            let outcome = agent.run_task(
                &mut shell,
                TaskInput::new(nosh_shell::Trigger::Hash, "continue"),
                &mut crate::Scripted::new([]),
                &mut crate::RecordUi::default(),
            );
            assert_eq!(outcome.status, TaskStatus::Failed);
            assert_eq!(outcome.error, Some(format!("{failure} failed")));
            assert_eq!(agent.carry, [result]);
            assert_eq!(*calls.lock().unwrap(), expected_calls);
        }
    }

    #[test]
    fn explicit_conversation_reset_discards_carried_results() {
        let (mut agent, calls) = recovery_agent(None);
        agent.sid = Some(1);
        agent.carry.push(Message::Tool("already executed".into()));
        agent.notes_seen.insert(PathBuf::from("NOSH.md"));
        agent.reset_conversation();
        assert!(agent.carry.is_empty());
        assert!(agent.notes_seen.is_empty());
        assert_eq!(agent.sid, None);
        assert_eq!(*calls.lock().unwrap(), ["close"]);
    }

    #[test]
    fn password_handoff_requires_a_rewritten_sudo_and_explicit_diagnostic() {
        let mut result = nosh_shell::CommandResult {
            exit_code: 1,
            stderr: "sudo: a password is required\n".into(),
            ..Default::default()
        };
        assert!(needs_handoff(&result, true));
        assert!(!needs_handoff(&result, false));
        result.exit_code = 0;
        assert!(
            needs_handoff(&result, true),
            "a later successful command must not mask sudo's diagnostic"
        );
        result.stderr = "sudo: user is not in the sudoers file\n".into();
        assert!(!needs_handoff(&result, true));
        result.needed_terminal = true;
        assert!(needs_handoff(&result, false));
    }
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use crate::{RecordUi, Scripted};
    use nosh_llm::MockChatEngine;
    use nosh_permissions::{
        ApprovalMode::{Auto, Confirm, Yolo},
        Risk, UserRule,
    };
    use std::cell::RefCell;

    thread_local! {
        static EXECUTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
    }

    fn fake_run(
        _shell: &mut EmbeddedShell,
        command: &str,
        _opts: &AgentExecOpts,
        _sink: &mut dyn nosh_shell::OutputSink,
    ) -> Result<nosh_shell::CommandResult, nosh_shell::ShellError> {
        EXECUTED.with(|calls| calls.borrow_mut().push(command.into()));
        Ok(nosh_shell::CommandResult::default())
    }

    fn fake_agent(mode: ApprovalMode, rules: UserRules) -> Agent {
        EXECUTED.with(|calls| calls.borrow_mut().clear());
        let mut agent = Agent::new(
            Box::new(MockChatEngine::new(vec![])),
            AgentConfig {
                mode,
                rules,
                ..AgentConfig::default()
            },
            Environment {
                os: "Linux".into(),
                arch: "test".into(),
                user: "test".into(),
                available: vec![],
            },
            ToolSet::Full,
        );
        agent.command_runner = fake_run;
        agent
    }

    fn call(command: &str) -> ToolCall {
        ToolCall {
            name: "run_command".into(),
            args: serde_json::json!({ "command": command })
                .as_object()
                .unwrap()
                .clone(),
        }
    }

    fn count() -> usize {
        EXECUTED.with(|calls| calls.borrow().len())
    }

    #[test]
    fn full_matrix_counts_real_dispatch_without_executing_harmful_commands() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        for mode in [Confirm, Auto, Yolo] {
            let allow = UserRule::prefix("rm").unwrap();
            let mut agent = fake_agent(
                mode,
                UserRules {
                    allow: vec![allow.clone()],
                    deny: vec![allow.clone()],
                },
            );
            let mut approvals = Scripted::new([ApprovalResponse::Approve]);
            let mut ui = RecordUi::default();
            assert!(matches!(
                agent.exec_call(&mut shell, &call("rm -rf /"), &mut approvals, &mut ui),
                Exec::Denied(_)
            ));
            assert!(approvals.seen.is_empty());
            assert_eq!(count(), 0);
            assert!(ui.events.iter().any(|e| e.contains("user deny")));

            let mut agent = fake_agent(
                mode,
                UserRules {
                    allow: vec![allow],
                    deny: vec![],
                },
            );
            let mut approvals = Scripted::new([]);
            assert!(matches!(
                agent.exec_call(&mut shell, &call("rm -rf /"), &mut approvals, &mut ui),
                Exec::CommandResult(_)
            ));
            assert!(approvals.seen.is_empty());
            assert_eq!(count(), 1);

            let mut agent = fake_agent(mode, UserRules::default());
            let mut approvals = Scripted::new([ApprovalResponse::Approve]);
            let result = agent.exec_call(&mut shell, &call("rm -rf /"), &mut approvals, &mut ui);
            if mode == Confirm {
                assert!(matches!(result, Exec::CommandResult(_)));
                assert_eq!(approvals.seen.len(), 1);
                assert!(approvals.seen[0].strong);
                assert!(!approvals.seen[0].can_grant);
                assert_eq!(count(), 1);
                assert!(agent.allow.is_empty());
            } else {
                assert!(matches!(result, Exec::Denied(_)));
                assert!(approvals.seen.is_empty());
                assert_eq!(count(), 0);
            }

            let mut agent = fake_agent(mode, UserRules::default());
            let mut approvals = Scripted::new([ApprovalResponse::Approve]);
            agent.exec_call(&mut shell, &call("rm -rf build"), &mut approvals, &mut ui);
            assert_eq!(count(), 1);
            assert_eq!(approvals.seen.len(), usize::from(mode != Yolo));
            assert!(approvals.seen.iter().all(|req| req.strong));
        }
    }

    #[test]
    fn edits_and_safe_rewrites_do_not_add_approval_to_valid_whitelists() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let mut agent = fake_agent(
            Confirm,
            UserRules {
                allow: vec![UserRule::prefix("rm").unwrap()],
                deny: vec![],
            },
        );
        let mut approval = Scripted::new([ApprovalResponse::Edit("rm -rf /".into())]);
        agent.exec_call(
            &mut shell,
            &call("touch unapproved"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(approval.seen.len(), 1);
        assert_eq!(count(), 1);
        EXECUTED.with(|calls| assert_eq!(&*calls.borrow(), &["rm -rf /"]));

        let mut agent = fake_agent(
            Confirm,
            UserRules {
                allow: vec![
                    UserRule::exact("sudo echo hello").unwrap(),
                    UserRule::exact("echo hello").unwrap(),
                ],
                deny: vec![],
            },
        );
        let mut approval = Scripted::new([]);
        agent.exec_call(
            &mut shell,
            &call("sudo echo hello"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert!(approval.seen.is_empty());
        assert_eq!(count(), 1);
        EXECUTED.with(|calls| assert_eq!(&*calls.borrow(), &["sudo -n echo hello"]));
    }

    #[test]
    fn invalid_grants_and_missing_terminals_never_execute() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let mut agent = fake_agent(Confirm, UserRules::default());
        let mut approval = Scripted::new([ApprovalResponse::ApproveSimilar]);
        assert!(matches!(
            agent.exec_call(
                &mut shell,
                &call("rm -rf /"),
                &mut approval,
                &mut RecordUi::default()
            ),
            Exec::Denied(_)
        ));
        assert_eq!(count(), 0);
        assert!(agent.allow.is_empty());
        let mut ui = RecordUi::default();
        assert!(matches!(
            agent.exec_call(
                &mut shell,
                &call("cargo test"),
                &mut crate::NoTerminal,
                &mut ui
            ),
            Exec::Denied(_)
        ));
        assert_eq!(count(), 0);
        assert!(ui.events.iter().any(|e| e.contains("approval unavailable")));
    }

    #[test]
    fn common_builds_execute_once_but_cannot_smuggle_an_extra_call() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let mut agent = fake_agent(Auto, UserRules::default());
        let mut approval = Scripted::new([]);
        agent.exec_call(
            &mut shell,
            &call("cargo test"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(count(), 1);
        assert!(approval.seen.is_empty());
        assert!(matches!(
            agent.exec_call(
                &mut shell,
                &call("cargo test && rm -rf /"),
                &mut approval,
                &mut RecordUi::default()
            ),
            Exec::Denied(_)
        ));
        assert_eq!(count(), 1);
        assert!(approval.seen.is_empty());
    }

    #[test]
    fn policy_uses_the_same_environment_overlay_as_execution() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        shell.run_user_line("PAGER=less");
        let mut agent = fake_agent(
            Confirm,
            UserRules {
                allow: vec![],
                deny: vec![UserRule::exact("echo cat").unwrap()],
            },
        );
        let mut approval = Scripted::new([]);
        assert!(matches!(
            agent.exec_call(
                &mut shell,
                &call("echo \"$PAGER\""),
                &mut approval,
                &mut RecordUi::default()
            ),
            Exec::Denied(_)
        ));
        assert_eq!(count(), 0);
        assert!(approval.seen.is_empty());
        assert_eq!(
            shell.var("PAGER").as_deref(),
            Some("less"),
            "analysis must not mutate the session"
        );
    }

    #[test]
    fn changed_symlink_scope_is_reassessed_before_dispatch() {
        struct Repoint {
            link: PathBuf,
            protected: PathBuf,
            seen: Vec<ApprovalRequest>,
        }
        impl ApprovalChannel for Repoint {
            fn request(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
                self.seen.push(req.clone());
                if self.seen.len() == 1 {
                    std::fs::remove_file(&self.link).unwrap();
                    std::os::unix::fs::symlink(&self.protected, &self.link).unwrap();
                    ApprovalResponse::Approve
                } else {
                    ApprovalResponse::Deny { reason: None }
                }
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let path = root.join("out");
        let protected = root.join("protected");
        std::fs::write(&path, "old").unwrap();
        std::fs::write(&protected, "keep").unwrap();
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let mut agent = fake_agent(Confirm, UserRules::default());
        agent.cfg.protected.push(protected.clone());
        let mut approval = Repoint {
            link: path.clone(),
            protected: protected.clone(),
            seen: vec![],
        };
        let command = format!("printf x > '{}'", path.display());
        assert!(matches!(
            agent.exec_call(
                &mut shell,
                &call(&command),
                &mut approval,
                &mut RecordUi::default()
            ),
            Exec::Denied(_)
        ));
        assert_eq!(approval.seen.len(), 2);
        assert_eq!(approval.seen[1].risk, Risk::Dangerous);
        assert_eq!(count(), 0);
        assert_eq!(std::fs::read_to_string(protected).unwrap(), "keep");
    }

    #[test]
    fn additional_effects_wait_for_approval_before_any_execution() {
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        for name in ["first", "second", "existing"] {
            std::fs::write(root.join(name), name).unwrap();
        }
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        shell.run_user_line(&format!("cd '{}'", root.display()));
        shell.set_workspace(root.clone());
        let mut agent = fake_agent(Auto, UserRules::default());
        let mut approvals = Scripted::new([]);
        let commands = [
            "ping -c 1 router.local > existing",
            "mvn test deploy",
            "mv first /dev/null",
            "cp first /etc/file",
            "printf new > missing",
            "npm run build --prefix=/etc",
            "mvn test --file=/etc/pom.xml",
        ];
        for command in commands {
            assert!(matches!(
                agent.exec_call(
                    &mut shell,
                    &call(command),
                    &mut approvals,
                    &mut RecordUi::default()
                ),
                Exec::Denied(_)
            ));
        }
        assert_eq!(approvals.seen.len(), commands.len());
        assert_eq!(count(), 0);
        for name in ["first", "second", "existing"] {
            assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), name);
        }
        assert!(!root.join("missing").exists());
    }

    #[test]
    fn routine_copy_and_move_execute_without_approval() {
        let directory = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(directory.path()).unwrap();
        std::fs::write(root.join("source"), "content").unwrap();
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
            working_dir: Some(root.clone()),
            ..Default::default()
        })
        .unwrap();
        shell.set_workspace(root.clone());
        let mut agent = fake_agent(Auto, UserRules::default());
        agent.command_runner = EmbeddedShell::run_agent_command;
        let mut approvals = Scripted::new([]);
        let mut ui = RecordUi::default();
        let result = agent.exec_call(
            &mut shell,
            &call("cp source copied && mv copied moved"),
            &mut approvals,
            &mut ui,
        );
        assert!(matches!(result, Exec::CommandResult(_)));
        assert!(approvals.seen.is_empty());
        assert_eq!(
            std::fs::read_to_string(root.join("moved")).unwrap(),
            "content"
        );
        assert!(!root.join("copied").exists());
        assert!(ui.events.iter().any(|event| event.contains("ordinary cp")));
        assert!(ui.events.iter().any(|event| event.contains("ordinary mv")));
    }

    #[test]
    fn scoped_script_and_deep_read_denies_never_ask_or_execute() {
        use nosh_permissions::RuleSpec;
        let dir = tempfile::tempdir().unwrap();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        std::fs::write(root.join("trusted.sh"), "bash -c 'printf new > blocked'\n").unwrap();
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        shell.run_user_line(&format!("cd '{}'", root.display()));
        shell.set_workspace(root.clone());
        for mode in [Confirm, Auto, Yolo] {
            let rule = UserRule::compile(
                RuleSpec {
                    command_exact: Some("./trusted.sh".into()),
                    write_paths: vec!["blocked".into()],
                    ..RuleSpec::default()
                },
                "script deny",
            )
            .unwrap();
            let read_rule = UserRule::compile(
                RuleSpec {
                    tool: Some("list_dir".into()),
                    path: Some("**".into()),
                    max_depth: Some(1),
                    ..RuleSpec::default()
                },
                "directory deny",
            )
            .unwrap();
            let mut agent = fake_agent(
                mode,
                UserRules {
                    allow: vec![UserRule::prefix("./trusted.sh").unwrap()],
                    deny: vec![rule, read_rule],
                },
            );
            let mut approvals = Scripted::new([]);
            let mut ui = RecordUi::default();
            let read = ToolCall {
                name: "list_dir".into(),
                args: serde_json::json!({"path": ".", "depth": 3})
                    .as_object()
                    .unwrap()
                    .clone(),
            };
            for call in [call("./trusted.sh"), read] {
                assert!(matches!(
                    agent.exec_call(&mut shell, &call, &mut approvals, &mut ui),
                    Exec::Denied(_)
                ));
            }
            assert!(approvals.seen.is_empty());
            assert_eq!(count(), 0);
            assert!(ui.events.iter().any(|event| event.contains("user deny")));
        }
        assert!(!root.join("blocked").exists());
    }
}
