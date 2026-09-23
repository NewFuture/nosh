//! nosh shell core: an embedded brush shell shared by the user and the agent,
//! the interactive REPL, and the AI trigger pipeline (design §4).

pub mod backend;
pub mod guard;
pub mod history;
pub mod procs;
pub mod repl;
pub mod spell;
pub mod style;
pub mod term;
pub mod trigger;

pub use backend::{
    AgentExecOpts, CommandResult, EmbeddedShell, Interrupts, NullSink, OutputSink, Resolution,
    SessionState, ShellError, ShellOptions, StateDiff, UserCommand, UserRun, register_internal_env,
};
pub use repl::{AiHandler, AiOutcome, AiRequest, Badge, OnFailure, ReplConfig};
pub use trigger::{Trigger, TriggerConfig};

/// The shell operations the harness relies on (design §3.4).
pub trait ShellBackend {
    fn run_user_line(&mut self, line: &str) -> UserRun;
    fn run_agent_command(
        &mut self,
        cmd: &str,
        opts: &AgentExecOpts,
        out: &mut dyn OutputSink,
    ) -> Result<CommandResult, ShellError>;
    fn parse(&self, src: &str) -> Result<brush_parser::ast::Program, brush_parser::ParseError>;
    fn resolve(&self, name: &str) -> Resolution;
    fn snapshot(&self) -> SessionState;
}

impl ShellBackend for EmbeddedShell {
    fn run_user_line(&mut self, line: &str) -> UserRun {
        EmbeddedShell::run_user_line(self, line)
    }

    fn run_agent_command(
        &mut self,
        cmd: &str,
        opts: &AgentExecOpts,
        out: &mut dyn OutputSink,
    ) -> Result<CommandResult, ShellError> {
        EmbeddedShell::run_agent_command(self, cmd, opts, out)
    }

    fn parse(&self, src: &str) -> Result<brush_parser::ast::Program, brush_parser::ParseError> {
        EmbeddedShell::parse(self, src)
    }

    fn resolve(&self, name: &str) -> Resolution {
        EmbeddedShell::resolve(self, name)
    }

    fn snapshot(&self) -> SessionState {
        EmbeddedShell::snapshot(self)
    }
}
