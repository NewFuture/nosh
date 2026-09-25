//! Deciding what to do with a line typed at the prompt (design §4.2): valid
//! commands run as-is; `#` lines, unparseable prose and unknown command names
//! go to the AI; typos that match a known command are corrected locally.

use brush_parser::ast;

use crate::backend::{EmbeddedShell, Resolution};
use crate::{guard, spell};

/// Why the AI was invoked (`trigger=` in the task header).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Hash,
    ParseError,
    NotFound,
    Failed { exit: i32 },
    Builtin,
    Cli,
    Pipe,
}

impl Trigger {
    pub fn name(&self) -> &'static str {
        match self {
            Trigger::Hash => "hash",
            Trigger::ParseError => "parse_error",
            Trigger::NotFound => "not_found",
            Trigger::Failed { .. } => "failed",
            Trigger::Builtin => "ai",
            Trigger::Cli => "cli",
            Trigger::Pipe => "pipe",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Empty,
    Execute,
    Ai {
        trigger: Trigger,
        text: String,
    },
    /// Local spelling fix; the corrected line is offered, never run automatically.
    Correct {
        corrected: String,
        from: String,
        to: String,
    },
    /// Destructive command with prose-like arguments: ask first.
    Guard,
    /// `ai …` builtin; the rest of the line after the name.
    AiBuiltin(String),
}

#[derive(Debug, Clone)]
pub struct TriggerConfig {
    pub ai_prefix: String,
    pub builtin_name: String,
    pub trigger_on_error: bool,
    pub nl_guard: bool,
    pub ai_enabled: bool,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            ai_prefix: "#".into(),
            builtin_name: "ai".into(),
            trigger_on_error: true,
            nl_guard: true,
            ai_enabled: true,
        }
    }
}

pub fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x3040..=0x30ff | 0x3400..=0x4dbf | 0x4e00..=0x9fff | 0xac00..=0xd7af | 0xf900..=0xfaff | 0xff00..=0xffef | 0x3000..=0x303f)
    })
}

/// Word-internal apostrophes (`what's`, `don't`) in a line with no other shell
/// syntax are prose, not an unterminated quote.
pub fn apostrophe_prose(line: &str) -> bool {
    let chars: Vec<char> = line.chars().collect();
    let mut quotes = 0;
    for (i, c) in chars.iter().enumerate() {
        match c {
            '\'' => {
                let prev = i.checked_sub(1).and_then(|j| chars.get(j));
                let next = chars.get(i + 1);
                if !(prev.is_some_and(|p| p.is_alphabetic())
                    && next.is_some_and(|n| n.is_alphabetic()))
                {
                    return false;
                }
                quotes += 1;
            }
            '|' | ';' | '&' | '>' | '<' | '$' | '`' | '"' | '(' | ')' | '{' | '}' | '\\' | '='
            | '*' => {
                return false;
            }
            _ => {}
        }
    }
    quotes > 0
}

pub fn is_incomplete(e: &brush_parser::ParseError) -> bool {
    match e {
        brush_parser::ParseError::Tokenizing { inner, .. } => inner.is_incomplete(),
        brush_parser::ParseError::ParsingAtEndOfInput => true,
        _ => false,
    }
}

#[derive(Debug)]
struct SimpleCmd {
    name: String,
    /// Character span of the command name in the line.
    span: Option<(usize, usize)>,
    argv: Vec<String>,
}

/// Only used for unresolved names: question words can otherwise look like
/// typos (`can` -> `cat`, `is` -> `ls`, `why` -> `who`).
fn looks_like_question(argv: &[String]) -> bool {
    let [first, second, third, ..] = argv else {
        return false;
    };
    let auxiliary = |word: &str| {
        matches!(
            word,
            "am" | "is"
                | "are"
                | "was"
                | "were"
                | "do"
                | "does"
                | "did"
                | "can"
                | "could"
                | "will"
                | "would"
                | "should"
                | "may"
                | "might"
                | "must"
                | "have"
                | "has"
                | "had"
        )
    };
    let second = second.to_ascii_lowercase();
    match first.to_ascii_lowercase().as_str() {
        "why" | "where" | "when" => auxiliary(&second),
        "how" => auxiliary(&second) || matches!(second.as_str(), "many" | "much"),
        "what" | "which" | "whose" => auxiliary(&second) || auxiliary(&third.to_ascii_lowercase()),
        word if auxiliary(word) => {
            matches!(
                second.as_str(),
                "i" | "you"
                    | "he"
                    | "she"
                    | "it"
                    | "we"
                    | "they"
                    | "there"
                    | "this"
                    | "that"
                    | "these"
                    | "those"
                    | "my"
                    | "your"
                    | "his"
                    | "her"
                    | "our"
                    | "their"
                    | "the"
                    | "a"
                    | "an"
            ) || argv.last().is_some_and(|arg| arg.ends_with('?'))
        }
        _ => false,
    }
}

fn static_word(w: &ast::Word) -> Option<String> {
    let opts = brush_parser::ParserOptions::default();
    let pieces = brush_parser::word::parse(&w.value, &opts).ok()?;
    let mut s = String::new();
    fn walk(p: &[brush_parser::word::WordPieceWithSource], s: &mut String) -> bool {
        use brush_parser::word::WordPiece as W;
        for x in p {
            match &x.piece {
                W::Text(t) | W::SingleQuotedText(t) => s.push_str(t),
                W::EscapeSequence(t) => s.push_str(t.strip_prefix('\\').unwrap_or(t)),
                W::DoubleQuotedSequence(inner) => {
                    if !walk(inner, s) {
                        return false;
                    }
                }
                W::TildeExpansion(_) => s.push('~'),
                _ => return false,
            }
        }
        true
    }
    walk(&pieces, &mut s).then_some(s)
}

fn collect(prog: &ast::Program, out: &mut Vec<SimpleCmd>, defined: &mut Vec<String>) {
    fn list(cl: &ast::CompoundList, out: &mut Vec<SimpleCmd>, defined: &mut Vec<String>) {
        for ast::CompoundListItem(aol, _) in &cl.0 {
            pipeline(&aol.first, out, defined);
            for x in &aol.additional {
                match x {
                    ast::AndOr::And(p) | ast::AndOr::Or(p) => pipeline(p, out, defined),
                }
            }
        }
    }
    fn pipeline(p: &ast::Pipeline, out: &mut Vec<SimpleCmd>, defined: &mut Vec<String>) {
        for c in &p.seq {
            match c {
                ast::Command::Simple(sc) => {
                    let Some(w) = &sc.word_or_name else {
                        continue;
                    };
                    let Some(name) = static_word(w) else {
                        continue;
                    };
                    let mut argv = vec![name.clone()];
                    if let Some(suffix) = &sc.suffix {
                        for item in &suffix.0 {
                            if let ast::CommandPrefixOrSuffixItem::Word(w) = item {
                                argv.push(static_word(w).unwrap_or_else(|| w.value.clone()));
                            }
                        }
                    }
                    out.push(SimpleCmd {
                        name,
                        span: w.loc.as_ref().map(|l| (l.start.index, l.end.index)),
                        argv,
                    });
                }
                ast::Command::Compound(cc, _) => compound(cc, out, defined),
                ast::Command::Function(fd) => {
                    if let Some(n) = static_word(&fd.fname) {
                        defined.push(n);
                    }
                }
                ast::Command::ExtendedTest(..) => {}
            }
        }
    }
    fn compound(cc: &ast::CompoundCommand, out: &mut Vec<SimpleCmd>, defined: &mut Vec<String>) {
        use ast::CompoundCommand as C;
        match cc {
            C::BraceGroup(b) => list(&b.list, out, defined),
            C::Subshell(s) => list(&s.list, out, defined),
            C::ForClause(f) => list(&f.body.list, out, defined),
            C::ArithmeticForClause(f) => list(&f.body.list, out, defined),
            C::CaseClause(c) => {
                for item in &c.cases {
                    if let Some(cmd) = &item.cmd {
                        list(cmd, out, defined);
                    }
                }
            }
            C::IfClause(i) => {
                list(&i.condition, out, defined);
                list(&i.then, out, defined);
                for e in i.elses.iter().flatten() {
                    if let Some(c) = &e.condition {
                        list(c, out, defined);
                    }
                    list(&e.body, out, defined);
                }
            }
            C::WhileClause(w) | C::UntilClause(w) => {
                list(&w.0, out, defined);
                list(&w.1.list, out, defined);
            }
            C::Arithmetic(_) | C::Coprocess(_) => {}
        }
    }
    for cl in &prog.complete_commands {
        list(cl, out, defined);
    }
}

fn replace_spans(line: &str, edits: &mut [((usize, usize), String)]) -> String {
    let chars: Vec<char> = line.chars().collect();
    edits.sort_by_key(|e| std::cmp::Reverse(e.0.0));
    let mut out = chars.clone();
    for ((s, e), rep) in edits.iter() {
        if *s <= *e && *e <= out.len() {
            out.splice(*s..*e, rep.chars());
        }
    }
    out.into_iter().collect()
}

/// Classifies an input line.
pub fn classify(line: &str, shell: &mut EmbeddedShell, cfg: &TriggerConfig) -> Action {
    let t = line.trim();
    if t.is_empty() {
        return Action::Empty;
    }
    if !cfg.ai_enabled {
        return Action::Execute;
    }
    if !cfg.ai_prefix.is_empty() && t.starts_with(&cfg.ai_prefix) {
        let text = t[cfg.ai_prefix.len()..].trim();
        return if text.is_empty() {
            Action::Empty
        } else {
            Action::Ai {
                trigger: Trigger::Hash,
                text: text.to_string(),
            }
        };
    }
    if let Some(rest) = t.strip_prefix(cfg.builtin_name.as_str())
        && (rest.is_empty() || rest.starts_with(char::is_whitespace))
        && !cfg.builtin_name.is_empty()
        && shell.resolve(&cfg.builtin_name) == Resolution::NotFound
    {
        return Action::AiBuiltin(rest.trim().to_string());
    }
    let prog = match shell.parse(t) {
        Ok(p) => p,
        Err(e) => {
            if apostrophe_prose(t) || (!is_incomplete(&e) && cfg.trigger_on_error) {
                return Action::Ai {
                    trigger: Trigger::ParseError,
                    text: t.to_string(),
                };
            }
            return Action::Execute;
        }
    };
    let mut cmds = Vec::new();
    let mut defined = Vec::new();
    collect(&prog, &mut cmds, &mut defined);
    let missing: Vec<&SimpleCmd> = cmds
        .iter()
        .filter(|c| !defined.contains(&c.name) && shell.resolve(&c.name) == Resolution::NotFound)
        .collect();
    if !missing.is_empty() {
        let names = shell.command_names();
        let mut edits = Vec::new();
        let mut first = None;
        for m in &missing {
            if looks_like_question(&m.argv) {
                edits.clear();
                break;
            }
            let fix = spell::ranked_matches(&m.name, &names)
                .into_iter()
                .find(|c| shell.resolve(c) != Resolution::NotFound);
            match (fix, m.span) {
                (Some(fix), Some(span)) if !m.name.contains('/') => {
                    first.get_or_insert((m.name.clone(), fix.to_string()));
                    edits.push((span, fix.to_string()));
                }
                _ => {
                    edits.clear();
                    break;
                }
            }
        }
        if !edits.is_empty() {
            let (from, to) = first.unwrap_or_default();
            return Action::Correct {
                corrected: replace_spans(t, &mut edits),
                from,
                to,
            };
        }
        return if cfg.trigger_on_error {
            Action::Ai {
                trigger: Trigger::NotFound,
                text: t.to_string(),
            }
        } else {
            Action::Execute
        };
    }
    if cfg.nl_guard {
        let cwd = shell.cwd();
        if cmds.iter().any(|c| guard::looks_like_prose(&c.argv, &cwd)) {
            return Action::Guard;
        }
    }
    Action::Execute
}

/// Commands whose exit status 1 means "no result", not failure.
const QUIET_FAILURES: &[&str] = &[
    "grep", "egrep", "fgrep", "zgrep", "rg", "ag", "diff", "cmp", "test", "[", "[[", "false",
    "pgrep", "pidof", "which", "type", "command",
];

/// Whether a failed user command should offer (or trigger) AI help.
pub fn failure_is_notable(line: &str, exit: i32) -> bool {
    if exit == 0 || exit == 130 || exit == 141 || exit == 148 {
        return false;
    }
    // The status comes from the last command of the line.
    let last = line
        .rsplit(['|', ';', '&', '\n'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or(line);
    let first = last.split_whitespace().next().unwrap_or("");
    !(exit == 1 && QUIET_FAILURES.contains(&first))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apostrophes() {
        assert!(apostrophe_prose("what's using port 8080"));
        assert!(apostrophe_prose("don't delete my files"));
        assert!(!apostrophe_prose("echo 'hi"));
        assert!(!apostrophe_prose("what's > out"));
        assert!(!apostrophe_prose("plain words"));
    }

    #[test]
    fn cjk() {
        assert!(contains_cjk("帮我看看 8080 端口"));
        assert!(!contains_cjk("ls -la"));
    }

    #[test]
    fn span_replacement() {
        let mut e = vec![((0, 3), "git".to_string())];
        assert_eq!(replace_spans("gti status", &mut e), "git status");
        let mut e = vec![((0, 2), "ls".to_string()), ((6, 9), "git".to_string())];
        assert_eq!(replace_spans("sl && gti push", &mut e), "ls && git push");
    }

    #[test]
    fn notable_failures() {
        assert!(failure_is_notable("npm start", 1));
        assert!(!failure_is_notable("grep x y", 1));
        assert!(!failure_is_notable("echo a | grep -q zzz", 1));
        assert!(failure_is_notable("grep x y | sort", 1));
        assert!(!failure_is_notable("sleep 10", 130));
        assert!(failure_is_notable("grep x y", 2));
    }
}
