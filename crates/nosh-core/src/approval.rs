//! Approval channel: the terminal card (y/n/e/a, `yes` for Dangerous) and
//! non-interactive fallbacks (design §6.3, §4.5).

use std::collections::VecDeque;
use std::io::Write;

use nosh_hub::tr;
use nosh_permissions::Risk;
use nosh_shell::{style, term};

#[derive(Debug, Clone)]
pub struct ApprovalRequest {
    pub tool: String,
    pub command: String,
    pub risk: Risk,
    pub reasons: Vec<String>,
    /// Dangerous: the user must type `yes`.
    pub strong: bool,
    /// Offer "allow similar for this session".
    pub can_grant: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalResponse {
    Approve,
    /// Approve and allow this kind of command for the rest of the session.
    ApproveSimilar,
    Edit(String),
    Deny {
        reason: Option<String>,
    },
}

pub trait ApprovalChannel {
    fn request(&mut self, req: &ApprovalRequest) -> ApprovalResponse;
}

/// Without a terminal nothing that needs confirmation runs; the command is
/// reported on stderr so the user can run it themselves.
pub struct NoTerminal;

impl ApprovalChannel for NoTerminal {
    fn request(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
        eprintln!(
            "nosh: {} [{}]: {}",
            tr!(
                "需要确认，但没有终端；已拒绝",
                "needs confirmation but there is no terminal; denied"
            ),
            req.risk,
            req.command
        );
        ApprovalResponse::Deny {
            reason: Some(
                "confirmation required but no terminal is available (the user can rerun with --auto or --yolo)"
                    .into(),
            ),
        }
    }
}

/// Answers from a script (tests).
pub struct Scripted {
    pub answers: VecDeque<ApprovalResponse>,
    pub seen: Vec<ApprovalRequest>,
}

impl Scripted {
    pub fn new(answers: impl IntoIterator<Item = ApprovalResponse>) -> Self {
        Self {
            answers: answers.into_iter().collect(),
            seen: Vec::new(),
        }
    }
}

impl ApprovalChannel for Scripted {
    fn request(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
        self.seen.push(req.clone());
        self.answers.pop_front().unwrap_or(ApprovalResponse::Deny {
            reason: Some("no scripted answer".into()),
        })
    }
}

/// The interactive approval card.
pub struct TerminalApproval {
    pub bar: String,
}

impl Default for TerminalApproval {
    fn default() -> Self {
        Self {
            bar: crate::ui::bar(),
        }
    }
}

fn risk_color(risk: Risk, s: &str) -> String {
    match risk {
        Risk::Safe => style::green(s),
        Risk::Mutating => style::yellow(s),
        _ => style::red_bold(s),
    }
}

impl TerminalApproval {
    /// Picks the terminal channel when a terminal exists, else [`NoTerminal`].
    pub fn detect() -> Box<dyn ApprovalChannel> {
        if term::available() {
            Box::new(TerminalApproval::default())
        } else {
            Box::new(NoTerminal)
        }
    }

    fn card(&self, req: &ApprovalRequest) {
        let b = &self.bar;
        let mut err = std::io::stderr();
        let _ = writeln!(
            err,
            "{b} {} {} {}",
            style::dim("╭─"),
            style::bold(&req.tool),
            risk_color(req.risk, &format!("· {}", req.risk))
        );
        for (i, line) in req.command.lines().enumerate() {
            let p = if i == 0 { "$ " } else { "  " };
            let _ = writeln!(err, "{b} {} {p}{line}", style::dim("│"));
        }
        for r in req.reasons.iter().take(3) {
            let _ = writeln!(
                err,
                "{b} {} {}",
                style::dim("│"),
                style::dim(&format!("! {r}"))
            );
        }
    }

    fn deny_reason(&self) -> Option<String> {
        eprint!(
            "{} {}",
            self.bar,
            style::dim(tr!(
                "拒绝理由（可选，回车跳过）：",
                "reason (optional, Enter to skip): "
            ))
        );
        term::read_text("")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

impl ApprovalChannel for TerminalApproval {
    fn request(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
        term::flush_input();
        self.card(req);
        let b = &self.bar;
        if req.strong {
            eprint!(
                "{b} {} ",
                style::red_bold(tr!(
                    "╰─ 危险操作：键入 yes 执行，其他任意输入拒绝 ›",
                    "╰─ dangerous: type yes to run, anything else denies ›"
                ))
            );
            return match term::read_text("") {
                Some(s) if s.trim().eq_ignore_ascii_case("yes") => ApprovalResponse::Approve,
                Some(_) => ApprovalResponse::Deny {
                    reason: self.deny_reason(),
                },
                None => ApprovalResponse::Deny { reason: None },
            };
        }
        let grant = if req.can_grant {
            tr!("  [a] 本会话同类放行", "  [a] allow similar")
        } else {
            ""
        };
        eprint!(
            "{b} {}{}{} ",
            style::dim(tr!(
                "╰─ [y] 执行  [n] 拒绝  [e] 编辑",
                "╰─ [y] run  [n] deny  [e] edit"
            )),
            style::dim(grant),
            style::dim(" ›")
        );
        let _ = std::io::stderr().flush();
        loop {
            let Some(k) = term::read_key() else {
                eprintln!();
                return ApprovalResponse::Deny { reason: None };
            };
            use crossterm::event::KeyCode;
            if term::is_ctrl(&k, 'c') || k.code == KeyCode::Esc {
                eprintln!("n");
                return ApprovalResponse::Deny { reason: None };
            }
            match k.code {
                KeyCode::Char('y' | 'Y') => {
                    eprintln!("y");
                    return ApprovalResponse::Approve;
                }
                KeyCode::Char('a' | 'A') if req.can_grant => {
                    eprintln!("a");
                    return ApprovalResponse::ApproveSimilar;
                }
                KeyCode::Char('n' | 'N') => {
                    eprintln!("n");
                    return ApprovalResponse::Deny {
                        reason: self.deny_reason(),
                    };
                }
                KeyCode::Char('e' | 'E') => {
                    eprintln!("e");
                    let prompt = format!("{b}   $ ");
                    return match term::edit_line(&prompt, &req.command) {
                        Some(c) if !c.trim().is_empty() => ApprovalResponse::Edit(c),
                        _ => ApprovalResponse::Deny { reason: None },
                    };
                }
                _ => {}
            }
        }
    }
}
