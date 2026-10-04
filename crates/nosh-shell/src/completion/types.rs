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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub native: Arc<NativeSnapshot>,
    pub script: Option<Result<Arc<serde_json::value::RawValue>, String>>,
}

impl Snapshot {
    pub fn validate(&self) -> std::io::Result<()> {
        if !self.native.context.cwd.is_absolute()
            || self.script.as_ref().is_some_and(|state| {
                state
                    .as_ref()
                    .is_ok_and(|state| state.get().len() > MAX_SNAPSHOT)
            })
        {
            return Err(std::io::Error::other(
                "invalid or oversized completion snapshot",
            ));
        }
        crate::input_assist::write_json(
            &self.native,
            &mut std::io::sink(),
            crate::input_assist::MAX_CONTEXT,
        )?;
        Ok(())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) enum Outcome {
    Progress {
        answer: Answer,
        budget: Budget,
    },
    Ready {
        answer: Answer,
        snapshot: Option<Snapshot>,
    },
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
    Npm,
    Yarn,
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
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.candidates.len() > MAX_RESULTS
            || self.candidates.iter().map(Candidate::bytes).sum::<usize>() > MAX_SET_BYTES
            || self.candidates.iter().any(|candidate| {
                candidate.value.len() > MAX_WORD
                    || candidate.span.start > candidate.span.end
                    || self.query.text.get(candidate.span.clone()).is_none()
            })
        {
            Err("invalid or oversized completion reply")
        } else {
            Ok(())
        }
    }

    pub fn unavailable(query: Query, message: impl Into<String>) -> Self {
        Self {
            query,
            candidates: Vec::new(),
            state: State::Unavailable(message.into()),
        }
    }

    pub fn failed(query: Query, error: impl Into<String>) -> Self {
        Self {
            query,
            candidates: Vec::new(),
            state: State::Failed(error.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reply() -> Answer {
        Answer {
            query: Query {
                text: "a中b".into(),
                cursor: 5,
                session: 1,
                epoch: 1,
                trigger: Trigger::Explicit,
            },
            candidates: vec![Candidate {
                source: Source::Path,
                value: "value".into(),
                kind: Kind::File,
                description: None,
                span: 1..4,
                filenames: true,
                noquote: false,
                nospace: false,
                matches: Vec::new(),
                display: None,
            }],
            state: State::Complete,
        }
    }

    #[test]
    fn reply_limits_accept_exact_boundaries() {
        let mut answer = reply();
        answer.candidates[0].value = "v".repeat(MAX_WORD);
        answer.candidates = vec![answer.candidates[0].clone(); MAX_RESULTS];
        assert!(answer.validate().is_ok());
        answer.candidates.push(answer.candidates[0].clone());
        assert!(answer.validate().is_err());
        answer.candidates.truncate(1);
        answer.candidates[0].value.push('v');
        assert!(answer.validate().is_err());
    }

    #[test]
    fn reply_display_bytes_are_counted_once() {
        let mut answer = reply();
        let candidate = &mut answer.candidates[0];
        candidate.display = Some("d".repeat(MAX_SET_BYTES - candidate.bytes()));
        assert_eq!(candidate.bytes(), MAX_SET_BYTES);
        assert!(answer.validate().is_ok());
        answer.candidates[0].display.as_mut().unwrap().push('d');
        assert!(answer.validate().is_err());
    }

    #[test]
    fn reply_spans_must_be_ordered_in_bounds_utf8_ranges() {
        let mut answer = reply();
        assert!(answer.validate().is_ok());
        for (start, end) in [(4, 1), (1, 2), (2, 4), (0, 6)] {
            answer.candidates[0].span = start..end;
            assert!(answer.validate().is_err(), "{start}..{end}");
        }
        answer.candidates[0].span = 5..5;
        assert!(answer.validate().is_ok());
    }
}
