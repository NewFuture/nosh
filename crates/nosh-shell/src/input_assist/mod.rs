//! Owned input diagnostics and explicit local draft correction. Parser and
//! filesystem work stays outside the editor thread and never executes a draft.

mod allocation;
mod analysis;
mod correction;
mod editor;
mod lookup;
#[cfg(test)]
mod tests;
mod worker;

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};

pub use allocation::WorkerAllocator;
pub(crate) use correction::{Correction, right_navigation};
pub(crate) use editor::Feedback;
pub use editor::InputAssist;
pub(crate) use lookup::scan_index;

#[cfg(test)]
#[global_allocator]
static TEST_WORKER_ALLOCATOR: WorkerAllocator = WorkerAllocator;
pub use worker::run_worker_from_env;

pub(crate) const MAX_INPUT: usize = 256 * 1024;
pub(crate) const MAX_WORD: usize = 8 * 1024;
pub(crate) const MAX_SPANS: usize = 4096;
pub(crate) const MAX_NODES: usize = 16_384;
pub(crate) const MAX_DEPTH: usize = 64;
pub(crate) const MAX_QUERIES: usize = 64;
pub(crate) const MAX_CONTEXT: usize = 256 * 1024;
pub(crate) const MAX_NAMES: usize = 16_384;
pub(crate) const MAX_INDEX_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_FRAME: usize = 8 * 1024 * 1024;
pub(crate) const SYNTAX_TIMEOUT: Duration = Duration::from_millis(250);
pub(crate) const LOOKUP_TIMEOUT: Duration = Duration::from_millis(500);
pub(crate) const INDEX_TIMEOUT: Duration = Duration::from_millis(1500);

/// An explicit launcher also lets embedded REPLs use their own worker entry point.
#[derive(Debug, Clone)]
pub struct WorkerCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl WorkerCommand {
    pub fn nosh() -> std::io::Result<Self> {
        Ok(Self {
            program: std::env::current_exe()?,
            args: vec!["--__nosh_input_worker".into()],
        })
    }
}

#[derive(Debug, Clone)]
pub struct Config {
    pub enabled: bool,
    pub worker: Option<WorkerCommand>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            worker: None,
        }
    }
}

/// Only rules already confirmed enabled and applicable by their owner belong here.
/// Recognition does not expand a rule or treat it as an external executable.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Abbreviations {
    pub revision: u64,
    pub applicable: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Context {
    pub cwd: PathBuf,
    pub path: Option<String>,
    pub home: Option<String>,
    pub oldpwd: Option<String>,
    pub cdpath: bool,
    pub hashed_commands: BTreeMap<String, PathBuf>,
    pub check_hash: bool,
    pub command_traps: bool,
    pub builtins: BTreeSet<String>,
    pub aliases: BTreeSet<String>,
    pub functions: BTreeSet<String>,
    pub abbreviations: Abbreviations,
    pub extglob: bool,
    pub posix: bool,
    pub sh: bool,
    pub ai_enabled: bool,
    pub ai_prefix: String,
    pub ai_builtin: String,
    pub ai_builtin_shadowed: bool,
    pub trigger_on_error: bool,
}

impl Context {
    pub(crate) fn options(&self) -> brush_parser::ParserOptions {
        brush_parser::ParserOptions {
            enable_extended_globbing: self.extglob,
            posix_mode: self.posix,
            sh_mode: self.sh,
            ..brush_parser::ParserOptions::default()
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Version {
    pub input: u64,
    pub session: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Input {
    pub version: Version,
    pub text: String,
    pub context: Arc<Context>,
    pub command_input: bool,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Role {
    #[default]
    Text,
    Command,
    Builtin,
    Alias,
    Function,
    External,
    Abbreviation,
    Keyword,
    String,
    Variable,
    Operator,
    Comment,
    Ai,
    Path,
    Incomplete,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Span {
    pub range: Range<usize>,
    pub role: Role,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum State {
    Known,
    Error,
    Incomplete,
    Unknown,
    Pending,
    Unavailable,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Reason {
    Syntax(String),
    Incomplete(String),
    Dynamic,
    Snapshot,
    MissingCommand,
    MissingPath,
    NotExecutable,
    NotDirectory,
    DirectoryOutput,
    NewTarget,
    Io(String),
    AccessDenied(String),
    Limit,
    Pending,
    Worker(String),
    Ai,
    Abbreviation,
}

impl Reason {
    fn bounded(&self) -> bool {
        match self {
            Self::Syntax(text)
            | Self::Incomplete(text)
            | Self::Io(text)
            | Self::AccessDenied(text)
            | Self::Worker(text) => text.len() <= 1024,
            _ => true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Finding {
    pub range: Range<usize>,
    pub state: State,
    pub reason: Reason,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum PathUse {
    Argument,
    Explicit,
    Read,
    Write,
    Directory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum QueryKind {
    Command {
        path: Option<String>,
        ai_on_missing: bool,
    },
    Path(PathUse),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Query {
    pub range: Range<usize>,
    pub word: String,
    pub kind: QueryKind,
    /// Prior statements may change the filesystem before this lookup occurs.
    pub definite: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Analysis {
    pub version: Version,
    pub spans: Vec<Span>,
    pub findings: Vec<Finding>,
    pub queries: Vec<Query>,
    pub ai_candidate: bool,
}

impl Analysis {
    fn new(version: Version) -> Self {
        Self {
            version,
            spans: Vec::new(),
            findings: Vec::new(),
            queries: Vec::new(),
            ai_candidate: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Observation {
    pub range: Range<usize>,
    pub role: Option<Role>,
    pub finding: Option<Finding>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) struct LookupStats {
    pub metadata_calls: u64,
    pub cache_entries: usize,
    pub cache_bytes: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Index {
    pub cwd: PathBuf,
    pub path: Option<String>,
    pub names: Arc<Vec<String>>,
    pub complete: bool,
    pub reason: Option<String>,
}

pub(crate) type SharedIndex = Arc<std::sync::Mutex<Option<Index>>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Request {
    Analyze(Arc<Input>),
    Lookup {
        input: Arc<Input>,
        queries: Vec<Query>,
    },
    Index {
        session: u64,
        context: Arc<Context>,
    },
    Correction {
        input: Arc<Input>,
        proposal: Correction,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Response {
    Analysis(Analysis),
    Lookup {
        version: Version,
        observations: Vec<Observation>,
        stats: LookupStats,
    },
    Index(Index),
    Correction {
        version: Version,
        accepted: bool,
    },
    Failed(String),
}

fn short_error(error: impl std::fmt::Display) -> String {
    error.to_string().chars().take(240).collect()
}

pub(crate) fn bounded_json(value: &impl Serialize, limit: usize) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    write_json(value, &mut bytes, limit)?;
    Ok(bytes)
}

pub(crate) fn write_json(
    value: &impl Serialize,
    writer: &mut impl std::io::Write,
    limit: usize,
) -> std::io::Result<usize> {
    struct Limited<'a, W> {
        writer: &'a mut W,
        remaining: usize,
    }
    impl<W: std::io::Write> std::io::Write for Limited<'_, W> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.remaining {
                return Err(std::io::Error::other("input analysis data limit"));
            }
            let written = self.writer.write(bytes)?;
            self.remaining -= written;
            Ok(written)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            self.writer.flush()
        }
    }
    let mut limited = Limited {
        writer,
        remaining: limit,
    };
    serde_json::to_writer(&mut limited, value).map_err(std::io::Error::other)?;
    Ok(limit - limited.remaining)
}

fn spans(text: &str, roles: &[Role]) -> Vec<Span> {
    let mut spans: Vec<Span> = Vec::new();
    for (start, c) in text.char_indices() {
        let end = start + c.len_utf8();
        let role = roles[start];
        if let Some(last) = spans.last_mut()
            && last.role == role
        {
            last.range.end = end;
        } else {
            spans.push(Span {
                range: start..end,
                role,
            });
        }
    }
    spans
}
