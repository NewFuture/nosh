//! nosh shell core: an embedded brush shell shared by the user and the agent,
//! the interactive REPL, and the AI trigger pipeline (design §4).

mod assist_display;
pub mod backend;
mod command_context;
mod command_snapshot;
pub mod completion;
pub mod editing;
pub mod guard;
pub mod history;
pub mod input_assist;
pub mod procs;
pub mod pty;
pub mod repl;
pub mod spell;
pub mod status;
pub mod style;
mod suggestion;
pub mod term;
pub mod trigger;
pub mod user_output;
mod yielding;

pub use assist_display::{AssistDisplay, Assistance};
pub use backend::{
    AgentExecOpts, CommandResult, EmbeddedShell, Interrupts, NullSink, OutputSink, Resolution,
    SessionState, ShellError, ShellOptions, StateDiff, UserCommand, UserRun, register_internal_env,
};
pub use command_snapshot::CommandSnapshot;
pub use repl::{AiHandler, AiOutcome, AiRequest, Badge, OnFailure, ReplConfig};
pub use trigger::{Trigger, TriggerConfig};
pub use user_output::{CaptureOutput, OutputState, OutputUnavailable, UserOutput};
