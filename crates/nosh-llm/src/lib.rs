//! Local inference for nosh: a candle-based quantized llama (forked from
//! candle-transformers), the MiniCPM5 chat template, sampling, token-id driven
//! tool-call parsing and the [`ChatEngine`] implementations.

pub mod engine;
pub mod local;
pub mod mock;
pub mod model;
pub mod pyjson;
pub mod sampling;
pub mod template;
pub mod tokenizer;
pub mod toolcall;

pub use engine::{
    CallError, CallErrorKind, CancelHandle, ChatEngine, Event, Message, SamplingParams, SessionId,
    SessionSpec, StepOutcome, StopReason, ToolCall, ToolSpec, Usage,
};
pub use local::{EngineInfo, LocalChatEngine, LocalEngineOptions, rss_mb};
pub use mock::MockChatEngine;
pub use model::attn::KvDtype;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("model error: {0}")]
    Model(#[from] candle_core::Error),
    #[error("tokenizer error: {0}")]
    Tokenizer(String),
    #[error("{0}")]
    Config(String),
    #[error("context is full ({used} of {max} tokens)")]
    ContextFull { used: usize, max: usize },
    #[error("unknown session {0}")]
    UnknownSession(SessionId),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}
