//! Rule provenance, the three-mode matrix and bounded session grants.

use crate::admission::{AutoAdmission, automatic};
use crate::{Context, Operation, Risk, RiskReport, UserRule};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    Confirm,
    #[default]
    Auto,
    Yolo,
}

impl ApprovalMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "confirm" => Some(Self::Confirm),
            "auto" => Some(Self::Auto),
            "yolo" => Some(Self::Yolo),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Auto => "auto",
            Self::Yolo => "yolo",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Allow,
    Ask { strong: bool },
    Deny { reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecisionSource {
    UserDeny(String),
    UserAllow(Vec<String>),
    Builtin(String),
    Session,
    ReadOnly,
    Automatic(AutoAdmission),
    Mode(ApprovalMode, String),
}

impl DecisionSource {
    pub fn label(&self) -> String {
        match self {
            Self::UserDeny(rule) => format!("user deny: {rule}"),
            Self::UserAllow(rules) => format!("user allow: {}", rules.join("; ")),
            Self::Builtin(why) => format!("built-in prohibition: {why}"),
            Self::Session => "session grant (same operation and scope)".into(),
            Self::ReadOnly => "read-only".into(),
            Self::Automatic(a) => a.reasons.join("; "),
            Self::Mode(mode, why) => format!("{}: {why}", mode.as_str()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicyDecision {
    pub decision: Decision,
    pub source: DecisionSource,
}

#[derive(Debug, Clone, Default)]
pub struct UserRules {
    pub allow: Vec<UserRule>,
    pub deny: Vec<UserRule>,
}

#[derive(Debug, Clone)]
struct Grant {
    operations: Vec<Operation>,
    context: Context,
    scripts: Vec<(std::path::PathBuf, String)>,
}

#[derive(Debug, Clone, Default)]
pub struct SessionAllowList {
    grants: Vec<Grant>,
}

impl SessionAllowList {
    pub fn can_grant(report: &RiskReport) -> bool {
        report.risk() <= Risk::Mutating
            && !report.reads_protected
            && !report.incomplete
            && !report.operations.is_empty()
            && report.operations.iter().all(|op| {
                op.cwd_known
                    && !op.opaque
                    && (!op.network || !op.hosts.is_empty())
                    && op.known.iter().all(|k| *k)
                    && op.paths.iter().all(|path| path.resolved.is_some())
            })
    }

    pub fn grant(&mut self, report: &RiskReport) -> bool {
        if !Self::can_grant(report) {
            return false;
        }
        if !self.covers(report) {
            self.grants.push(Grant {
                operations: report.operations.clone(),
                context: report.context.clone(),
                scripts: report.scripts.clone(),
            });
        }
        true
    }

    pub fn covers(&self, report: &RiskReport) -> bool {
        Self::can_grant(report)
            && self.grants.iter().any(|grant| {
                grant.operations == report.operations
                    && grant.context == report.context
                    && grant.scripts == report.scripts
            })
    }

    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
    }
}

pub fn evaluate(
    report: &RiskReport,
    mode: ApprovalMode,
    rules: &UserRules,
    session: &SessionAllowList,
) -> PolicyDecision {
    let result = |decision, source| PolicyDecision { decision, source };
    let paths = crate::paths::PathResolver::new(&report.context);
    for rule in &rules.deny {
        if (0..report.operations.len()).any(|index| rule.denies_operation(report, index, &paths)) {
            let mut reason = rule.explanation();
            if report.operations.iter().any(|op| {
                op.opaque
                    || op.network && op.hosts.is_empty()
                    || op.paths.iter().any(|path| path.resolved.is_none())
            }) && (!rule.spec.write_paths.is_empty()
                || !rule.spec.read_paths.is_empty()
                || !rule.spec.hosts.is_empty())
            {
                reason.push_str(" (unresolved effects cannot be excluded from this deny scope)");
            }
            return result(
                Decision::Deny {
                    reason: format!("user deny: {reason}"),
                },
                DecisionSource::UserDeny(reason),
            );
        }
    }
    let mut matched = Vec::new();
    let allowed = !report.operations.is_empty()
        && report.operations.iter().enumerate().all(|(index, op)| {
            if let Some(rule) = rules
                .allow
                .iter()
                .find(|rule| rule.covers_operation(report, index, &paths))
            {
                let label = rule.explanation();
                if !matched.contains(&label) {
                    matched.push(label);
                }
                true
            } else {
                op.risk == Risk::Safe
                    && (!op.opaque || op.payload)
                    && op.paths.iter().all(|path| !path.extra)
                    && op.variables.is_empty()
            }
        });
    if allowed && !matched.is_empty() {
        return result(Decision::Allow, DecisionSource::UserAllow(matched));
    }
    let risk = report.risk();
    if risk == Risk::Forbidden {
        let reason = report.top_reasons().join("; ");
        let decision = if mode == ApprovalMode::Confirm {
            Decision::Ask { strong: true }
        } else {
            Decision::Deny {
                reason: format!("built-in prohibition: {reason}"),
            }
        };
        return result(decision, DecisionSource::Builtin(reason));
    }
    if session.covers(report) {
        return result(Decision::Allow, DecisionSource::Session);
    }
    if risk == Risk::Safe && !report.reads_protected && !report.changes_session {
        return result(Decision::Allow, DecisionSource::ReadOnly);
    }
    if mode == ApprovalMode::Yolo {
        return result(
            Decision::Allow,
            DecisionSource::Mode(
                mode,
                "non-prohibited operation; no per-call approval".into(),
            ),
        );
    }
    if mode == ApprovalMode::Auto && report.explicit_risk() < Risk::Dangerous {
        return match automatic(report) {
            Ok(admission) => result(Decision::Allow, DecisionSource::Automatic(admission)),
            Err(error) => result(
                Decision::Ask {
                    strong: risk.max(error.risk) >= Risk::Dangerous,
                },
                DecisionSource::Mode(mode, error.reason),
            ),
        };
    }
    result(
        Decision::Ask {
            strong: risk >= Risk::Dangerous,
        },
        DecisionSource::Mode(mode, report.top_reasons().join("; ")),
    )
}
