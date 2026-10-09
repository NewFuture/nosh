//! The task loop (design §5.3): task message → model step → tool calls, each
//! risk-assessed and approved as needed → results back to the model, until
//! it answers, the step limit is hit, or the user cancels.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use nosh_engine::{
    ChatEngine, EngineError, Message, SamplingParams, SessionId, SessionSpec, StepOutcome,
    StopReason, ToolCall, Usage,
};
use nosh_permissions::{
    ApprovalMode, Context, Decision, PathClass, Risk, RiskReport, SessionAllowList, UserRules,
    assess_command_with_lookup, assess_read, classify_path_real, evaluate,
};
use nosh_platform::tr;
use nosh_shell::{AgentExecOpts, EmbeddedShell};

use crate::approval::{ApprovalChannel, ApprovalRequest, ApprovalResponse};
use crate::prompt::{self, Environment, TaskInput};
use crate::tools::{self, BuiltinTool, NoRedact, Redactor, ToolSet};
use crate::ui::{Activity, AgentUi, TaskSummary, UiSink};
use crate::user_input::{self, InputError, NoUserInput, UserInput};

#[derive(Debug, Clone)]
pub struct AgentConfig {
    pub command_prefix: String,
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
            command_prefix: "#".into(),
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

impl AgentConfig {
    /// Live paths and settings shared by command assessment and automatic context reads.
    pub fn permission_context(&self, shell: &EmbeddedShell) -> Context {
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
            .protected
            .iter()
            .map(|path| ctx.resolve_workspace(&path.to_string_lossy()))
            .collect();
        for path in [
            nosh_platform::paths::config_dir(),
            nosh_platform::paths::state_dir(),
        ] {
            ctx.protected
                .push(std::path::absolute(&path).unwrap_or(path));
        }
        ctx
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

/// Full output of an agent command, for `#out <id>`.
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
    UserAnswer(String),
    CommandResult(String),
    Denied(String),
    Handoff(String, String),
    Aborted(String),
    Cancelled(String),
    Failed(String),
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
    user_outputs_seen: HashSet<u64>,
    guidance: crate::guidance::GuidanceCache,
    guidance_sent: Option<String>,
    hooked: bool,
    /// Filters what the agent writes to disk (see [`tools::Redactor`]).
    redactor: Arc<dyn Redactor>,
    command_runner: CommandRunner,
    user_input: Box<dyn UserInput>,
    can_ask: bool,
}

const SUMMARIZE: &str = "Step limit reached for the preceding user request only. Finish it without tools, with supported results and the next step in its language. Later user requests may use tools normally.";

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
            user_outputs_seen: HashSet::new(),
            guidance: crate::guidance::GuidanceCache::default(),
            guidance_sent: None,
            hooked: false,
            redactor: Arc::new(NoRedact),
            command_runner: EmbeddedShell::run_agent_command,
            user_input: Box::new(NoUserInput),
            can_ask: false,
        }
    }

    /// Replaces the filter for what the agent writes to disk; the local agent
    /// is trusted and keeps [`NoRedact`].
    pub fn with_redactor(mut self, redactor: Arc<dyn Redactor>) -> Self {
        self.redactor = redactor;
        self
    }

    pub fn with_user_input(mut self, input: Box<dyn UserInput>) -> Self {
        self.reset_conversation();
        self.user_input = input;
        self.can_ask = false;
        self
    }

    pub fn engine_mut(&mut self) -> &mut dyn ChatEngine {
        self.engine.as_mut()
    }

    /// Starts a new conversation (`#clear`, idle timeout, config change).
    pub fn reset_conversation(&mut self) {
        if let Some(sid) = self.sid.take() {
            self.engine.close(sid);
        }
        self.carry.clear();
        self.user_outputs_seen.clear();
        self.guidance_sent = None;
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
        let system = prompt::system_prompt(&self.env, self.tools);
        let mut tools = tools::specs(self.tools);
        if self.can_ask {
            tools.push(user_input::spec());
        }
        SessionSpec {
            label: "agent".into(),
            system,
            tools,
            thinking: self.cfg.thinking,
            sampling: self.cfg.sampling,
            max_new_tokens: self.cfg.max_new_tokens,
        }
    }

    fn ensure_session(&mut self) -> Result<SessionId, EngineError> {
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

    fn step(
        &mut self,
        sid: SessionId,
        msgs: Vec<Message>,
        ui: &mut dyn AgentUi,
    ) -> Result<StepOutcome, EngineError> {
        let mut sink = |ev: nosh_engine::Event| match ev {
            nosh_engine::Event::Text(t) => ui.text(&t),
            nosh_engine::Event::Think(t) => ui.think(&t),
            nosh_engine::Event::Prefill { done, total } => ui.prefill(done, total),
            _ => {}
        };
        let keep = self.engine.message_count(sid);
        match self.engine.step(sid, msgs.clone(), &mut sink) {
            Err(EngineError::ContextFull { .. }) => {
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
                .filter(|message| matches!(message, Message::Tool(_) | Message::UserAnswer(_)))
                .cloned(),
        );
    }

    fn rollback_failed_step(
        &mut self,
        sid: SessionId,
        keep: usize,
        messages: &[Message],
    ) -> Result<(), EngineError> {
        if let Err(error) = self.engine.rewind(sid, keep) {
            self.reset_conversation();
            self.retain_tool_messages(messages);
            return Err(error);
        }
        self.retain_tool_messages(messages);
        Ok(())
    }

    fn project_documents(
        &mut self,
        context: &Context,
        sid: SessionId,
    ) -> (Option<String>, Option<String>) {
        let guidance = self.guidance.load(context);
        if self.guidance_sent.as_deref() == Some(guidance.key.as_str()) {
            return (None, None);
        }
        // Even an incomplete replacement clears the previously delivered scope.
        self.guidance_sent = None;
        let mut text = guidance.text;
        if self.engine.message_count(sid) > 0 {
            text.insert_str(0, "[project documents cleared]\n");
        }
        (
            (!text.is_empty()).then_some(text),
            guidance.complete.then_some(guidance.key),
        )
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
        let environment_interrupts = shell.interrupts().count();
        if self.tools == ToolSet::Full
            && let Err(error) = shell.refresh_project_env()
        {
            ui.error(&error);
            return TaskOutcome {
                status: if shell.interrupts().count() != environment_interrupts {
                    TaskStatus::Cancelled
                } else {
                    TaskStatus::Failed
                },
                error: Some(error),
                ..TaskOutcome::default()
            };
        }
        let started = Instant::now();
        let ints = shell.interrupts();
        let cancel = self.engine.cancel_handle();
        if !self.hooked {
            let c = cancel.clone();
            ints.on_interrupt(move || c.cancel());
            self.hooked = true;
        }
        cancel.reset();
        let mut out = TaskOutcome::default();
        // Keep executed results outside the history that automatic resets discard.
        let mut pending = std::mem::take(&mut self.carry);
        let can_ask = self.user_input.available();
        if self.can_ask != can_ask {
            self.reset_conversation();
            self.can_ask = can_ask;
        }
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
        let permission_context = self.cfg.permission_context(shell);
        let (notes, mut guidance_key) = self.project_documents(&permission_context, sid);
        pending.extend(prompt::task_messages(
            shell,
            &input,
            notes.as_deref(),
            &permission_context,
        ));
        let mut errors: HashMap<String, usize> = HashMap::new();
        let mut summarizing = false;
        loop {
            if cancel.is_cancelled() {
                out.status = TaskStatus::Cancelled;
                self.carry = pending;
                break;
            }
            ui.state(self.cfg.mode, Activity::Thinking);
            if out.steps >= self.cfg.max_steps && !summarizing {
                summarizing = true;
                pending.push(Message::System(SUMMARIZE.into()));
            }
            out.steps += 1;
            let step = match self.step(sid, std::mem::take(&mut pending), ui) {
                Ok(s) => {
                    if let Some(command_id) = attached_output.take() {
                        self.user_outputs_seen.insert(command_id);
                    }
                    s
                }
                Err(e) => {
                    self.guidance_sent = None;
                    ui.error(&e.to_string());
                    out.status = TaskStatus::Failed;
                    out.error = Some(e.to_string());
                    break;
                }
            };
            if let Some(key) = guidance_key.take() {
                self.guidance_sent = Some(key);
            }
            add_usage(&mut out.usage, &step.usage);
            out.answer = step.text.trim().to_string();
            if step.stop == StopReason::Cancelled {
                self.guidance_sent = None;
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
            let mut fatal = None;
            let has_question = step.tool_calls.iter().any(|call| call.name == "ask_user")
                || step
                    .errors
                    .iter()
                    .any(|error| error.tool.as_deref() == Some("ask_user"));
            let mixed_question = has_question && step.tool_calls.len() + step.errors.len() > 1;
            if mixed_question {
                ui.error("ask_user must be the only tool call in its turn; no tools were executed");
            }
            let calls_cwd = shell.cwd();
            for call in &step.tool_calls {
                if mixed_question {
                    pending.push(Message::Tool("error: ask_user must be the only tool call in its turn; no tools were executed".into()));
                    continue;
                }
                if denied || aborted || handed_off || fatal.is_some() {
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
                    Exec::UserAnswer(answer) => {
                        pending.push(Message::UserAnswer(answer));
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
                    Exec::Cancelled(t) => {
                        aborted = true;
                        pending.push(Message::Tool(t));
                    }
                    Exec::Failed(t) => {
                        fatal = Some(t.clone());
                        pending.push(Message::Tool(t));
                    }
                }
            }
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
                self.guidance_sent = None;
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
            if shell.cwd() != calls_cwd {
                let context = self.cfg.permission_context(shell);
                let (notes, key) = self.project_documents(&context, sid);
                let mut text = crate::project::describe(&context);
                if let Some(notes) = notes {
                    text.push('\n');
                    text.push_str(notes.trim_end());
                }
                guidance_key = key;
                pending.push(Message::System(text));
            }
        }
        if out.status == TaskStatus::Completed && out.denied > 0 && out.commands_run == 0 {
            out.status = TaskStatus::Incomplete;
        }
        let cwd1 = shell.cwd();
        if cwd1 != cwd0 {
            if self.cfg.restore_cwd {
                if let Err(error) = shell.restore_working_dir(&cwd0) {
                    let error = format!("could not restore task directory: {error}");
                    ui.error(&error);
                    out.error = Some(error);
                    if out.status != TaskStatus::Cancelled {
                        out.status = TaskStatus::Failed;
                    }
                }
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
        if call.name == "ask_user" && self.can_ask {
            ui.pause();
            ui.tool_start(
                "ask_user",
                call.str_arg("question").unwrap_or(""),
                None,
                "user input",
            );
            ui.state(self.cfg.mode, Activity::NeedsUser);
            ui.pause();
            return match user_input::ask(
                self.user_input.as_mut(),
                call,
                &self.engine.cancel_handle(),
            ) {
                Ok(answer) => {
                    ui.tool_end("answered");
                    Exec::UserAnswer(
                        serde_json::json!({
                            "question": answer.question.question,
                            "choices": answer.question.choices,
                            "answer": answer.answer,
                        })
                        .to_string(),
                    )
                }
                Err(InputError::Cancelled) => {
                    ui.tool_end("cancelled");
                    Exec::Cancelled("[user input cancelled]".into())
                }
                Err(error @ InputError::Unavailable(_)) => {
                    ui.tool_end("unavailable");
                    Exec::Failed(error.to_string())
                }
                Err(error) => {
                    ui.error(&error.to_string());
                    Exec::Result(format!("error: {error}"))
                }
            };
        }
        let Some(tool) = self.tools.resolve(&call.name) else {
            let mut allowed: Vec<_> = self.tools.tools().iter().map(|t| t.name()).collect();
            if self.can_ask {
                allowed.push("ask_user");
            }
            let error = format!(
                "error: unknown tool '{}'; available tools: {}",
                call.name,
                allowed.join(", ")
            );
            ui.error(&error);
            return Exec::Result(error);
        };
        match tool {
            BuiltinTool::Exec => self.exec(shell, call, approval, ui),
            BuiltinTool::ReadFile | BuiltinTool::Grep => {
                self.read_tool(shell, call, approval, ui, tool)
            }
        }
    }

    fn authorize(
        &mut self,
        tool: &str,
        detail: &str,
        cwd: &std::path::Path,
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
                let can_grant = !strong && tool == "exec" && SessionAllowList::can_grant(report);
                let mut reasons = vec![source];
                reasons.extend(report.top_reasons().into_iter().map(str::to_string));
                let req = ApprovalRequest {
                    tool: tool.into(),
                    command: detail.into(),
                    cwd: cwd.to_path_buf(),
                    risk: if strong {
                        report.risk().max(Risk::Dangerous)
                    } else {
                        report.risk()
                    },
                    reasons,
                    strong,
                    can_grant,
                    can_edit: tool == "exec",
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
                    ApprovalResponse::Edit(command) if tool == "exec" => {
                        Authorization::Edit(command)
                    }
                    ApprovalResponse::Deny { reason } => denied(
                        ui,
                        format!(
                            "[denied by user] Pending call was not approved; no command was run.\n\
Approval request: {}; {}.{}",
                            req.risk,
                            req.reasons.join("; "),
                            reason
                                .map(|reason| format!("\nUser reason: {reason}."))
                                .unwrap_or_default()
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

    fn exec(
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
            if let Err(result) = Self::refresh_environment(shell, ui) {
                return result;
            }
            if command.is_empty() || command.contains('\0') {
                ui.error("invalid empty command or NUL byte");
                return Exec::Result("error: invalid command".into());
            }
            let mut ctx = self.cfg.permission_context(shell);
            ctx.timeout = timeout;
            let report = prepared_command(&command, &ctx, shell);
            if let Some(error) = &report.syntax_error {
                ui.error(&format!("invalid command syntax: {error}"));
                return Exec::Result(format!("error: invalid command syntax: {error}"));
            }
            let shown = report.rewritten.as_deref().unwrap_or(&command);
            let label = match self.authorize("exec", shown, &ctx.cwd, &report, approval, ui) {
                Authorization::Allowed {
                    label,
                    manual,
                    grant,
                } => {
                    if manual {
                        if let Err(result) = Self::refresh_environment(shell, ui) {
                            return result;
                        }
                        let mut fresh = self.cfg.permission_context(shell);
                        fresh.timeout = timeout;
                        if prepared_command(&command, &fresh, shell) != report {
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
        ui.tool_start("exec", &to_run, Some(report.risk()), label);
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
        if (!r.stdout.is_empty() || !r.stderr.is_empty()) && !self.cfg.command_prefix.is_empty() {
            summary.push_str(&format!(
                " · {}out {id}",
                nosh_shell::style::visible_text(&self.cfg.command_prefix)
            ));
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

    fn refresh_environment(shell: &mut EmbeddedShell, ui: &mut dyn AgentUi) -> Result<(), Exec> {
        let interrupts = shell.interrupts().count();
        shell.refresh_project_env().map_err(|error| {
            ui.error(&error);
            if shell.interrupts().count() != interrupts {
                Exec::Cancelled(error)
            } else {
                Exec::Failed(format!("[environment not ready] {error}"))
            }
        })
    }

    fn read_tool(
        &mut self,
        shell: &mut EmbeddedShell,
        call: &ToolCall,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
        tool: BuiltinTool,
    ) -> Exec {
        if self.tools == ToolSet::Full
            && let Err(result) = Self::refresh_environment(shell, ui)
        {
            return result;
        }
        let ctx = self.cfg.permission_context(shell);
        let prepared = match tools::prepare_read(call, &ctx) {
            Ok(call) => call,
            Err(error) => {
                ui.error(&error);
                return Exec::Result(format!("error: {error}"));
            }
        };
        let path = tools::tool_path(&prepared, &ctx.cwd);
        let (risk, label, manual) = match self.approve_read(&path, &ctx, &prepared, approval, ui) {
            Ok(value) => value,
            Err(error) => return error,
        };
        ui.state(self.cfg.mode, Activity::Running);
        ui.tool_start(&call.name, &path.display().to_string(), Some(risk), &label);
        let root_protected = matches!(
            classify_path_real(&path, &ctx, true).0,
            PathClass::Protected(_)
        );
        let mut authorized = if root_protected && manual {
            vec![nosh_permissions::real_path(&path, true).unwrap_or_else(|| path.clone())]
        } else {
            Vec::new()
        };
        let mut denied = None;
        let cancel = self.engine.cancel_handle();
        let result = match tool {
            BuiltinTool::ReadFile => tools::read_file(&prepared, &ctx.cwd),
            BuiltinTool::Grep => tools::grep(
                &prepared,
                &ctx.cwd,
                self.cfg.command_timeout,
                &|| cancel.is_cancelled(),
                |child| {
                    let resolved = nosh_permissions::real_path(child, true)
                        .unwrap_or_else(|| child.to_path_buf());
                    if child != path {
                        if authorized.iter().any(|root| resolved.starts_with(root)) {
                            let report = assess_read(&call.name, child, None, &ctx);
                            if !matches!(
                                evaluate(&report, self.cfg.mode, &self.cfg.rules, &self.allow)
                                    .decision,
                                Decision::Deny { .. }
                            ) {
                                return Ok(());
                            }
                        }
                        let manual = match self.approve_read(child, &ctx, &prepared, approval, ui) {
                            Ok((_, _, manual)) => manual,
                            Err(error) => {
                                denied = Some(error);
                                return Err("reading a protected grep path was denied".into());
                            }
                        };
                        if manual
                            && matches!(
                                classify_path_real(child, &ctx, true).0,
                                PathClass::Protected(_)
                            )
                        {
                            authorized.push(resolved);
                        }
                    }
                    Ok(())
                },
            ),
            BuiltinTool::Exec => unreachable!("commands are dispatched separately"),
        };
        if tool == BuiltinTool::Grep && cancel.is_cancelled() {
            ui.tool_end("cancelled");
            return Exec::Cancelled("[cancelled by the user]".into());
        }
        if let Some(error) = denied {
            ui.tool_end("protected grep path denied");
            return error;
        }
        match result {
            Ok(text) => {
                ui.tool_end(&format!("{} lines", text.lines().count().saturating_sub(1)));
                Exec::Result(text)
            }
            Err(error) => {
                ui.tool_end(&format!("error: {error}"));
                Exec::Result(format!("error: {error}"))
            }
        }
    }

    fn approve_read(
        &mut self,
        path: &Path,
        ctx: &Context,
        call: &ToolCall,
        approval: &mut dyn ApprovalChannel,
        ui: &mut dyn AgentUi,
    ) -> Result<(Risk, String, bool), Exec> {
        let report = assess_read(&call.name, path, None, ctx);
        let detail = format!("{} {}", call.name, path.display());
        match self.authorize(&call.name, &detail, &ctx.cwd, &report, approval, ui) {
            Authorization::Allowed { label, manual, .. } => Ok((report.risk(), label, manual)),
            Authorization::Denied(reason) => Err(Exec::Denied(reason)),
            Authorization::Edit(_) => unreachable!("read approvals cannot edit calls"),
        }
    }
}

fn prepared_command(command: &str, ctx: &Context, shell: &EmbeddedShell) -> RiskReport {
    let lookup = |name: &str, cwd: &Path, path: Option<&str>, use_cache| {
        shell.resolve_program_at(name, cwd, path, use_cache)
    };
    let report = assess_command_with_lookup(command, ctx, &lookup);
    if let Some(rewritten) = report.rewritten {
        let mut report = assess_command_with_lookup(&rewritten, ctx, &lookup);
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
mod tests;

#[cfg(test)]
mod permission_tests;
