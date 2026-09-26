//! Engine-facing types: messages in, structured events out. Nothing here
//! exposes template text, special tokens or tool-call syntax.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::LlmError;

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

/// Conversation messages. System text is trusted (special tokens allowed);
/// user and tool content is untrusted and encoded as plain text.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    System(String),
    User(String),
    Assistant {
        content: String,
        tool_calls: Vec<ToolCall>,
    },
    Tool(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallErrorKind {
    Malformed,
    UnknownTool,
    MissingParam,
    BadType,
    Truncated,
}

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    EndOfTurn,
    MaxTokens,
    Cancelled,
}

#[derive(Debug, Clone, Default, PartialEq)]
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

#[derive(Debug, Clone, PartialEq)]
pub struct StepOutcome {
    pub text: String,
    pub think: String,
    pub tool_calls: Vec<ToolCall>,
    pub errors: Vec<CallError>,
    pub stop: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, Copy, PartialEq)]
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

#[derive(Debug, Clone)]
pub struct SessionSpec {
    /// Static system prompt; `<tool_def_sep>` marks where tool definitions go.
    pub system: String,
    pub tools: Vec<ToolSpec>,
    pub thinking: bool,
    pub sampling: SamplingParams,
    pub max_new_tokens: usize,
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

/// Messages in, events out (design §3.4). Implementations: in-process
/// [`crate::LocalChatEngine`] and the scripted [`crate::MockChatEngine`].
pub trait ChatEngine {
    fn open(&mut self, spec: SessionSpec) -> Result<SessionId, LlmError>;

    /// Appends `append` to the conversation and generates one assistant turn.
    fn step(
        &mut self,
        sid: SessionId,
        append: Vec<Message>,
        sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, LlmError>;

    /// Keeps only the first `keep` non-system messages.
    fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), LlmError>;

    /// Number of non-system messages (including generated assistant turns).
    fn message_count(&self, sid: SessionId) -> usize;

    /// Replaces the content of tool results older than the last `keep_recent`
    /// messages with a one-line note; returns the number of shortened results.
    /// A grouped tool turn overlapping recent messages may be kept intact.
    /// Tokenization failures leave the conversation unchanged.
    fn compact_tool_results(
        &mut self,
        sid: SessionId,
        keep_recent: usize,
    ) -> Result<usize, LlmError>;

    /// `(used, max)` context tokens.
    fn context_usage(&self, sid: SessionId) -> (usize, usize);

    fn cancel_handle(&self) -> CancelHandle;

    fn cancel(&self, _sid: SessionId) {
        self.cancel_handle().cancel();
    }

    fn close(&mut self, sid: SessionId);
}
