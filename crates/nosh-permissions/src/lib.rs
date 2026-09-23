//! Risk analysis and approval policy for agent-issued commands (design §6).
//!
//! Commands are parsed with brush-parser — the same parser that executes them —
//! and every simple command (including those inside pipelines, lists,
//! subshells, `$(…)`, process substitutions, wrappers such as `sudo`/`env`/
//! `xargs`/`timeout`/`bash -c`/`eval`, and session aliases/functions) is
//! classified; the report carries the highest level found.

mod analyze;
mod paths;
mod policy;
mod rules;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub use analyze::assess_command;
pub use paths::{PathClass, classify_path};
pub use policy::{ApprovalMode, Decision, SessionAllowList, UserRules, decide, glob_match};

/// Risk levels, ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Risk {
    /// Read-only and offline.
    Safe,
    /// Recoverable writes, network access, session-state changes.
    Mutating,
    /// Destructive, irreversible, privilege escalation, remote code execution.
    Dangerous,
    /// Never allowed.
    Forbidden,
}

impl Risk {
    pub fn label(self) -> &'static str {
        match self {
            Risk::Safe => "SAFE",
            Risk::Mutating => "MUTATING",
            Risk::Dangerous => "DANGEROUS",
            Risk::Forbidden => "FORBIDDEN",
        }
    }

    pub fn bump(self) -> Risk {
        match self {
            Risk::Safe => Risk::Mutating,
            Risk::Mutating => Risk::Dangerous,
            r => r,
        }
    }
}

impl std::fmt::Display for Risk {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Session information the analysis needs.
#[derive(Debug, Clone, Default)]
pub struct Context {
    pub cwd: PathBuf,
    /// Session start directory or its git root; writes outside bump the risk.
    pub workspace: PathBuf,
    pub home: Option<PathBuf>,
    /// Live alias table (`name → replacement text`).
    pub aliases: HashMap<String, String>,
    /// Live function table (`name → body text`).
    pub functions: HashMap<String, String>,
    /// Extra protected paths (already `~`-expanded).
    pub protected: Vec<PathBuf>,
}

impl Context {
    pub fn new(cwd: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        Self {
            cwd: cwd.into(),
            workspace: workspace.into(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            ..Self::default()
        }
    }

    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self
    }

    pub fn resolve(&self, p: &str) -> PathBuf {
        paths::resolve(p, &self.cwd, self.home.as_deref())
    }

    pub fn home_dir(&self) -> Option<&Path> {
        self.home.as_deref()
    }
}

/// What a command was found to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub risk: Risk,
    pub reason: String,
}

#[derive(Debug, Clone, Default)]
pub struct RiskReport {
    pub findings: Vec<Finding>,
    /// Simple commands found, for display.
    pub commands: Vec<String>,
    /// Command to run instead (e.g. `sudo` rewritten to `sudo -n`).
    pub rewritten: Option<String>,
    pub network: bool,
    pub writes_outside_workspace: bool,
    pub changes_session: bool,
    pub reads_protected: bool,
}

impl RiskReport {
    pub fn risk(&self) -> Risk {
        self.findings
            .iter()
            .map(|f| f.risk)
            .max()
            .unwrap_or(Risk::Safe)
    }

    pub fn add(&mut self, risk: Risk, reason: impl Into<String>) {
        let reason = reason.into();
        if !self
            .findings
            .iter()
            .any(|f| f.risk == risk && f.reason == reason)
        {
            self.findings.push(Finding { risk, reason });
        }
    }

    /// Reasons at the report's highest level (for approval cards).
    pub fn top_reasons(&self) -> Vec<&str> {
        let r = self.risk();
        self.findings
            .iter()
            .filter(|f| f.risk == r && r > Risk::Safe)
            .map(|f| f.reason.as_str())
            .collect()
    }
}
