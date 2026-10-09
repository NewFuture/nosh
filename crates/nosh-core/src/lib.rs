//! The agent harness (design §5): prompts, the task loop, tools, approvals
//! and rendering. Engine-agnostic: runs against [`nosh_engine::ChatEngine`].

pub mod agent;
pub mod approval;
mod assist_worker;
pub mod command_assist;
mod command_help;
mod guidance;
pub mod handler;
mod project;
pub mod prompt;
pub mod tools;
pub mod ui;
pub mod user_input;

pub use agent::{Agent, AgentConfig, TaskOutcome, TaskStatus};
pub use approval::{
    ApprovalChannel, ApprovalRequest, ApprovalResponse, NoTerminal, Scripted, TerminalApproval,
};
pub use handler::{EngineLoader, LoadMode, LoadedEngine, ShellAi};
pub use prompt::{Attachment, Environment, TaskInput};
pub use tools::{NoRedact, Redactor, ToolSet};
pub use ui::{AgentUi, JsonUi, RecordUi, TaskSummary, TermUi};
