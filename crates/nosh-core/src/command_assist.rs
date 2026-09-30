//! Short, query-only command assistance. Task execution remains in Agent.

use std::path::Path;
use std::time::{Duration, Instant};

use nosh_llm::{
    CancelHandle, ChatEngine, LlmError, Message, SessionSpec, StopReason, ToolCall, ToolSpec, Usage,
};
use nosh_permissions::{Context, Decision, Risk, SessionAllowList, assess_read, evaluate};
use nosh_shell::{CommandSnapshot, EmbeddedShell, UserCommand, UserOutput};
use serde_json::json;

use crate::user_input::{self, InputError, UserInput};
use crate::{AgentConfig, prompt, tools};

const FINAL_RESPONSE_RULE: &str = "Return only the complete shell command as plain text, without explanation or Markdown. If no justified command can be suggested, return exactly [None].";

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
                "Generate or revise the requested shell program, preserving named inputs and output format. Do not invent essential user choices."
            }
            Self::Fix => {
                "Use the recorded failure to propose a corrected command preserving the intended operation. Account for possible partial execution before recommending a retry."
            }
            Self::Next => {
                "Suggest one useful next command supported by the known goal and completed command. Do not invent a next step when there is no justified one."
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssistResult {
    Command(String),
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
                    let (line, command_truncated) =
                        nosh_shell::user_output::bounded_metadata(&command.line);
                    let cwd = command.cwd.to_string_lossy();
                    let (cwd, truncated) = nosh_shell::user_output::bounded_metadata(&cwd);
                    command.id != output.command_id
                        || line != output.command
                        || command_truncated != output.command_truncated
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
            let execution_cwd = if command.cwd == self.context.cwd {
                Path::new(".")
            } else {
                command.cwd.as_path()
            };
            background.push_str(&format!(
                "\n[execution]\n{}",
                json!({
                    "command_id": command.id, "command": line, "command_truncated": truncated,
                    "execution_cwd": execution_cwd, "exit": command.exit,
                    "status": if command.exit == 0 { "succeeded" } else { "failed" }
                })
            ));
        }
        if let Some(output) = &self.output {
            background.push('\n');
            background.push_str(&tools::format_assist_output(output));
        }
        let mut messages = vec![Message::System(background)];
        if !self.text.is_empty() {
            messages.push(Message::User(self.text.clone()));
        }
        if self.intent != Intent::Generate {
            messages.push(Message::System(format!(
                "[command_completed]\nintent: {}\n\
Draft a command suggestion from the recorded result, current project state and applicable project guidance. \
The suggestion is for the user to review; it will not be executed automatically.",
                self.intent.name()
            )));
        }
        Ok(messages)
    }

    fn final_message(&self, answers: &[user_input::UserAnswer]) -> Message {
        let mut text = self.intent.instruction().to_owned();
        if !self.text.is_empty() {
            text.push_str(&format!("\nUser request (quoted):\n{}", json!(self.text)));
        }
        if let Some(command) = &self.command {
            text.push_str(&format!(
                "\nRecorded command (quoted):\n{}",
                json!(command.line)
            ));
        }
        for answer in answers {
            text.push_str(&format!(
                "\nClarification (quoted, in conversation order):\n{}",
                json!({
                    "question": answer.question.question,
                    "choices": answer.question.choices,
                    "answer": answer.answer,
                })
            ));
        }
        text.push_str(&format!(
            "\nAnswer now without calling tools. {FINAL_RESPONSE_RULE}"
        ));
        Message::System(text)
    }
}

fn system_prompt(intent: Intent, can_ask: bool) -> String {
    let interaction = if can_ask {
        ""
    } else {
        "\nUser interaction is unavailable. Do not ask questions; return [None] if essential user input is missing."
    };
    format!(
        "You are nosh's command assistant on {} ({}), shell nosh (bash-compatible).\n<tool_def_sep>\n{}\n{}{interaction}\nUse tools when more information is needed.\n{FINAL_RESPONSE_RULE}",
        std::env::consts::OS,
        std::env::consts::ARCH,
        prompt::BACKGROUND_RULE,
        intent.instruction()
    )
}

fn specs(can_ask: bool) -> Vec<ToolSpec> {
    let mut tools = vec![
        crate::command_help::spec(),
        tools::read_file_spec(),
        tools::grep_spec(),
    ];
    if can_ask {
        tools.push(user_input::spec());
    }
    tools
}

pub fn generate(
    engine: &mut dyn ChatEngine,
    shell: &EmbeddedShell,
    text: &str,
    cfg: &AgentConfig,
    input: &mut dyn UserInput,
) -> Result<AssistOutcome, AssistError> {
    let cancel = engine.cancel_handle();
    cancel.reset();
    let request = AssistRequest::capture(shell, cfg, Intent::Generate, text.into(), None, None)?;
    run(engine, &request, cfg, &cancel, input, |_| true)
}

fn direct_final(text: &str, commands: &CommandSnapshot) -> Result<AssistResult, AssistError> {
    let final_text = text.trim();
    if text.len() > 16 * 1024
        || final_text.is_empty()
        || final_text.chars().any(nosh_shell::style::is_hidden)
    {
        return Err(AssistError::Protocol(
            "expected a command or exact [None], not an empty or invalid final response".into(),
        ));
    }
    if final_text == "[None]" {
        return Ok(AssistResult::NoSuggestion);
    }
    if final_text.contains("```") || !commands.validate(final_text) {
        return Err(AssistError::Protocol(
            "final response did not contain a valid complete shell program or exact [None]".into(),
        ));
    }
    Ok(AssistResult::Command(final_text.into()))
}

/// Cancellation belongs to this request, so superseded background jobs cannot
/// reset the cancellation flag of a foreground task.
/// `deliver` validates publication before the host records an accepted result.
pub(crate) fn run(
    engine: &mut dyn ChatEngine,
    request: &AssistRequest,
    cfg: &AgentConfig,
    cancel: &CancelHandle,
    input: &mut dyn UserInput,
    deliver: impl FnOnce(&Result<AssistOutcome, AssistError>) -> bool,
) -> Result<AssistOutcome, AssistError> {
    if cancel.is_cancelled() {
        return Err(AssistError::Cancelled);
    }
    let messages = request.messages()?;
    let can_ask = request.intent == Intent::Generate && !request.background && input.available();
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
        system: system_prompt(request.intent, can_ask),
        tools: specs(can_ask),
        thinking: false,
        sampling: cfg.sampling,
        max_new_tokens: 512,
    })?;
    let result = run_session(
        engine,
        sid,
        request,
        cfg,
        cancel,
        messages,
        can_ask.then_some(input),
    );
    let result = if deliver(&result) {
        result
    } else {
        Err(AssistError::Cancelled)
    };
    let mut observation = match &result {
        Ok(outcome) => {
            let (kind, text) = match &outcome.result {
                AssistResult::Command(text) => ("command", Some(text.as_str())),
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
    observation["response_format"] = json!("command_or_none");
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
    mut input: Option<&mut dyn UserInput>,
) -> Result<AssistOutcome, AssistError> {
    let mut started = Instant::now();
    let mut usage = Usage::default();
    let mut query_error = None;
    let mut answers = Vec::new();
    let mut final_response = false;
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
        final_response |= step == max_steps;
        if final_response {
            pending.push(request.final_message(&answers));
        }
        engine.set_tool_choice(
            sid,
            if final_response {
                nosh_llm::ToolChoice::None
            } else {
                nosh_llm::ToolChoice::Auto
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
        if started.elapsed() > cfg.command_timeout {
            return Err(AssistError::Budget);
        }
        if out.stop != StopReason::EndOfTurn || !out.errors.is_empty() {
            return Err(AssistError::Protocol(
                "incomplete or malformed model response".into(),
            ));
        }
        if out.tool_calls.is_empty() {
            let result = match direct_final(&out.text, &request.commands) {
                Ok(result) => result,
                Err(error) if !final_response => {
                    pending.push(Message::System(format!(
                        "Previous response rejected: {error}"
                    )));
                    final_response = true;
                    continue;
                }
                Err(error) => return Err(error),
            };
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
        if step == max_steps {
            return Err(AssistError::Budget);
        }
        if final_response {
            return Err(AssistError::Protocol(
                "tool calls are not allowed in the final response".into(),
            ));
        }
        if out.tool_calls.len() != 1 && out.tool_calls.iter().any(|call| call.name == "ask_user") {
            return Err(AssistError::Protocol(
                "ask_user must be the only tool call in its turn".into(),
            ));
        }
        for call in &out.tool_calls {
            if cancel.is_cancelled() {
                return Err(AssistError::Cancelled);
            }
            if started.elapsed() > cfg.command_timeout {
                return Err(AssistError::Budget);
            }
            let result = if call.name == "ask_user" {
                if let Some(input) = input.as_deref_mut() {
                    let waiting = Instant::now();
                    let answer = user_input::ask(input, call, cancel);
                    started += waiting.elapsed();
                    match answer {
                        Ok(answer) => {
                            let message = Message::UserAnswer(answer.answer.clone());
                            answers.push(answer);
                            Ok(message)
                        }
                        Err(InputError::Cancelled) => return Err(AssistError::Cancelled),
                        Err(error @ InputError::Unavailable(_)) => {
                            return Err(AssistError::Protocol(error.to_string()));
                        }
                        Err(error) => Err(error.to_string()),
                    }
                } else {
                    Err("ask_user is unavailable in this session".into())
                }
            } else {
                query(request, cfg, call, cancel).map(Message::Tool)
            };
            pending.push(match result {
                Ok(message) => {
                    query_error = None;
                    message
                }
                Err(error) => {
                    query_error = Some(error.clone());
                    Message::Tool(format!("error: {error}"))
                }
            });
        }
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
        "command_help" => {
            crate::command_help::query(&request.commands, &request.context, cfg, call, cancel)
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
