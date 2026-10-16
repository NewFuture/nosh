//! The evaluation harness's private Unix-socket worker is not a Windows service.

use std::path::Path;

type Connected = (Box<dyn nosh_engine::ChatEngine>, String, serde_json::Value);

pub fn connect(_: &Path, _: &crate::engine::EngineSetup) -> Result<Connected, String> {
    Err("the evaluation Unix-socket worker is not supported on Windows; unset NOSH_EVAL_WORKER to use local inference".into())
}

pub fn run(_: &Path, _: &crate::engine::EngineSetup) -> Result<(), String> {
    Err("the evaluation Unix-socket worker is not supported on Windows".into())
}
