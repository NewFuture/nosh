//! End-to-end task flows against MockChatEngine (no model): the REPL
//! pipeline, the agent loop, permissions and the shared shell session.

pub(super) use std::sync::{Mutex, MutexGuard, Once};

pub(super) use nosh_core::command_assist::{AssistResult, generate};
pub(super) use nosh_core::user_input::NoUserInput;
pub(super) use nosh_core::{
    Agent, AgentConfig, ApprovalResponse, Environment, NoTerminal, RecordUi, Scripted, ShellAi,
    TaskInput, TaskStatus, ToolSet,
};
pub(super) use nosh_llm::mock::{bad_call, call, text};
pub(super) use nosh_llm::{CallErrorKind, Message, MockChatEngine};
pub(super) use nosh_permissions::{ApprovalMode, Risk};
pub(super) use nosh_shell::repl::{GuardChoice, LineOutcome, Pipeline, ReplUi};
pub(super) use nosh_shell::{EmbeddedShell, ReplConfig, ShellOptions, Trigger};
pub(super) use serde_json::json;

static SERIAL: Mutex<()> = Mutex::new(());
static HOME: Once = Once::new();

pub(super) fn setup() -> MutexGuard<'static, ()> {
    HOME.call_once(|| {
        let home = std::env::temp_dir().join(format!("nosh-flow-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: set once before any test reads it; tests are serialized.
        unsafe { std::env::set_var("NOSH_HOME", home) };
    });
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

pub(super) fn shell() -> EmbeddedShell {
    EmbeddedShell::new(ShellOptions::default()).unwrap()
}

pub(super) fn env() -> Environment {
    Environment {
        os: "Linux".into(),
        arch: "x86_64".into(),
        user: "test".into(),
        available: vec![],
    }
}

pub(super) fn agent(engine: MockChatEngine, cfg: AgentConfig) -> Agent {
    Agent::new(Box::new(engine), cfg, env(), ToolSet::Full)
}

pub(super) fn tool_results(received: &[Vec<Message>]) -> Vec<String> {
    received
        .iter()
        .flatten()
        .filter_map(|m| match m {
            Message::Tool(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

pub(super) fn context_field<'a>(message: &'a str, name: &str) -> Option<&'a str> {
    let context = message.split_once("[context]\n")?.1;
    let prefix = format!("{name}: ");
    context.lines().find_map(|line| line.strip_prefix(&prefix))
}

pub(super) fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nosh-flow-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::canonicalize(&d).unwrap()
}

pub(super) struct Ui;

impl ReplUi for Ui {
    fn guard(&mut self, _: &str) -> GuardChoice {
        GuardChoice::Ai
    }
    fn notice(&mut self, _: &str) {}
}
