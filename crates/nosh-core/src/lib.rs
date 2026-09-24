//! The agent harness (design §5): prompts, the task loop, tools, approvals
//! and rendering. Engine-agnostic: runs against [`nosh_llm::ChatEngine`].

pub mod agent;
pub mod approval;
pub mod handler;
pub mod prompt;
pub mod suggest;
pub mod tools;
pub mod ui;

pub use agent::{Agent, AgentConfig, TaskOutcome, TaskStatus};
pub use approval::{
    ApprovalChannel, ApprovalRequest, ApprovalResponse, NoTerminal, Scripted, TerminalApproval,
};
pub use handler::{EngineLoader, LoadedEngine, ShellAi};
pub use prompt::{Attachment, Environment, TaskInput};
pub use tools::{NoRedact, Redactor, ToolSet};
pub use ui::{AgentUi, JsonUi, RecordUi, TaskSummary, TermUi};
