//! Source-aware completion with bounded, isolated native and programmable queries.

mod cache;
mod context;
mod editor;
mod matching;
mod native;
mod providers;
mod service;
mod snapshot;
#[cfg(test)]
mod tests;
pub(crate) mod types;
pub(crate) mod worker;

pub use crate::input_assist::WorkerCommand;
pub use editor::{AbbreviationSelection, Completion, Selection, SelectionObserver};

#[derive(Clone)]
pub struct Config {
    pub enabled: bool,
    pub scripts: bool,
    pub worker: Option<WorkerCommand>,
    pub abbreviations: crate::input_assist::Abbreviations,
    pub selection_observer: Option<SelectionObserver>,
}

impl std::fmt::Debug for Config {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Config")
            .field("enabled", &self.enabled)
            .field("scripts", &self.scripts)
            .field("worker", &self.worker)
            .field("abbreviations", &self.abbreviations)
            .field("selection_observer", &self.selection_observer.is_some())
            .finish()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            scripts: true,
            worker: None,
            abbreviations: Default::default(),
            selection_observer: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum Phase {
    #[default]
    Complete,
    Pending,
    Partial,
    Unavailable,
}
