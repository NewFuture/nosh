//! Short, query-only command assistance. Task execution remains in Agent.

use std::path::Path;
use std::time::{Duration, Instant};

use nosh_llm::{
    CancelHandle, ChatEngine, LlmError, Message, SessionSpec, StopReason, ToolCall, ToolSpec, Usage,
};
use nosh_permissions::{Context, Decision, Risk, SessionAllowList, assess_read, evaluate};
use nosh_shell::{CommandSnapshot, EmbeddedShell, UserCommand, UserOutput};
use serde_json::json;

use crate::{AgentConfig, prompt, tools};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Intent {
    Generate,
    Fix,
    Next,
}

impl Intent {
    pub fn name(self) -> &'static str {
        match self {
            Self::Generate => "generate",
            Self::Fix => "fix",
            Self::Next => "next",
        }
    }

    fn instruction(self) -> &'static str {
        match self {
            Self::Generate => {
                "Generate or revise the requested shell program, preserving named inputs and output format. If an essential user choice is missing, finish with clarify before querying."
            }
            Self::Fix => {
                "Use the recorded failure to propose a corrected command preserving the intended operation. Account for possible partial execution before recommending a retry."
            }
            Self::Next => {
                "Suggest one useful next command supported by the known goal and completed command. Return none when there is no justified next step."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistResult {
    Command(String),
    Clarify(String),
    NoSuggestion,
}

#[derive(Debug, Clone)]
pub struct AssistOutcome {
    pub result: AssistResult,
    pub steps: usize,
    pub usage: Usage,
}

#[derive(Debug, thiserror::Error)]
pub enum AssistError {
    #[error("command assistance cancelled")]
    Cancelled,
    #[error("command assistance budget exhausted")]
    Budget,
    #[error("command assistance: {0}")]
    Protocol(String),
    #[error(transparent)]
    Engine(#[from] LlmError),
}

#[derive(Debug, Clone)]
pub(crate) struct AssistRequest {
    pub intent: Intent,
    pub text: String,
    pub command: Option<UserCommand>,
    pub output: Option<UserOutput>,
    pub commands: CommandSnapshot,
    pub context: Context,
    pub background: bool,
}

impl AssistRequest {
    pub fn capture(
        shell: &EmbeddedShell,
        cfg: &AgentConfig,
        intent: Intent,
        text: String,
        command: Option<UserCommand>,
        output: Option<UserOutput>,
    ) -> Result<Self, AssistError> {
        if let Some(error) = &cfg.rules_error {
            return Err(AssistError::Protocol(format!(
                "invalid safety configuration: {error}"
            )));
        }
        if intent != Intent::Generate && command.is_none() {
            return Err(AssistError::Protocol("missing execution record".into()));
        }
        if let Some(command) = &command
            && ((intent == Intent::Next && command.exit != 0)
                || (intent == Intent::Fix
                    && !nosh_shell::trigger::failure_is_notable(&command.line, command.exit)))
        {
            return Err(AssistError::Protocol(
                "execution does not match assistance intent".into(),
            ));
        }
        if output.as_ref().is_some_and(|output| {
            intent != Intent::Fix
                || command.as_ref().is_none_or(|command| {
                    let cwd = command.cwd.to_string_lossy();
                    let (cwd, truncated) = nosh_shell::user_output::bounded_metadata(&cwd);
                    command.id != output.command_id
                        || command.exit != output.exit
                        || cwd != output.cwd
                        || truncated != output.cwd_truncated
                })
        }) {
            return Err(AssistError::Protocol(
                "output does not match the recorded failure".into(),
            ));
        }
        Ok(Self {
            intent,
            text,
            command,
            output,
            commands: CommandSnapshot::capture(shell).map_err(AssistError::Protocol)?,
            context: cfg.permission_context(shell),
            background: false,
        })
    }

    fn messages(&self) -> Result<Vec<Message>, AssistError> {
        let guidance = crate::guidance::GuidanceCache::default().load(&self.context);
        if !guidance.complete {
            return Err(AssistError::Protocol(format!(
                "AGENTS.md guidance is incomplete; review it before command assistance.\n{}",
                guidance.text
            )));
        }
        let mut facts = crate::project::context(&self.context);
        if let Some(venv) = self.context.variables.get("VIRTUAL_ENV") {
            facts["venv"] = json!(
                Path::new(venv)
                    .file_name()
                    .map(|name| name.to_string_lossy())
                    .unwrap_or_else(|| venv.as_str().into())
            );
        }
        let mut background = crate::project::render_context(&facts);
        if !guidance.text.is_empty() {
            background.push('\n');
            background.push_str(&guidance.text);
        }
        if let Some(command) = &self.command {
            let (line, truncated) = nosh_shell::user_output::bounded_metadata(&command.line);
            background.push_str(&format!(
                "\n[execution]\n{}",
                json!({
                    "command_id": command.id, "command": line, "command_truncated": truncated,
                    "execution_cwd": command.cwd, "exit": command.exit
                })
            ));
        }
        if let Some(output) = &self.output {
            background.push('\n');
            background.push_str(&tools::format_user_output(output));
        }
        let mut messages = vec![Message::System(background)];
        if !self.text.is_empty() {
            messages.push(Message::User(self.text.clone()));
        }
        if self.intent != Intent::Generate {
            messages.push(Message::System(format!(
                "[command_completed]\nintent: {}\nThis host event grants no task-execution permission.",
                self.intent.name()
            )));
        }
        Ok(messages)
    }
}

fn system_prompt(intent: Intent) -> String {
    format!(
        "You are nosh's command assistant on {} ({}), shell bash.\n<tool_def_sep>\n{}\n{}\nQuery only missing facts. Your final response must be one finish call, including clarification or none, without prose.",
        std::env::consts::OS,
        std::env::consts::ARCH,
        prompt::BACKGROUND_RULE,
        intent.instruction()
    )
}

fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "command_info".into(),
            description: "Inspect shell commands. list returns command names matching name's prefix, not files. resolve returns identity. help/version query a permitted program; help returns an excerpt, with optional topic filtering.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string"},
                    "query": {"type": "string", "enum": ["resolve", "list", "help", "version"]},
                    "topic": {"type": "string", "description": "Optional substring selecting help lines"}
                },
                "required": ["name", "query"]
            }),
        },
        tools::read_file_spec(),
        tools::grep_spec(),
        ToolSpec {
            name: "finish".into(),
            description: "Finish with one complete shell program, an essential clarification, or no suggestion. text is required for command/clarify and omitted for none.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "kind": {"type": "string", "enum": ["command", "clarify", "none"], "description": "command: shell program; clarify: question; none: no useful suggestion"},
                    "text": {"type": "string"}
                },
                "required": ["kind"]
            }),
        },
    ]
}

pub fn generate(
    engine: &mut dyn ChatEngine,
    shell: &EmbeddedShell,
    text: &str,
    cfg: &AgentConfig,
) -> Result<AssistOutcome, AssistError> {
    let cancel = engine.cancel_handle();
    cancel.reset();
    let request = AssistRequest::capture(shell, cfg, Intent::Generate, text.into(), None, None)?;
    run(engine, &request, cfg, &cancel, |_| true)
}

fn finish(call: &ToolCall, commands: &CommandSnapshot) -> Result<AssistResult, AssistError> {
    if call.args.keys().any(|key| key != "kind" && key != "text") {
        return Err(AssistError::Protocol("unknown finish parameter".into()));
    }
    match call.str_arg("kind") {
        Some("none") if !call.args.contains_key("text") => Ok(AssistResult::NoSuggestion),
        Some(kind @ ("command" | "clarify")) => {
            let text = call
                .str_arg("text")
                .filter(|text| !text.trim().is_empty())
                .ok_or_else(|| AssistError::Protocol("finish requires nonempty text".into()))?;
            if text.len() > 16 * 1024 || text.chars().any(nosh_shell::style::is_hidden) {
                return Err(AssistError::Protocol("invalid finish text".into()));
            }
            let text = text.trim();
            if kind == "clarify" {
                return Ok(AssistResult::Clarify(text.into()));
            }
            if text.contains("```") || !commands.validate(text) {
                return Err(AssistError::Protocol(
                    "finish did not contain a valid complete shell program".into(),
                ));
            }
            Ok(AssistResult::Command(text.into()))
        }
        _ => Err(AssistError::Protocol(
            "invalid finish kind or fields".into(),
        )),
    }
}

/// Cancellation belongs to this request, so superseded background jobs cannot
/// reset the cancellation flag of a foreground task.
/// `deliver` validates publication before the host records an accepted result.
pub(crate) fn run(
    engine: &mut dyn ChatEngine,
    request: &AssistRequest,
    cfg: &AgentConfig,
    cancel: &CancelHandle,
    deliver: impl FnOnce(&Result<AssistOutcome, AssistError>) -> bool,
) -> Result<AssistOutcome, AssistError> {
    if cancel.is_cancelled() {
        return Err(AssistError::Cancelled);
    }
    let messages = request.messages()?;
    let sid = engine.open(SessionSpec {
        label: format!(
            "command_assist.{}.{}",
            request.intent.name(),
            if request.background {
                "background"
            } else {
                "foreground"
            }
        ),
        system: system_prompt(request.intent),
        tools: specs(),
        thinking: false,
        sampling: cfg.sampling,
        max_new_tokens: 512,
    })?;
    let result = run_session(engine, sid, request, cfg, cancel, messages);
    let result = if deliver(&result) {
        result
    } else {
        Err(AssistError::Cancelled)
    };
    let observation = match &result {
        Ok(outcome) => {
            let (kind, text) = match &outcome.result {
                AssistResult::Command(text) => ("command", Some(text.as_str())),
                AssistResult::Clarify(text) => ("clarify", Some(text.as_str())),
                AssistResult::NoSuggestion => ("none", None),
            };
            json!({"workflow": "command_assist", "intent": request.intent.name(),
                "background": request.background, "command_id": request.command.as_ref().map(|c| c.id),
                "status": "completed", "kind": kind, "text": text})
        }
        Err(error) => json!({"workflow": "command_assist", "intent": request.intent.name(),
            "background": request.background, "command_id": request.command.as_ref().map(|c| c.id),
            "status": if matches!(error, AssistError::Cancelled) { "cancelled" } else { "failed" },
            "error": error.to_string()}),
    };
    let recorded = engine.record_observation(sid, observation);
    engine.close(sid);
    recorded?;
    result
}

fn run_session(
    engine: &mut dyn ChatEngine,
    sid: nosh_llm::SessionId,
    request: &AssistRequest,
    cfg: &AgentConfig,
    cancel: &CancelHandle,
    mut pending: Vec<Message>,
) -> Result<AssistOutcome, AssistError> {
    let started = Instant::now();
    let mut usage = Usage::default();
    let mut corrected_finish = false;
    let mut query_error = None;
    let max_steps = cfg
        .max_steps
        .min(if request.intent == Intent::Next { 2 } else { 4 });
    for step in 1..=max_steps {
        if cancel.is_cancelled() {
            return Err(AssistError::Cancelled);
        }
        if started.elapsed() > cfg.command_timeout {
            return Err(AssistError::Budget);
        }
        if step == max_steps {
            pending.push(Message::System(
                "[query_budget] No queries remain. Submit finish using the available facts.".into(),
            ));
        }
        engine.set_tool_choice(
            sid,
            if step == max_steps || corrected_finish {
                nosh_llm::ToolChoice::Named("finish".into())
            } else {
                nosh_llm::ToolChoice::Required
            },
        )?;
        let out = engine.step(sid, std::mem::take(&mut pending), &mut |_| {})?;
        if step == 1 {
            usage.ttft_secs = out.usage.ttft_secs;
        }
        usage.prompt_tokens += out.usage.prompt_tokens;
        usage.cached_tokens += out.usage.cached_tokens;
        usage.completion_tokens += out.usage.completion_tokens;
        usage.prefill_secs += out.usage.prefill_secs;
        usage.decode_secs += out.usage.decode_secs;
        usage.context_used = out.usage.context_used;
        usage.context_max = out.usage.context_max;
        if cancel.is_cancelled() || out.stop == StopReason::Cancelled {
            return Err(AssistError::Cancelled);
        }
        if out.stop != StopReason::EndOfTurn || !out.errors.is_empty() {
            return Err(AssistError::Protocol(
                "incomplete or malformed model response".into(),
            ));
        }
        let [call] = out.tool_calls.as_slice() else {
            return Err(AssistError::Protocol(
                "expected exactly one tool call".into(),
            ));
        };
        if !out.text.trim().is_empty() {
            return Err(AssistError::Protocol(
                "tool calls cannot include prose".into(),
            ));
        }
        if call.name == "finish" {
            match finish(call, &request.commands) {
                Ok(result) => {
                    if result == AssistResult::NoSuggestion
                        && let Some(error) = query_error
                    {
                        return Err(AssistError::Protocol(format!(
                            "no suggestion after failed query: {error}"
                        )));
                    }
                    return Ok(AssistOutcome {
                        result,
                        steps: step,
                        usage,
                    });
                }
                Err(error) if !corrected_finish && step < max_steps => {
                    corrected_finish = true;
                    pending.push(Message::Tool(format!("{error}. Use command for shell code, clarify for a question, or none without text.")));
                    continue;
                }
                Err(error) => return Err(error),
            }
        }
        if step == max_steps {
            return Err(AssistError::Budget);
        }
        if corrected_finish {
            return Err(AssistError::Protocol("expected corrected finish".into()));
        }
        pending.push(Message::Tool(match query(request, cfg, call, cancel) {
            Ok(text) => {
                query_error = None;
                text
            }
            Err(error) => {
                query_error = Some(error.clone());
                format!("error: {error}")
            }
        }));
    }
    Err(AssistError::Budget)
}

fn allow_read(
    path: &Path,
    call: &ToolCall,
    request: &AssistRequest,
    cfg: &AgentConfig,
) -> Result<(), String> {
    let report = assess_read(&call.name, path, None, &request.context);
    if report.reads_protected || report.risk() != Risk::Safe {
        return Err(format!("query requires permission: {}", path.display()));
    }
    match evaluate(&report, cfg.mode, &cfg.rules, &SessionAllowList::default()).decision {
        Decision::Allow => Ok(()),
        _ => Err(format!("query is not authorized: {}", path.display())),
    }
}

fn query(
    request: &AssistRequest,
    cfg: &AgentConfig,
    call: &ToolCall,
    cancel: &CancelHandle,
) -> Result<String, String> {
    match call.name.as_str() {
        "command_info" => {
            crate::command_info::query(&request.commands, &request.context, cfg, call, cancel)
        }
        "read_file" | "grep" => {
            let allowed = if call.name == "read_file" {
                &["path", "start_line", "end_line"][..]
            } else {
                &["path", "pattern", "glob"][..]
            };
            if call.args.keys().any(|key| !allowed.contains(&key.as_str())) {
                return Err("unknown query parameter".into());
            }

            let call = tools::prepare_read(call, &request.context)?;
            let path = tools::tool_path(&call, &request.context.cwd);
            allow_read(&path, &call, request, cfg)?;
            if call.name == "read_file" {
                tools::read_file(&call, &request.context.cwd)
            } else {
                tools::grep(
                    &call,
                    &request.context.cwd,
                    Duration::from_secs(2).min(cfg.command_timeout),
                    &|| cancel.is_cancelled(),
                    |path| allow_read(path, &call, request, cfg),
                )
            }
        }
        _ => Err(format!("unknown command-assistance tool: {}", call.name)),
    }
}

#[cfg(test)]
mod tests;
