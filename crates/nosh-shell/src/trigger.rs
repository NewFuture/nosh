//! Deciding what to do with a line typed at the prompt (design §4.2): valid
//! commands run as-is; prefix commands are parsed locally, while spaced prefix
//! tasks, prose and unknown names may go to AI. Typos are corrected locally.

use brush_parser::ast;

use crate::backend::{EmbeddedShell, Resolution};
use crate::command_context::{self, Scope};
use crate::{guard, inline_commands, spell};

/// Internal reason the AI was invoked; not exposed in the model's task header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trigger {
    Hash,
    ParseError,
    NotFound,
    Failed { exit: i32 },
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
    Inline(inline_commands::Command),
    InvalidInline(inline_commands::Error),
}

#[derive(Debug, Clone)]
pub struct TriggerConfig {
    pub ai_prefix: String,
    pub trigger_on_error: bool,
    pub nl_guard: bool,
    pub ai_enabled: bool,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            ai_prefix: "#".into(),
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
    resolution: Option<Resolution>,
    path: Option<String>,
    correction_blocked: bool,
    runtime_dependent: bool,
}

/// Only used for unresolved names: question words can otherwise look like
/// typos (`can` -> `cat`, `is` -> `ls`, `why` -> `who`).
pub(crate) fn looks_like_question(argv: &[String]) -> bool {
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

pub(crate) fn static_word(w: &ast::Word) -> Option<String> {
    static_word_with_tilde(w, &|_| None)
}

pub(crate) fn static_word_with_tilde(
    w: &ast::Word,
    expand_tilde: &impl Fn(&brush_parser::word::TildeExpr) -> Option<String>,
) -> Option<String> {
    let opts = brush_parser::ParserOptions::default();
    let pieces = brush_parser::word::parse(&w.value, &opts).ok()?;
    static_word_pieces(&pieces, expand_tilde)
}

pub(crate) fn static_word_pieces(
    pieces: &[brush_parser::word::WordPieceWithSource],
    expand_tilde: &impl Fn(&brush_parser::word::TildeExpr) -> Option<String>,
) -> Option<String> {
    let mut s = String::new();
    fn walk(
        p: &[brush_parser::word::WordPieceWithSource],
        s: &mut String,
        expand_tilde: &impl Fn(&brush_parser::word::TildeExpr) -> Option<String>,
    ) -> bool {
        use brush_parser::word::WordPiece as W;
        for x in p {
            match &x.piece {
                W::Text(t) | W::SingleQuotedText(t) => s.push_str(t),
                W::EscapeSequence(t) => s.push_str(t.strip_prefix('\\').unwrap_or(t)),
                W::DoubleQuotedSequence(inner) => {
                    if !walk(inner, s, expand_tilde) {
                        return false;
                    }
                }
                W::TildeExpansion(expr) => {
                    let Some(value) = expand_tilde(expr) else {
                        return false;
                    };
                    s.push_str(&value);
                }
                _ => return false,
            }
        }
        true
    }
    walk(pieces, &mut s, expand_tilde).then_some(s)
}

struct Collector<'a> {
    shell: &'a EmbeddedShell,
    options: brush_parser::ParserOptions,
    commands: Vec<SimpleCmd>,
    remaining: usize,
}

impl Collector<'_> {
    fn literal(&self, word: &ast::Word) -> Option<String> {
        command_context::literal(word, &self.options, &|expr| match expr {
            brush_parser::word::TildeExpr::Home => self.shell.var("HOME"),
            brush_parser::word::TildeExpr::WorkingDir => {
                self.shell.cwd().to_str().map(str::to_owned)
            }
            brush_parser::word::TildeExpr::OldWorkingDir => self.shell.var("OLDPWD"),
            _ => None,
        })
    }

    fn list(&mut self, cl: &ast::CompoundList, scope: &mut Scope, depth: usize) {
        if depth > 64 || self.remaining == 0 {
            scope.dynamic = true;
            return;
        }
        for ast::CompoundListItem(aol, separator) in &cl.0 {
            let mut child = scope.clone();
            self.pipeline(&aol.first, &mut child, depth + 1);
            for x in &aol.additional {
                let mut branch = child.clone();
                match x {
                    ast::AndOr::And(p) | ast::AndOr::Or(p) => {
                        self.pipeline(p, &mut branch, depth + 1)
                    }
                }
                child.merge_optional(&branch);
            }
            if matches!(separator, ast::SeparatorOperator::Async) {
                scope.files_changed = true;
            } else {
                *scope = child;
            }
        }
    }
    fn pipeline(&mut self, p: &ast::Pipeline, scope: &mut Scope, depth: usize) {
        if p.seq.len() == 1 {
            self.command(&p.seq[0], scope, depth + 1);
        } else {
            for (index, c) in p.seq.iter().enumerate() {
                let mut child = scope.clone();
                child.files_changed = true;
                self.command(c, &mut child, depth + 1);
                if index + 1 == p.seq.len() {
                    scope.merge_optional(&child);
                }
            }
            scope.files_changed = true;
        }
    }
    fn command(&mut self, c: &ast::Command, scope: &mut Scope, depth: usize) {
        if depth > 64 || self.remaining == 0 {
            scope.dynamic = true;
            return;
        }
        self.remaining -= 1;
        match c {
            ast::Command::Simple(sc) => {
                let mut local = scope.clone();
                let mut parent_dynamic = false;
                for item in command_context::items(sc) {
                    match item {
                        ast::CommandPrefixOrSuffixItem::AssignmentWord(assignment, _) => {
                            let value = match &assignment.value {
                                ast::AssignmentValue::Scalar(word) => self.literal(word),
                                _ => None,
                            };
                            parent_dynamic |= command_context::assignment_value_changes_resolution(
                                &assignment.value,
                            );
                            local.assignment(assignment, value);
                        }
                        ast::CommandPrefixOrSuffixItem::IoRedirect(r) => {
                            local.files_changed |= command_context::writes_files(r);
                            local.correction_blocked |=
                                command_context::redirect_blocks_correction(r);
                            parent_dynamic |= command_context::redirect_changes_resolution(r);
                            local.dynamic |= command_context::redirect_changes_resolution(r);
                        }
                        ast::CommandPrefixOrSuffixItem::ProcessSubstitution(..) => {
                            local.files_changed = true;
                            local.correction_blocked = true;
                        }
                        _ => {}
                    }
                }
                let Some(w) = &sc.word_or_name else {
                    *scope = local;
                    return;
                };
                let Some(name) = self.literal(w) else {
                    scope.dynamic = true;
                    return;
                };
                let mut argv = vec![name.clone()];
                if let Some(suffix) = &sc.suffix {
                    for item in &suffix.0 {
                        if let ast::CommandPrefixOrSuffixItem::Word(w) = item {
                            let value = self.literal(w);
                            local.files_changed |=
                                value.is_none() && command_context::word_may_write(w);
                            parent_dynamic |= command_context::word_changes_resolution(w);
                            local.dynamic |= command_context::word_changes_resolution(w);
                            argv.push(value.unwrap_or_else(|| w.value.clone()));
                        }
                    }
                }
                let resolution = if local.dynamic {
                    None
                } else {
                    Some(self.shell.resolve_scoped(
                        &name,
                        local.path.as_deref(),
                        local.functions.contains(&name),
                    ))
                };
                scope.files_changed |= local.files_changed;
                scope.dynamic |= parent_dynamic;
                if resolution != Some(Resolution::NotFound) {
                    scope.after_command(
                        &name,
                        resolution == Some(Resolution::Builtin),
                        matches!(
                            resolution,
                            Some(Resolution::Alias(_) | Resolution::Function)
                        ),
                        name == "printf" && argv.iter().any(|arg| arg == "-v"),
                    );
                }
                self.commands.push(SimpleCmd {
                    name,
                    span: w.loc.as_ref().map(|l| (l.start.index, l.end.index)),
                    argv,
                    resolution,
                    path: local.path,
                    correction_blocked: local.correction_blocked,
                    runtime_dependent: local.files_changed,
                });
            }
            ast::Command::Compound(cc, redirects) => {
                let writes = redirects
                    .iter()
                    .flat_map(|r| &r.0)
                    .any(command_context::writes_files);
                let blocks = redirects
                    .iter()
                    .flat_map(|r| &r.0)
                    .any(command_context::redirect_blocks_correction);
                scope.files_changed |= writes;
                scope.correction_blocked |= blocks;
                scope.dynamic |= blocks;
                self.compound(cc, scope, depth + 1);
            }
            ast::Command::Function(fd) => {
                if let Some(n) = self.literal(&fd.fname) {
                    scope.functions.insert(n);
                }
            }
            ast::Command::ExtendedTest(test, redirects) => {
                scope.files_changed |= command_context::extended_test_may_write(&test.expr);
                let writes = redirects
                    .iter()
                    .flat_map(|r| &r.0)
                    .any(command_context::writes_files);
                let blocks = redirects
                    .iter()
                    .flat_map(|r| &r.0)
                    .any(command_context::redirect_blocks_correction);
                scope.files_changed |= writes;
                scope.correction_blocked |= blocks;
                scope.dynamic |= blocks;
            }
        }
    }
    fn optional(&mut self, list: &ast::CompoundList, scope: &mut Scope, depth: usize) {
        let mut child = scope.clone();
        self.list(list, &mut child, depth);
        scope.merge_optional(&child);
    }

    fn compound(&mut self, cc: &ast::CompoundCommand, scope: &mut Scope, depth: usize) {
        use ast::CompoundCommand as C;
        match cc {
            C::BraceGroup(b) => self.list(&b.list, scope, depth),
            C::Subshell(s) => {
                let mut child = scope.clone();
                self.list(&s.list, &mut child, depth);
                scope.files_changed |= child.files_changed;
                scope.correction_blocked |= child.correction_blocked;
            }
            C::ForClause(f) => {
                scope.for_loop(f);
                self.optional(&f.body.list, scope, depth);
            }
            C::ArithmeticForClause(f) => {
                scope.dynamic = true;
                self.optional(&f.body.list, scope, depth);
            }
            C::CaseClause(c) => {
                for item in &c.cases {
                    if let Some(cmd) = &item.cmd {
                        self.optional(cmd, scope, depth);
                    }
                }
            }
            C::IfClause(i) => {
                self.list(&i.condition, scope, depth);
                self.optional(&i.then, scope, depth);
                for e in i.elses.iter().flatten() {
                    if let Some(c) = &e.condition {
                        self.optional(c, scope, depth);
                    }
                    self.optional(&e.body, scope, depth);
                }
            }
            C::WhileClause(w) | C::UntilClause(w) => {
                self.list(&w.0, scope, depth);
                self.optional(&w.1.list, scope, depth);
            }
            C::Arithmetic(_) => scope.dynamic = true,
            C::Coprocess(c) => {
                self.command(&c.body, &mut scope.clone(), depth);
                scope.files_changed = true;
                scope.correction_blocked = true;
            }
        }
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

/// Syntax and best-effort static checks; dynamic shell behavior is not executed.
pub fn is_suggestion_program(text: &str, shell: &EmbeddedShell) -> bool {
    crate::suggestion::validate(text, shell)
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
    match inline_commands::parse(line, &cfg.ai_prefix, cfg.ai_enabled) {
        inline_commands::Input::Shell => {}
        inline_commands::Input::Task(text) => {
            return Action::Ai {
                trigger: Trigger::Hash,
                text: text.to_owned(),
            };
        }
        inline_commands::Input::Command(command) => return Action::Inline(command),
        inline_commands::Input::Error(error) => return Action::InvalidInline(error),
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
    let mut collector = Collector {
        shell,
        options: shell.parser_options(),
        commands: Vec::new(),
        remaining: 16_384,
    };
    let mut scope = Scope {
        dynamic: shell.has_command_traps(),
        ..Scope::default()
    };
    for cl in &prog.complete_commands {
        collector.list(cl, &mut scope, 0);
    }
    let cmds = collector.commands;
    let missing: Vec<&SimpleCmd> = cmds
        .iter()
        .filter(|c| c.resolution == Some(Resolution::NotFound))
        .collect();
    if !missing.is_empty() {
        let names = shell.command_names();
        let mut edits = Vec::new();
        let mut first = None;
        for m in &missing {
            if m.correction_blocked {
                edits.clear();
                break;
            }
            if looks_like_question(&m.argv) {
                edits.clear();
                break;
            }
            let fix = spell::ranked_matches(&m.name, &names)
                .into_iter()
                .find(|c| shell.resolve_with_path(c, m.path.as_deref()) != Resolution::NotFound);
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
        return if cfg.trigger_on_error
            && (!missing.iter().any(|m| m.runtime_dependent)
                || missing.iter().any(|m| looks_like_question(&m.argv)))
        {
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
    #[cfg(unix)]
    use crate::backend::ShellOptions;
    #[cfg(unix)]
    use std::fs;

    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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

    #[cfg(unix)]
    #[test]
    fn miss_after_filesystem_effect_is_not_routed_to_ai() {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let touch = bin.join("touch");
        fs::write(&touch, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&touch, fs::Permissions::from_mode(0o700)).unwrap();
        let git = bin.join("git");
        fs::write(&git, "#!/bin/sh\nexit 0\n").unwrap();
        fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
        let mut shell = EmbeddedShell::new(ShellOptions {
            working_dir: Some(root.path().to_owned()),
            ..ShellOptions::default()
        })
        .unwrap();
        shell.run_user_line(&format!("PATH='{}'", bin.display()));

        for line in [
            "touch custom_command | custom_command",
            "[[ $(touch custom_command) ]]; custom_command",
        ] {
            assert_eq!(
                classify(line, &mut shell, &TriggerConfig::default()),
                Action::Execute,
                "{line}"
            );
        }

        assert!(matches!(
            classify("X=$Y; gti status", &mut shell, &TriggerConfig::default()),
            Action::Correct { from, to, .. } if from == "gti" && to == "git"
        ));
        assert!(matches!(
            classify(
                "echo '${PATH:=/tmp}'; gti status",
                &mut shell,
                &TriggerConfig::default()
            ),
            Action::Correct { from, to, .. } if from == "gti" && to == "git"
        ));
        assert_eq!(
            classify(
                "true <<< \"${PATH:=/tmp}\"; gti status",
                &mut shell,
                &TriggerConfig::default()
            ),
            Action::Execute
        );
    }
}
