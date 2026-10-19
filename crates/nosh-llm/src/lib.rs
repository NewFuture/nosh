//! Local Candle inference: quantized Llama/MiniCPM5 and text-only Qwen3.5,
//! model-specific templates and token-driven tools, sampling, and the local
//! [`nosh_engine::ChatEngine`] implementation.

mod conversation;
pub mod cpu;
pub mod device;
pub mod local;
pub mod model;
mod protocol;
pub mod pyjson;
pub mod sampling;
pub mod template;
pub mod tokenizer;
pub mod toolcall;

pub use device::{DeviceSelection, InferenceDevice};
pub use local::{EngineInfo, LocalChatEngine, LocalEngineOptions, ModelSource};
pub use model::attn::KvDtype;

#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("model error: {0}")]
    Model(#[from] candle_core::Error),
    #[error("tokenizer error: {0}")]
    Tokenizer(String),
    #[error("{0}")]
    Config(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl From<LlmError> for nosh_engine::EngineError {
    fn from(error: LlmError) -> Self {
        match error {
            LlmError::Config(message) => Self::Config(message),
            LlmError::Io(error) => Self::Io(error),
            error => Self::Backend(Box::new(error)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nosh_engine::EngineError;

    #[test]
    fn engine_errors_preserve_local_diagnostics_and_error_types() {
        for error in [
            LlmError::Tokenizer("encoding failed".into()),
            LlmError::Model(candle_core::Error::Msg("tensor failed".into())),
        ] {
            let message = error.to_string();
            let converted = EngineError::from(error);
            assert_eq!(converted.to_string(), message);
            let EngineError::Backend(source) = converted else {
                panic!("local failure must remain an opaque backend error");
            };
            assert!(source.downcast_ref::<LlmError>().is_some());
        }
        assert!(matches!(
            EngineError::from(LlmError::Config("invalid choice".into())),
            EngineError::Config(message) if message == "invalid choice"
        ));
        assert!(matches!(
            EngineError::from(LlmError::Io(std::io::Error::from(
                std::io::ErrorKind::PermissionDenied
            ))),
            EngineError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied
        ));
    }
}
