//! Engine-facing types: messages in, structured events out. Nothing here
//! exposes template text, special tokens or tool-call syntax.

pub mod mock;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use mock::MockChatEngine;

/// Recoverable protocol errors and opaque, source-preserving backend failures.
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("{0}")]
    Config(String),
    #[error("context is full ({used} of {max} tokens)")]
    ContextFull { used: usize, max: usize },
    #[error("unknown session {0}")]
    UnknownSession(SessionId),
    #[error(transparent)]
    Backend(Box<dyn std::error::Error + Send + Sync>),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

/// A tool the model may call. `parameters` is a JSON Schema object.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl ToolSpec {
    pub fn param_type(&self, name: &str) -> Option<&str> {
        self.parameters
            .get("properties")?
            .get(name)?
            .get("type")?
            .as_str()
    }

    pub fn required(&self) -> Vec<&str> {
        self.parameters
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub name: String,
    pub args: Map<String, Value>,
}

impl ToolCall {
    pub fn str_arg(&self, key: &str) -> Option<&str> {
        self.args.get(key).and_then(Value::as_str)
    }

    pub fn int_arg(&self, key: &str) -> Option<i64> {
        self.args.get(key).and_then(Value::as_i64)
    }
}

/// Conversation messages. Appended system context, user input and tool content
/// are encoded as plain text; the initial [`SessionSpec::system`] is trusted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool(String),
    /// A standalone tool reply supplied by the user; never compacted as tool output.
    UserAnswer(String),
}

/// One-line stand-in for an old tool result (keeps the status header).
pub fn shorten_tool_result(content: &str) -> String {
    const KEEP: usize = 200;
    let count = content.chars().count();
    if count <= KEEP + 40 {
        return content.to_string();
    }
    let end = content.char_indices().nth(KEEP).expect("long result").0;
    format!(
        "{}\n[\u{2026} older output omitted to save context ({count} chars)]",
        &content[..end]
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallErrorKind {
    Malformed,
    UnknownTool,
    MissingParam,
    BadType,
    Truncated,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CallError {
    pub kind: CallErrorKind,
    pub message: String,
    /// Tool name if it could be parsed.
    pub tool: Option<String>,
}

impl std::fmt::Display for CallError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Event {
    /// Visible answer text (streamed).
    Text(String),
    /// Reasoning text inside `<think>` (streamed).
    Think(String),
    ToolCall(ToolCall),
    CallError(CallError),
    /// Prompt processing progress in tokens.
    Prefill {
        done: usize,
        total: usize,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StopReason {
    EndOfTurn,
    MaxTokens,
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Usage {
    /// Tokens that had to be prefilled this step.
    pub prompt_tokens: usize,
    /// Tokens reused from the KV cache.
    pub cached_tokens: usize,
    pub completion_tokens: usize,
    pub prefill_secs: f64,
    pub decode_secs: f64,
    /// Time from `step` start to the first sampled token.
    pub ttft_secs: f64,
    pub context_used: usize,
    pub context_max: usize,
}

impl Usage {
    pub fn prefill_tps(&self) -> f64 {
        self.prompt_tokens as f64 / self.prefill_secs.max(1e-9)
    }

    pub fn decode_tps(&self) -> f64 {
        self.completion_tokens as f64 / self.decode_secs.max(1e-9)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepOutcome {
    pub text: String,
    pub think: String,
    pub tool_calls: Vec<ToolCall>,
    pub errors: Vec<CallError>,
    pub stop: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct SamplingParams {
    pub temperature: f32,
    pub top_p: f32,
    pub min_p: f32,
    /// Applied only once repetition is detected (design §7.3).
    pub repetition_penalty: f32,
    /// Temperature inside `<function … </function>`.
    pub tool_call_temperature: f32,
    pub seed: Option<u64>,
}

impl Default for SamplingParams {
    fn default() -> Self {
        Self {
            temperature: 1.0,
            top_p: 0.95,
            min_p: 0.0,
            repetition_penalty: 1.05,
            tool_call_temperature: 0.3,
            seed: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSpec {
    /// Host-side provenance for observations; never rendered into model tokens.
    pub label: String,
    /// Trusted static system prompt; `<tool_def_sep>` marks where tool definitions go.
    pub system: String,
    pub tools: Vec<ToolSpec>,
    pub thinking: bool,
    pub sampling: SamplingParams,
    pub max_new_tokens: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "name", rename_all = "snake_case")]
pub enum ToolChoice {
    #[default]
    Auto,
    /// Disable tool-call decoding for this step without changing the conversation.
    None,
    Required,
    Named(String),
}

pub type SessionId = u64;

/// Cheap, thread-safe cancellation flag (e.g. set from a SIGINT handler).
#[derive(Debug, Clone, Default)]
pub struct CancelHandle(Arc<AtomicBool>);

impl CancelHandle {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }

    pub fn reset(&self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// Messages in, events out (design §3.4). Implemented by inference adapters
/// and the scripted [`MockChatEngine`], without requiring a model runtime.
pub trait ChatEngine: Send {
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, EngineError>;

    /// One-step decoding policy, reset to Auto after the next step.
    /// None disables tool calls; Required/Named return one call, not prose.
    fn set_tool_choice(&mut self, _sid: SessionId, _choice: ToolChoice) -> Result<(), EngineError> {
        Err(EngineError::Config(
            "engine does not support tool choice".into(),
        ))
    }

    /// Appends `append` to the conversation and generates one assistant turn.
    /// System messages append plain-text context; they do not replace the initial system prompt.
    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, EngineError>;

    /// Keeps the first `keep` appended/generated messages, including system context.
    /// The initial system prefix is never removed.
    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), EngineError>;

    /// Number of appended/generated messages, excluding only the initial system prefix.
    fn message_count(&self, sid: SessionId) -> usize;

    /// Replaces the content of tool results older than the last `keep_recent`
    /// messages with a one-line note; returns the number of shortened results.
    /// A grouped tool turn overlapping recent messages may be kept intact.
    /// Tokenization failures leave the conversation unchanged.
    fn compact_tool_results(
        &mut self,
        sid: SessionId,
        keep_recent: usize,
    ) -> Result<usize, EngineError>;

    /// `(used, max)` context tokens.
    fn context_usage(&self, sid: SessionId) -> (usize, usize);

    fn cancel_handle(&self) -> CancelHandle;

    fn cancel(&self, _sid: SessionId) {
        self.cancel_handle().cancel();
    }

    /// Optional host observations, separate from model input and output.
    fn record_observation(&mut self, _sid: SessionId, _value: Value) -> Result<(), EngineError> {
        Ok(())
    }

    fn close(&mut self, sid: SessionId);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortening_preserves_unicode_and_the_existing_marker() {
        let short = "\u{4e2d}".repeat(240);
        assert_eq!(shorten_tool_result(&short), short);
        let long = "\u{4e2d}".repeat(241);
        assert_eq!(
            shorten_tool_result(&long),
            format!(
                "{}\n[\u{2026} older output omitted to save context (241 chars)]",
                "\u{4e2d}".repeat(200)
            )
        );
    }

    #[test]
    fn cancellation_is_shared_and_resettable() {
        let cancel = CancelHandle::default();
        let worker = cancel.clone();
        std::thread::spawn(move || worker.cancel()).join().unwrap();
        assert!(cancel.is_cancelled());
        let observer = cancel.clone();
        cancel.reset();
        assert!(!observer.is_cancelled());
    }

    #[test]
    fn protocol_serialization_preserves_existing_shapes() {
        let message = Message::UserAnswer("keep backups".into());
        let value = serde_json::to_value(&message).unwrap();
        assert_eq!(value, serde_json::json!({"UserAnswer": "keep backups"}));
        assert_eq!(serde_json::from_value::<Message>(value).unwrap(), message);
        for (choice, expected) in [
            (ToolChoice::Auto, serde_json::json!({"type": "auto"})),
            (ToolChoice::None, serde_json::json!({"type": "none"})),
            (
                ToolChoice::Required,
                serde_json::json!({"type": "required"}),
            ),
            (
                ToolChoice::Named("exec".into()),
                serde_json::json!({"type": "named", "name": "exec"}),
            ),
        ] {
            assert_eq!(serde_json::to_value(&choice).unwrap(), expected);
            assert_eq!(
                serde_json::from_value::<ToolChoice>(expected).unwrap(),
                choice
            );
        }
    }
}
