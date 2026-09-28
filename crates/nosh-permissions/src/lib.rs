//! Risk analysis and approval policy for agent-issued commands (design §6).
//!
//! Commands are parsed with brush-parser — the same parser that executes them —
//! and every simple command (including those inside pipelines, lists,
//! subshells, `$(…)`, process substitutions, wrappers such as `sudo`/`env`/
//! `xargs`/`timeout`/`bash -c`/`eval`, and session aliases/functions) is
//! classified; the report carries the highest level found.

mod admission;
mod analyze;
mod paths;
mod policy;
mod rules;
mod user_rules;

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

pub use admission::AutoAdmission;
pub use analyze::{ProgramLookup, assess_command, assess_command_with_lookup};
pub use paths::{PathClass, classify_path, classify_path_real, real_path};
pub use policy::{
    ApprovalMode, Decision, DecisionSource, PolicyDecision, SessionAllowList, UserRules, evaluate,
};
pub use user_rules::{RuleSpec, UserRule};

/// Risk levels, ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum Risk {
    /// Read-only and offline.
    #[default]
    Safe,
    /// Writes, network access or session changes; not a recovery guarantee.
    Mutating,
    /// Destructive, irreversible, privilege escalation, remote code execution.
    Dangerous,
    /// A built-in prohibition, not a user deny rule.
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
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Context {
    pub cwd: PathBuf,
    /// Session start directory or its git root; writes outside bump the risk.
    pub workspace: PathBuf,
    pub home: Option<PathBuf>,
    /// The user's original home remains protected if a command changes HOME.
    pub user_home: Option<PathBuf>,
    /// Live alias table (`name → replacement text`).
    pub aliases: HashMap<String, String>,
    /// Live function table (`name → body text`).
    pub functions: HashMap<String, String>,
    /// Scalar shell variables of the session and their values, so a read of
    /// `"$VAR"` is checked against the protected paths like a literal one;
    /// those in `exported` also reach child processes such as scripts.
    pub variables: HashMap<String, String>,
    /// A full live snapshot can distinguish an unset name from missing context.
    pub variables_complete: bool,
    /// Defined variables whose values/attributes cannot be represented safely.
    pub unknown_variables: HashSet<String>,
    pub readonly_variables: HashSet<String>,
    /// Harness-owned execution overlays; None denotes a fresh per-run value.
    pub execution_variables: HashMap<String, Option<String>>,
    pub exported: HashSet<String>,
    /// Extra protected paths (already `~`-expanded).
    pub protected: Vec<PathBuf>,
    /// The actual execution deadline also bounds ordinary network diagnostics.
    pub timeout: std::time::Duration,
}

impl Context {
    pub fn new(cwd: impl Into<PathBuf>, workspace: impl Into<PathBuf>) -> Self {
        let mut ctx = Self {
            cwd: cwd.into(),
            workspace: workspace.into(),
            home: std::env::var_os("HOME").map(PathBuf::from),
            timeout: std::time::Duration::from_secs(60),
            ..Self::default()
        };
        ctx.user_home = ctx.home.clone();
        ctx.variables
            .insert("PWD".into(), ctx.cwd.to_string_lossy().into_owned());
        if let Some(home) = &ctx.home {
            ctx.variables
                .insert("HOME".into(), home.to_string_lossy().into_owned());
            ctx.exported.insert("HOME".into());
        }
        ctx
    }

    pub fn with_home(mut self, home: impl Into<PathBuf>) -> Self {
        self.home = Some(home.into());
        self.user_home = self.home.clone();
        if let Some(home) = &self.home {
            self.variables
                .insert("HOME".into(), home.to_string_lossy().into_owned());
            self.exported.insert("HOME".into());
        }
        self
    }

    pub fn resolve(&self, p: &str) -> PathBuf {
        paths::resolve(p, &self.cwd, self.home.as_deref())
    }

    pub fn resolve_workspace(&self, p: &str) -> PathBuf {
        paths::resolve(p, &self.workspace, self.home.as_deref())
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
    pub uncertain: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccessKind {
    Read,
    Write,
    Delete,
    List { depth: usize },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathAccess {
    pub kind: AccessKind,
    pub lexical: Option<PathBuf>,
    pub resolved: Option<PathBuf>,
    /// An effect outside argv, such as a shell redirection.
    pub extra: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Operation {
    pub tool: String,
    pub parent: Option<usize>,
    pub argv: Vec<String>,
    pub executable: Option<PathBuf>,
    pub local_program: bool,
    pub known: Vec<bool>,
    pub may_disappear: Vec<bool>,
    pub cwd: PathBuf,
    pub paths: Vec<PathAccess>,
    pub cwd_known: bool,
    pub variables: Vec<(String, Option<String>)>,
    pub extra_variables: bool,
    pub network: bool,
    pub hosts: Vec<String>,
    pub risk: Risk,
    /// An analyzed wrapper whose child operations are recorded separately.
    pub transparent: bool,
    /// The invocation is known, but its implementation has unknown effects.
    pub opaque: bool,
    /// An actual script/interpreter invocation owns these child operations;
    /// an alias or a sibling command does not.
    pub payload: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
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
    pub operations: Vec<Operation>,
    pub context: Context,
    pub incomplete: bool,
    pub syntax_error: Option<String>,
    /// Exact script versions used for this analysis, bounded by the analyzer.
    pub scripts: Vec<(PathBuf, String)>,
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
        self.add_finding(risk, reason.into(), false);
    }

    pub(crate) fn uncertain(&mut self, risk: Risk, reason: impl Into<String>) {
        self.add_finding(risk, reason.into(), true);
    }

    fn add_finding(&mut self, risk: Risk, reason: String, uncertain: bool) {
        if !self
            .findings
            .iter()
            .any(|f| f.risk == risk && f.reason == reason && f.uncertain == uncertain)
        {
            self.findings.push(Finding {
                risk,
                reason,
                uncertain,
            });
        }
    }

    pub fn explicit_risk(&self) -> Risk {
        self.findings
            .iter()
            .filter(|f| !f.uncertain)
            .map(|f| f.risk)
            .max()
            .unwrap_or_default()
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

/// The read tools authorize their real path and traversal scope before reading.
pub fn assess_read(tool: &str, path: &Path, depth: Option<usize>, ctx: &Context) -> RiskReport {
    let real = real_path(path, true).unwrap_or_else(|| path.to_path_buf());
    let kind = depth.map_or(AccessKind::Read, |depth| AccessKind::List { depth });
    let mut op = Operation {
        tool: tool.into(),
        cwd: ctx.cwd.clone(),
        cwd_known: true,
        argv: vec![
            path.to_string_lossy().into_owned(),
            depth.unwrap_or(0).to_string(),
        ],
        known: vec![true, true],
        paths: vec![PathAccess {
            kind,
            lexical: Some(path.to_path_buf()),
            resolved: Some(real.clone()),
            extra: false,
        }],
        ..Operation::default()
    };
    let mut report = RiskReport {
        context: ctx.clone(),
        ..RiskReport::default()
    };
    let class = match classify_path(path, ctx) {
        protected @ PathClass::Protected(_) => protected,
        _ => classify_path(&real, ctx),
    };
    if let PathClass::Protected(what) = class {
        report.add(Risk::Mutating, format!("reads protected path {what}"));
        report.reads_protected = true;
    }
    if let Some(depth) = depth {
        for (protected, what) in paths::protected_list(ctx) {
            let resolved = real_path(&protected, true).unwrap_or_else(|| protected.clone());
            if paths::relative_path(&resolved, &real)
                .is_some_and(|relative| relative.components().count() < depth)
            {
                report.add(
                    Risk::Mutating,
                    format!("traverses protected directory {what}"),
                );
                report.reads_protected = true;
                op.paths.push(PathAccess {
                    kind: AccessKind::List { depth },
                    lexical: Some(protected.clone()),
                    resolved: Some(resolved),
                    extra: false,
                });
            }
        }
    }
    op.risk = report.risk();
    report.operations.push(op);
    report
}
