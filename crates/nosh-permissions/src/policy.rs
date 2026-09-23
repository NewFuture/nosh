//! Approval policy: the confirm/auto/yolo decision matrix (design §6.3), user
//! allow/deny rules and per-session "allow this kind" grants.

use crate::{Risk, RiskReport};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ApprovalMode {
    #[default]
    Confirm,
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
    /// Needs the user's approval; `strong` means typing `yes`.
    Ask {
        strong: bool,
    },
    Deny {
        reason: String,
    },
}

/// `[safety] allow/deny` glob rules from the config (matched on the whole command).
#[derive(Debug, Clone, Default)]
pub struct UserRules {
    pub allow: Vec<String>,
    pub deny: Vec<String>,
}

/// `*` matches any run of characters, `?` one character.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let (mut star, mut mark) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

impl UserRules {
    fn denies(&self, cmd: &str) -> Option<&str> {
        self.deny
            .iter()
            .find(|p| glob_match(p, cmd.trim()))
            .map(String::as_str)
    }

    fn allows(&self, cmd: &str) -> bool {
        self.allow.iter().any(|p| glob_match(p, cmd.trim()))
    }
}

/// Grants from answering `a` on an approval card: exact command prefixes,
/// only for commands whose risk is at most Mutating.
#[derive(Debug, Clone, Default)]
pub struct SessionAllowList {
    prefixes: Vec<String>,
}

const SUBCOMMAND_TOOLS: &[&str] = &[
    "git",
    "docker",
    "podman",
    "kubectl",
    "npm",
    "pnpm",
    "yarn",
    "cargo",
    "go",
    "pip",
    "pip3",
    "apt",
    "apt-get",
    "systemctl",
    "brew",
    "make",
    "uv",
    "poetry",
    "helm",
    "gh",
];

impl SessionAllowList {
    /// The prefix a grant for `simple_command` covers (`git add`, `mkdir`, …).
    pub fn prefix_of(simple_command: &str) -> String {
        let words: Vec<&str> = simple_command.split_whitespace().collect();
        let first = words.first().copied().unwrap_or("");
        if SUBCOMMAND_TOOLS.contains(&first) {
            let mut i = 1;
            while i < words.len() && words[i].starts_with('-') {
                i += if matches!(
                    words[i],
                    "-C" | "-c"
                        | "--git-dir"
                        | "--work-tree"
                        | "-f"
                        | "--file"
                        | "-n"
                        | "--namespace"
                ) {
                    2
                } else {
                    1
                };
            }
            if let Some(sub) = words.get(i) {
                return format!("{first} {sub}");
            }
        }
        first.to_string()
    }

    pub fn grant(&mut self, report: &RiskReport) {
        for c in &report.commands {
            let p = Self::prefix_of(c);
            if !p.is_empty() && !self.prefixes.contains(&p) {
                self.prefixes.push(p);
            }
        }
    }

    pub fn covers(&self, report: &RiskReport) -> bool {
        report.risk() <= Risk::Mutating
            && !report.commands.is_empty()
            && report
                .commands
                .iter()
                .all(|c| self.prefixes.contains(&Self::prefix_of(c)))
    }

    pub fn prefixes(&self) -> &[String] {
        &self.prefixes
    }
}

/// Decision matrix:
///
/// | mode    | Safe  | Mutating                          | Dangerous | Forbidden |
/// |---------|-------|-----------------------------------|-----------|-----------|
/// | confirm | allow | ask                               | ask (yes) | deny      |
/// | auto    | allow | allow in workspace, else ask      | ask (yes) | deny      |
/// | yolo    | allow | allow                             | ask       | deny      |
pub fn decide(
    report: &RiskReport,
    command: &str,
    mode: ApprovalMode,
    rules: &UserRules,
    session: &SessionAllowList,
) -> Decision {
    let risk = report.risk();
    if risk == Risk::Forbidden {
        let why = report
            .findings
            .iter()
            .find(|f| f.risk == Risk::Forbidden)
            .map(|f| f.reason.clone())
            .unwrap_or_else(|| "forbidden".into());
        return Decision::Deny { reason: why };
    }
    if let Some(p) = rules.denies(command) {
        return Decision::Deny {
            reason: format!("matches deny rule '{p}'"),
        };
    }
    if rules.allows(command) || session.covers(report) {
        return Decision::Allow;
    }
    match (risk, mode) {
        (Risk::Safe, _) => Decision::Allow,
        (Risk::Mutating, ApprovalMode::Yolo) => Decision::Allow,
        (Risk::Mutating, ApprovalMode::Auto) => {
            if report.writes_outside_workspace
                || report.network
                || report.changes_session
                || report.reads_protected
            {
                Decision::Ask { strong: false }
            } else {
                Decision::Allow
            }
        }
        (Risk::Mutating, ApprovalMode::Confirm) => Decision::Ask { strong: false },
        (Risk::Dangerous, ApprovalMode::Yolo) => Decision::Ask { strong: false },
        (Risk::Dangerous, _) => Decision::Ask { strong: true },
        (Risk::Forbidden, _) => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Finding;

    fn report(risk: Risk) -> RiskReport {
        RiskReport {
            findings: vec![Finding {
                risk,
                reason: "x".into(),
            }],
            commands: vec!["git add a.txt".into()],
            ..RiskReport::default()
        }
    }

    #[test]
    fn globbing() {
        assert!(glob_match("git status*", "git status -s"));
        assert!(glob_match("git status*", "git status"));
        assert!(!glob_match("git status*", "git stash"));
        assert!(glob_match(
            "docker system prune*",
            "docker system prune -af"
        ));
        assert!(glob_match("a?c", "abc"));
        assert!(glob_match("*", ""));
    }

    #[test]
    fn matrix() {
        let r = UserRules::default();
        let s = SessionAllowList::default();
        use ApprovalMode::*;
        assert_eq!(
            decide(&report(Risk::Safe), "ls", Confirm, &r, &s),
            Decision::Allow
        );
        assert_eq!(
            decide(&report(Risk::Mutating), "x", Confirm, &r, &s),
            Decision::Ask { strong: false }
        );
        assert_eq!(
            decide(&report(Risk::Mutating), "x", Auto, &r, &s),
            Decision::Allow
        );
        let mut outside = report(Risk::Mutating);
        outside.writes_outside_workspace = true;
        assert_eq!(
            decide(&outside, "x", Auto, &r, &s),
            Decision::Ask { strong: false }
        );
        assert_eq!(decide(&outside, "x", Yolo, &r, &s), Decision::Allow);
        assert_eq!(
            decide(&report(Risk::Dangerous), "x", Confirm, &r, &s),
            Decision::Ask { strong: true }
        );
        assert_eq!(
            decide(&report(Risk::Dangerous), "x", Auto, &r, &s),
            Decision::Ask { strong: true }
        );
        assert_eq!(
            decide(&report(Risk::Dangerous), "x", Yolo, &r, &s),
            Decision::Ask { strong: false }
        );
        assert!(matches!(
            decide(&report(Risk::Forbidden), "x", Yolo, &r, &s),
            Decision::Deny { .. }
        ));
    }

    #[test]
    fn user_rules_and_session_grants() {
        let r = UserRules {
            allow: vec!["git add*".into(), "rm -rf /".into()],
            deny: vec!["docker system prune*".into()],
        };
        let s = SessionAllowList::default();
        assert_eq!(
            decide(
                &report(Risk::Mutating),
                "git add a.txt",
                ApprovalMode::Confirm,
                &r,
                &s
            ),
            Decision::Allow
        );
        assert!(matches!(
            decide(
                &report(Risk::Safe),
                "docker system prune -a",
                ApprovalMode::Yolo,
                &r,
                &s
            ),
            Decision::Deny { .. }
        ));
        // allow cannot override Forbidden
        assert!(matches!(
            decide(
                &report(Risk::Forbidden),
                "rm -rf /",
                ApprovalMode::Confirm,
                &r,
                &s
            ),
            Decision::Deny { .. }
        ));

        let mut grants = SessionAllowList::default();
        grants.grant(&report(Risk::Mutating));
        assert_eq!(grants.prefixes(), &["git add".to_string()]);
        let none = UserRules::default();
        assert_eq!(
            decide(
                &report(Risk::Mutating),
                "git add b.txt",
                ApprovalMode::Confirm,
                &none,
                &grants
            ),
            Decision::Allow
        );
        // grants never cover Dangerous
        assert_eq!(
            decide(
                &report(Risk::Dangerous),
                "git add b.txt",
                ApprovalMode::Confirm,
                &none,
                &grants
            ),
            Decision::Ask { strong: true }
        );
        assert_eq!(SessionAllowList::prefix_of("mkdir -p x"), "mkdir");
        assert_eq!(
            SessionAllowList::prefix_of("git -C d commit -m x"),
            "git commit"
        );
    }
}
