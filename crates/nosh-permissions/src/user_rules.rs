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
    pub max_depth: Option<usize>,
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
}

impl PathPattern {
    fn new(text: &str) -> Result<Self, String> {
        if text.is_empty() {
            return Err("path patterns must not be empty".into());
        }
        let pattern = text
            .strip_prefix("~/")
            .unwrap_or(text)
            .trim_start_matches("./");
        let pattern = if pattern == "." { "" } else { pattern };
        if Path::new(pattern)
            .components()
            .any(|part| part == std::path::Component::ParentDir)
        {
            return Err("path patterns cannot traverse '..'; use an absolute or ~/ pattern".into());
        }
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
        })
    }

    fn matches(&self, path: &Path, ctx: &Context) -> bool {
        let base = if self.text.starts_with("~/") {
            self.home.as_deref()
        } else if Path::new(&self.text).is_absolute() {
            return self.glob.is_match(path);
        } else {
            Some(ctx.workspace.as_path())
        };
        base.and_then(|base| path.strip_prefix(base).ok())
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
pub fn command_words(text: &str) -> Result<Vec<String>, String> {
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

fn comparable_argv(argv: &[String]) -> Vec<&str> {
    let mut words: Vec<_> = argv.iter().map(String::as_str).collect();
    // This harness-owned rewrite changes prompting, not the granted privilege.
    if words.first() == Some(&"sudo")
        && let Some(index) = words
            .iter()
            .skip(1)
            .take_while(|w| w.starts_with('-'))
            .position(|w| matches!(*w, "-n" | "--non-interactive"))
    {
        words.remove(index + 1);
    }
    words
}

impl UserRule {
    pub fn compile(spec: RuleSpec, source: impl Into<String>) -> Result<Self, String> {
        let source = source.into();
        let fail = |why: &str| format!("{source}: {why}");
        let tool = spec.tool.as_deref().unwrap_or("run_command");
        if !matches!(tool, "run_command" | "read_file" | "list_dir") {
            return Err(fail("tool must be run_command, read_file or list_dir"));
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
        if spec.max_depth.is_some() && tool != "list_dir" {
            return Err(fail("max_depth applies only to list_dir"));
        }
        if spec.max_depth.is_some_and(|d| !(1..=3).contains(&d)) {
            return Err(fail("max_depth must be between 1 and 3"));
        }
        if tool != "run_command"
            && (!spec.variables.is_empty() || !spec.hosts.is_empty() || spec.allow_opaque)
        {
            return Err(fail(
                "variables, hosts and allow_opaque apply only to run_command",
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
        let path = spec.path.as_deref().map(PathPattern::new).transpose()?;
        let compile_paths = |patterns: &[String]| {
            patterns
                .iter()
                .map(|p| PathPattern::new(p))
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
        if let Some((words, prefix)) = &self.argv {
            let expected = comparable_argv(words);
            let actual = comparable_argv(&op.argv);
            if deny && let Some(unknown) = op.known.iter().position(|known| !known) {
                let common = unknown.min(expected.len()).min(actual.len());
                if actual[..common] != expected[..common] || !prefix && unknown > expected.len() {
                    return false;
                }
            } else {
                if !actual.starts_with(&expected) || !prefix && actual.len() != expected.len() {
                    return false;
                }
                if op
                    .known
                    .iter()
                    .take(if *prefix { words.len() } else { op.known.len() })
                    .any(|k| !k)
                {
                    return false;
                }
            }
        }
        if let Some(max) = self.spec.max_depth
            && op
                .paths
                .iter()
                .any(|p| matches!(p.kind, AccessKind::List { depth } if depth > max))
        {
            return false;
        }
        true
    }

    pub(crate) fn denies(&self, op: &Operation, ctx: &Context) -> bool {
        if !self.selector_matches(op, ctx, true) {
            return false;
        }
        let matches_any = |patterns: &[PathPattern], kinds: fn(AccessKind) -> bool| {
            patterns.is_empty()
                || op.opaque
                || op
                    .paths
                    .iter()
                    .filter(|p| kinds(p.kind))
                    .any(|p| patterns.iter().any(|pattern| pattern.intersects(p, ctx)))
        };
        if let Some(pattern) = &self.path
            && !op.paths.iter().any(|p| pattern.intersects(p, ctx))
        {
            return false;
        }
        matches_any(&self.reads, |k| {
            matches!(k, AccessKind::Read | AccessKind::List { .. })
        }) && matches_any(&self.writes, |k| {
            matches!(k, AccessKind::Write | AccessKind::Delete)
        }) && (self.spec.variables.is_empty()
            || op.variables.iter().any(|(key, _)| {
                self.spec.variables.contains(key) || ctx.unknown_variables.contains(key)
            }))
            && (self.hosts.is_empty()
                || op.network && op.hosts.is_empty()
                || op
                    .hosts
                    .iter()
                    .any(|host| self.hosts.iter().any(|g| g.is_match(host))))
    }

    pub(crate) fn allows(&self, op: &Operation, ctx: &Context) -> bool {
        if !self.selector_matches(op, ctx, false) {
            return false;
        }
        self.effect_scope(op, ctx, false)
    }

    pub(crate) fn allows_payload(&self, op: &Operation, ctx: &Context) -> bool {
        op.tool == self.spec.tool.as_deref().unwrap_or("run_command")
            && self.effect_scope(op, ctx, true)
    }

    pub(crate) fn allows_incomplete(&self, op: &Operation) -> bool {
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
}
