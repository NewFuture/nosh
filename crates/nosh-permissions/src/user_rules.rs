use std::io::BufReader;
use std::path::Path;

use brush_parser::ast;
use brush_parser::word::{self, WordPiece};
use globset::{GlobBuilder, GlobMatcher};
use serde::Deserialize;

use crate::{AccessKind, Context, Operation, PathAccess};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuleSpec {
    pub name: Option<String>,
    pub reason: Option<String>,
    pub tool: Option<String>,
    pub command_prefix: Option<String>,
    pub command_exact: Option<String>,
    pub cwd: Option<String>,
    pub path: Option<String>,
    #[serde(default)]
    pub read_paths: Vec<String>,
    #[serde(default)]
    pub write_paths: Vec<String>,
    #[serde(default)]
    pub variables: Vec<String>,
    #[serde(default)]
    pub hosts: Vec<String>,
    /// Explicitly trust the implementation of a matching invocation, not
    /// additional shell commands or effects outside its declared scopes.
    #[serde(default)]
    pub allow_opaque: bool,
}

#[derive(Debug, Clone)]
struct PathPattern {
    text: String,
    glob: GlobMatcher,
    home: Option<std::path::PathBuf>,
    absolute_root: Option<std::path::PathBuf>,
}

impl PathPattern {
    fn new(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("path patterns must not be empty".into());
        }
        let absolute = Path::new(text).is_absolute();
        let mut pattern = text
            .strip_prefix("~/")
            .map_or(text, |tail| tail.trim_start_matches('/'));
        while let Some(tail) = pattern.strip_prefix("./") {
            pattern = tail.trim_start_matches('/');
        }
        let pattern = if pattern == "." { "" } else { pattern };
        if Path::new(pattern)
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        {
            return Err("path patterns cannot traverse '..'; use an absolute or ~/ pattern".into());
        }
        let (absolute_root, pattern) = if absolute {
            let cut = pattern.find(['*', '?', '[', '{']);
            let root_end = cut.map_or(pattern.len(), |cut| {
                pattern[..cut].rfind('/').unwrap_or(0) + 1
            });
            (
                Some(std::path::PathBuf::from(&pattern[..root_end])),
                &pattern[root_end..],
            )
        } else {
            (None, pattern)
        };
        let glob = GlobBuilder::new(pattern)
            .literal_separator(true)
            .backslash_escape(true)
            .build()
            .map_err(|e| format!("invalid path pattern '{text}': {e}"))?
            .compile_matcher();
        let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
        if text.starts_with("~/") && home.is_none() {
            return Err("cannot expand a rule's home path without HOME".into());
        }
        Ok(Self {
            text: text.into(),
            glob,
            home,
            absolute_root,
        })
    }

    fn matches(&self, path: &Path, ctx: &Context) -> bool {
        let base = if self.text.starts_with("~/") {
            self.home.as_deref()
        } else if let Some(root) = &self.absolute_root {
            Some(root.as_path())
        } else {
            Some(ctx.workspace.as_path())
        };
        base.and_then(|base| crate::paths::relative_path(path, base))
            .is_some_and(|relative| self.glob.is_match(relative))
    }

    fn covers(&self, access: &PathAccess, ctx: &Context) -> bool {
        access
            .resolved
            .as_ref()
            .is_some_and(|path| self.matches(path, ctx))
    }

    fn intersects(&self, access: &PathAccess, ctx: &Context) -> bool {
        if access.resolved.is_none() {
            return true;
        }
        if access
            .lexical
            .iter()
            .chain(access.resolved.iter())
            .any(|path| self.matches(path, ctx))
        {
            return true;
        }
        let AccessKind::List { depth } = access.kind else {
            return false;
        };
        let cut = self
            .text
            .find(['*', '?', '[', '{'])
            .unwrap_or(self.text.len());
        let prefix = if cut == self.text.len() {
            &self.text[..cut]
        } else {
            self.text[..cut]
                .rfind('/')
                .map(|i| &self.text[..=i])
                .unwrap_or("")
        };
        let base = crate::paths::resolve(prefix, &ctx.workspace, self.home.as_deref());
        let base = crate::real_path(&base, true).unwrap_or(base);
        access.resolved.as_ref().is_some_and(|root| {
            root.starts_with(&base)
                || base
                    .strip_prefix(root)
                    .is_ok_and(|tail| tail.components().count() <= depth)
        })
    }
}

#[derive(Debug, Clone)]
pub struct UserRule {
    pub spec: RuleSpec,
    pub source: String,
    argv: Option<(Vec<String>, bool)>,
    path: Option<PathPattern>,
    reads: Vec<PathPattern>,
    writes: Vec<PathPattern>,
    hosts: Vec<GlobMatcher>,
    home: Option<std::path::PathBuf>,
}

/// Compile literal shell words without evaluating any shell expression.
fn command_words(text: &str) -> Result<Vec<String>, String> {
    let options = brush_parser::ParserOptions::default();
    let mut parser = brush_parser::Parser::new(BufReader::new(text.as_bytes()), &options);
    let program = parser
        .parse_program()
        .map_err(|e| format!("invalid command: {e}"))?;
    let invalid = || "expected one command containing only literal shell words".to_string();
    let [list] = program.complete_commands.as_slice() else {
        return Err(invalid());
    };
    let [ast::CompoundListItem(chain, ast::SeparatorOperator::Sequence)] = list.0.as_slice() else {
        return Err(invalid());
    };
    if !chain.additional.is_empty() || chain.first.seq.len() != 1 {
        return Err(invalid());
    }
    let ast::Command::Simple(command) = &chain.first.seq[0] else {
        return Err(invalid());
    };
    if command.prefix.is_some() {
        return Err(invalid());
    }
    let mut words = vec![command.word_or_name.as_ref().ok_or_else(invalid)?];
    if let Some(suffix) = &command.suffix {
        for item in &suffix.0 {
            let ast::CommandPrefixOrSuffixItem::Word(word) = item else {
                return Err(invalid());
            };
            words.push(word);
        }
    }
    fn literal(pieces: &[word::WordPieceWithSource], out: &mut String, quoted: bool) -> bool {
        for piece in pieces {
            match &piece.piece {
                WordPiece::Text(s) if quoted || !s.contains(['*', '?', '[', '{', '}']) => {
                    out.push_str(s)
                }
                WordPiece::SingleQuotedText(s) => out.push_str(s),
                WordPiece::DoubleQuotedSequence(inner) => {
                    if !literal(inner, out, true) {
                        return false;
                    }
                }
                WordPiece::EscapeSequence(s) => out.push_str(s.strip_prefix('\\').unwrap_or(s)),
                _ => return false,
            }
        }
        true
    }
    words.into_iter().map(|word| {
        let pieces = word::parse(&word.value, &options).map_err(|e| e.to_string())?;
        let mut value = String::new();
        if !literal(&pieces, &mut value, false) {
            return Err("command selectors cannot contain expansions or globs; use command_prefix for trailing arguments".into());
        }
        Ok(value)
    }).collect()
}

fn comparable_argv(argv: &[String]) -> impl Iterator<Item = (usize, &str)> {
    // Only the leading option is the harness-owned rewrite. A later "-n"
    // can instead be a value, e.g. the argument of sudo's -u option.
    let rewritten = argv.first().is_some_and(|word| word == "sudo");
    argv.iter().enumerate().filter_map(move |(i, word)| {
        (!(rewritten && i == 1 && matches!(word.as_str(), "-n" | "--non-interactive")))
            .then_some((i, word.as_str()))
    })
}

fn command_matches(words: &[String], prefix: bool, op: &Operation, deny: bool) -> bool {
    let mut actual = comparable_argv(&op.argv);
    for (_, expected) in comparable_argv(words) {
        let Some((index, word)) = actual.next() else {
            return false;
        };
        if op.known.get(index) != Some(&true) {
            return deny;
        }
        if word != expected {
            return false;
        }
    }
    prefix
        || actual.all(|(index, _)| {
            deny && op.known.get(index) == Some(&false)
                && op.may_disappear.get(index) == Some(&true)
        })
}

impl UserRule {
    pub fn compile(spec: RuleSpec, source: impl Into<String>) -> Result<Self, String> {
        let source = source.into();
        let fail = |why: &str| format!("{source}: {why}");
        let tool = spec.tool.as_deref().unwrap_or("run_command");
        if !matches!(tool, "run_command" | "read_file" | "grep") {
            return Err(fail("tool must be run_command, read_file or grep"));
        }
        if spec.command_prefix.is_some() && spec.command_exact.is_some() {
            return Err(fail(
                "command_prefix and command_exact are mutually exclusive",
            ));
        }
        let selector = spec.command_prefix.as_ref().or(spec.command_exact.as_ref());
        if selector.is_some() && tool != "run_command" {
            return Err(fail("command selectors require run_command"));
        }
        if tool == "run_command" && spec.path.is_some() {
            return Err(fail(
                "use read_paths/write_paths for command effects, not path",
            ));
        }
        if tool != "run_command"
            && (!spec.variables.is_empty()
                || !spec.hosts.is_empty()
                || spec.allow_opaque
                || !spec.read_paths.is_empty()
                || !spec.write_paths.is_empty())
        {
            return Err(fail(
                "read_paths, write_paths, variables, hosts and allow_opaque apply only to run_command; use path for read tools",
            ));
        }
        if selector.is_none() && spec.tool.is_none() {
            return Err(fail("specify a command selector or an explicit tool"));
        }
        if spec.cwd.as_deref().is_some_and(str::is_empty) {
            return Err(fail("cwd must not be empty"));
        }
        if spec
            .cwd
            .as_deref()
            .is_some_and(|cwd| cwd == "~" || cwd.starts_with("~/"))
            && std::env::var_os("HOME").is_none()
        {
            return Err(fail("cannot expand cwd without HOME"));
        }
        let argv = selector
            .map(|text| command_words(text).map(|words| (words, spec.command_prefix.is_some())))
            .transpose()
            .map_err(|e| format!("{source}: {e}"))?;
        let path = spec
            .path
            .as_deref()
            .map(PathPattern::new)
            .transpose()
            .map_err(|error| fail(&error))?;
        let compile_paths = |patterns: &[String]| {
            patterns
                .iter()
                .map(|p| PathPattern::new(p).map_err(|error| fail(&error)))
                .collect::<Result<Vec<_>, _>>()
        };
        let reads = compile_paths(&spec.read_paths)?;
        let writes = compile_paths(&spec.write_paths)?;
        let hosts = spec
            .hosts
            .iter()
            .map(|host| {
                GlobBuilder::new(host)
                    .case_insensitive(true)
                    .build()
                    .map(|g| g.compile_matcher())
                    .map_err(|e| format!("{source}: invalid host pattern: {e}"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self {
            spec,
            source,
            argv,
            path,
            reads,
            writes,
            hosts,
            home: std::env::var_os("HOME").map(std::path::PathBuf::from),
        })
    }

    pub fn prefix(command: &str) -> Result<Self, String> {
        Self::compile(
            RuleSpec {
                command_prefix: Some(command.into()),
                ..RuleSpec::default()
            },
            command,
        )
    }

    pub fn exact(command: &str) -> Result<Self, String> {
        Self::compile(
            RuleSpec {
                command_exact: Some(command.into()),
                ..RuleSpec::default()
            },
            command,
        )
    }

    pub fn explanation(&self) -> String {
        let label = self.spec.name.as_deref().unwrap_or(&self.source);
        match &self.spec.reason {
            Some(reason) => format!("{label}: {reason}"),
            None => label.to_string(),
        }
    }

    fn selector_matches(&self, op: &Operation, ctx: &Context, deny: bool) -> bool {
        if op.tool != self.spec.tool.as_deref().unwrap_or("run_command") {
            return false;
        }
        if let Some(cwd) = &self.spec.cwd {
            if !op.cwd_known && !deny {
                return false;
            }
            let root = crate::paths::resolve(cwd, &ctx.workspace, self.home.as_deref());
            let root = crate::real_path(&root, true).unwrap_or(root);
            let actual = crate::real_path(&op.cwd, true).unwrap_or_else(|| op.cwd.clone());
            if op.cwd_known && !actual.starts_with(root) {
                return false;
            }
        }
        if let Some((words, prefix)) = &self.argv
            && !command_matches(words, *prefix, op, deny)
        {
            return false;
        }
        true
    }

    pub(crate) fn denies_operation(&self, report: &crate::RiskReport, index: usize) -> bool {
        let op = &report.operations[index];
        let ctx = &report.context;
        if !self.selector_matches(op, ctx, true) {
            return false;
        }
        // A selector binds the whole script/wrapper invocation, including its
        // descendant effects, but never a sibling command in the shell program.
        let effects: Vec<_> = report
            .operations
            .iter()
            .enumerate()
            .skip(index)
            .filter(|(candidate, _)| {
                let mut current = *candidate;
                while current > index {
                    let Some(parent) = report.operations[current]
                        .parent
                        .filter(|parent| *parent < current)
                    else {
                        return false;
                    };
                    current = parent;
                }
                current == index
            })
            .map(|(_, op)| op)
            .collect();
        let matches_any = |patterns: &[PathPattern], kinds: fn(AccessKind) -> bool| {
            patterns.is_empty()
                || effects.iter().any(|op| op.opaque)
                || effects
                    .iter()
                    .flat_map(|op| &op.paths)
                    .filter(|p| kinds(p.kind))
                    .any(|p| patterns.iter().any(|pattern| pattern.intersects(p, ctx)))
        };
        if let Some(pattern) = &self.path
            && !effects
                .iter()
                .flat_map(|op| &op.paths)
                .any(|p| pattern.intersects(p, ctx))
        {
            return false;
        }
        matches_any(&self.reads, |k| {
            matches!(k, AccessKind::Read | AccessKind::List { .. })
        }) && matches_any(&self.writes, |k| {
            matches!(k, AccessKind::Write | AccessKind::Delete)
        }) && (self.spec.variables.is_empty()
            || effects.iter().flat_map(|op| &op.variables).any(|(key, _)| {
                self.spec.variables.contains(key) || ctx.unknown_variables.contains(key)
            }))
            && (self.hosts.is_empty()
                || effects
                    .iter()
                    .any(|op| op.opaque || op.network && op.hosts.is_empty())
                || effects
                    .iter()
                    .flat_map(|op| &op.hosts)
                    .any(|host| self.hosts.iter().any(|g| g.is_match(host))))
    }

    fn allows(&self, op: &Operation, ctx: &Context) -> bool {
        if !self.selector_matches(op, ctx, false) {
            return false;
        }
        self.effect_scope(op, ctx, false)
    }

    pub(crate) fn covers_operation(&self, report: &crate::RiskReport, index: usize) -> bool {
        let op = &report.operations[index];
        if (!report.incomplete || self.allows_incomplete(op)) && self.allows(op, &report.context) {
            return true;
        }
        let mut current = index;
        while let Some(parent) = report.operations[current]
            .parent
            .filter(|parent| *parent < current)
        {
            let ancestor = &report.operations[parent];
            if ancestor.payload
                && (!report.incomplete || self.allows_incomplete(ancestor))
                && self.allows(ancestor, &report.context)
                && self.allows_payload(op, &report.context)
            {
                return true;
            }
            current = parent;
        }
        false
    }

    fn allows_payload(&self, op: &Operation, ctx: &Context) -> bool {
        op.tool == self.spec.tool.as_deref().unwrap_or("run_command")
            && self.effect_scope(op, ctx, true)
    }

    fn allows_incomplete(&self, op: &Operation) -> bool {
        self.spec.allow_opaque
            || (op.payload || self.argv.is_none())
                && self.path.is_none()
                && self.reads.is_empty()
                && self.writes.is_empty()
                && self.hosts.is_empty()
                && self.spec.variables.is_empty()
    }

    fn effect_scope(&self, op: &Operation, ctx: &Context, payload: bool) -> bool {
        if op.opaque
            && (!self.reads.is_empty()
                || !self.writes.is_empty()
                || !self.hosts.is_empty()
                || !self.spec.variables.is_empty())
        {
            return false;
        }
        if let Some(pattern) = &self.path
            && (op.paths.is_empty() || !op.paths.iter().all(|p| pattern.covers(p, ctx)))
        {
            return false;
        }
        for access in &op.paths {
            let patterns = match access.kind {
                AccessKind::Read | AccessKind::List { .. } => &self.reads,
                AccessKind::Write | AccessKind::Delete => &self.writes,
            };
            if access.extra && patterns.is_empty() && self.argv.is_some() && !payload {
                return false;
            }
            if !patterns.is_empty() && !patterns.iter().any(|p| p.covers(access, ctx)) {
                return false;
            }
        }
        if op.extra_variables && self.spec.variables.is_empty() && self.argv.is_some() && !payload {
            return false;
        }
        if !self.spec.variables.is_empty()
            && !op.variables.iter().all(|(key, _)| {
                self.spec.variables.contains(key) && !ctx.unknown_variables.contains(key)
            })
        {
            return false;
        }
        if !self.hosts.is_empty()
            && (op.hosts.is_empty()
                || !op
                    .hosts
                    .iter()
                    .all(|h| self.hosts.iter().any(|g| g.is_match(h))))
        {
            return false;
        }
        // Scope-only rules must not accidentally approve unrelated commands.
        if !payload
            && self.argv.is_none()
            && op.tool == "run_command"
            && !op.argv.is_empty()
            && (!self.spec.variables.is_empty()
                || !self.reads.is_empty()
                || !self.writes.is_empty())
        {
            return false;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_words_and_explicit_prefixes() {
        assert_eq!(
            command_words("git commit -m 'hello world'").unwrap(),
            ["git", "commit", "-m", "hello world"]
        );
        for bad in [
            "",
            "git **",
            "echo $HOME",
            "echo $(whoami)",
            "git status; pwd",
            "echo > file",
        ] {
            assert!(command_words(bad).is_err(), "{bad}");
        }
        assert_eq!(
            command_words("printf '%s' '*'").unwrap(),
            ["printf", "%s", "*"]
        );
    }

    #[test]
    fn invalid_scopes_report_the_rule_location() {
        for spec in [
            RuleSpec {
                command_prefix: Some("echo".into()),
                write_paths: vec!["../outside".into()],
                ..RuleSpec::default()
            },
            RuleSpec {
                tool: Some("read_file".into()),
                path: Some("[".into()),
                ..RuleSpec::default()
            },
            RuleSpec {
                tool: Some("grep".into()),
                write_paths: vec!["**".into()],
                ..RuleSpec::default()
            },
        ] {
            let error = UserRule::compile(spec, "safety.allow[2]").unwrap_err();
            assert!(error.contains("safety.allow[2]"), "{error}");
        }
    }

    #[test]
    fn path_normalization_preserves_the_original_root_kind() {
        for text in [".//**", "././/**", "~//**", "~/.//**"] {
            let pattern = PathPattern::new(text).unwrap();
            assert!(pattern.absolute_root.is_none(), "{text}");
            assert!(pattern.glob.is_match("file"), "{text}");
        }
        assert!(PathPattern::new("/**").unwrap().absolute_root.is_some());
        for text in [".//../**", "~//../**"] {
            assert!(PathPattern::new(text).is_err(), "{text}");
        }
    }
}
