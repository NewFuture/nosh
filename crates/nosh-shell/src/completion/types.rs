use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub(crate) const MAX_INPUT: usize = 256 * 1024;
pub(crate) const MAX_WORD: usize = 8 * 1024;
pub(crate) const MAX_SET: usize = 16_384;
pub(crate) const MAX_SET_BYTES: usize = 4 * 1024 * 1024;
pub(crate) const MAX_RESULTS: usize = 256;
pub(crate) const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;
pub(crate) const MAX_SNAPSHOT: usize = 4 * 1024 * 1024;
pub(crate) const MAX_FRAME: usize = 8 * 1024 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Registry {
    pub names: BTreeSet<String>,
    pub default: bool,
    pub initial: bool,
    pub empty: bool,
    pub overflow: bool,
}

impl Registry {
    pub fn capture(shell: &crate::backend::BrushShell) -> Self {
        let config = shell.completion_config();
        let mut names = BTreeSet::new();
        let mut bytes = 0;
        let mut overflow = false;
        for (name, _) in config.iter() {
            if names.len() >= MAX_SET || bytes + name.len() + 32 > 64 * 1024 {
                overflow = true;
                break;
            }
            bytes += name.len() + 32;
            names.insert(name.clone());
        }
        Self {
            names,
            default: config.default.is_some(),
            initial: config.initial_word.is_some(),
            empty: config.empty_line.is_some(),
            overflow,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct NativeSnapshot {
    pub context: crate::input_assist::Context,
    pub registry: Registry,
    pub variables: BTreeSet<String>,
    pub word_breaks: String,
    pub scripts: bool,
    pub environment: BTreeMap<String, String>,
    pub abbreviations: crate::input_assist::Abbreviations,
    pub nocase_paths: bool,
    pub variables_complete: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct Snapshot {
    pub native: Arc<NativeSnapshot>,
    pub script: Result<Arc<serde_json::value::RawValue>, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Install {
    Native(Arc<NativeSnapshot>),
    Script {
        native: Arc<NativeSnapshot>,
        state: Arc<serde_json::value::RawValue>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Progress {
        answer: Answer,
        budget: Budget,
    },
    Ready {
        answer: Answer,
        registry: Option<Registry>,
        checkpoint: Option<Box<serde_json::value::RawValue>>,
    },
    ScriptRequired(Query),
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub(crate) enum Budget {
    Lookup,
    Index,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Trigger {
    Explicit,
    Refresh,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Query {
    pub text: String,
    pub cursor: usize,
    pub session: u64,
    pub epoch: u64,
    pub trigger: Trigger,
}

impl Query {
    pub fn matches(&self, text: &str, cursor: usize) -> bool {
        self.cursor == cursor && self.text == text
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub(crate) enum Source {
    Command,
    Path,
    Git,
    Make,
    Script(String),
    Variable,
    Abbreviation { name: String, revision: u64 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    Command,
    Directory,
    File,
    Subcommand,
    Option,
    Value,
    Branch,
    Target,
    Variable,
    Abbreviation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Candidate {
    pub source: Source,
    pub value: String,
    pub kind: Kind,
    pub description: Option<String>,
    pub span: Range<usize>,
    pub filenames: bool,
    pub noquote: bool,
    pub nospace: bool,
    pub matches: Vec<usize>,
    pub display: Option<String>,
}

impl Candidate {
    pub fn identity(&self) -> String {
        format!("{:?}\0{:?}\0{}", self.source, self.kind, self.value)
    }

    pub fn bytes(&self) -> usize {
        self.value.len()
            + self.description.as_ref().map_or(0, String::len)
            + self.display.as_ref().map_or(0, String::len)
            + self.matches.len() * std::mem::size_of::<usize>()
            + 128
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum State {
    Complete,
    Partial(String),
    Failed(String),
    Unavailable(String),
}

impl State {
    pub fn is_complete(&self) -> bool {
        matches!(self, Self::Complete)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Answer {
    pub query: Query,
    pub candidates: Vec<Candidate>,
    pub state: State,
}

impl Answer {
    pub fn failed(query: Query, error: impl Into<String>) -> Self {
        Self {
            query,
            candidates: Vec::new(),
            state: State::Failed(error.into()),
        }
    }
}
