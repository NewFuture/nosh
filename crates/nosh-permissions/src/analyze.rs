//! AST walk: finds every simple command (and wrapper-inner command) in a
//! command line and applies the per-command rules plus path policy.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;

use brush_parser::ParserOptions;
use brush_parser::ast;
use brush_parser::word::{self, TildeExpr, WordPiece, WordPieceWithSource};

use crate::paths::{PathClass, classify_path_real, is_top_level};
use crate::rules::{self, Arg, Target, Verdict, has_flag, opt_value};
use crate::{AccessKind, Context, Operation, PathAccess, Risk, RiskReport};

const MAX_DEPTH: usize = 8;
/// Largest shell script whose contents are analyzed; bigger ones stay Mutating.
const SCRIPT_BYTES: u64 = 256 * 1024;
/// Script bytes read per command line, nested scripts included.
const SCRIPT_BUDGET: usize = 1024 * 1024;
/// Calls of functions defined in the command line re-analyzed with their
/// arguments, per command line.
const CALL_BUDGET: usize = 1024;
const PROTECTED_READ: &str = "reads protected path";

/// Analyzes an agent-issued command line.
pub fn assess_command(cmd: &str, ctx: &Context) -> RiskReport {
    assess_command_with_lookup(cmd, ctx, &|_, _, _, _| None)
}

/// Resolve external commands in the effective cwd/PATH, optionally using the
/// parent shell's command cache. The callback must not execute the program.
pub type ProgramLookup<'a> = dyn Fn(&str, &Path, Option<&str>, bool) -> Option<PathBuf> + 'a;

pub fn assess_command_with_lookup(
    cmd: &str,
    ctx: &Context,
    lookup: &ProgramLookup<'_>,
) -> RiskReport {
    let mut a = Analyzer {
        ctx,
        lookup,
        report: RiskReport {
            context: ctx.clone(),
            ..RiskReport::default()
        },
        operation: None,
        redirecting: false,
        readonly: ctx
            .readonly_variables
            .iter()
            .map(|name| (name.clone(), ctx.variables.get(name).cloned()))
            .collect(),
        unknown_values: ctx.unknown_variables.clone(),
        depth: 0,
        top: false,
        cwd: ctx.cwd.clone(),
        cwd_unknown: false,
        expanding: HashSet::new(),
        local_funcs: HashMap::new(),
        sudo_inserts: Vec::new(),
        opts: ParserOptions::default(),
        child: false,
        script_budget: SCRIPT_BUDGET,
        bound_vars: HashMap::new(),
        script_path: None,
        vars: ctx
            .variables
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect(),
        exported: ctx.exported.clone(),
        positional: None,
        replaying: false,
        calls_left: CALL_BUDGET,
    };
    for (name, value) in &ctx.execution_variables {
        match value {
            Some(value) => {
                a.vars.insert(name.clone(), value.clone());
            }
            None => {
                a.vars.remove(name);
                a.unknown_values.insert(name.clone());
            }
        }
        a.exported.insert(name.clone());
    }
    if cmd.trim().is_empty() {
        a.report.add(Risk::Safe, "empty command");
        return a.report;
    }
    let compact: String = cmd.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.contains(":(){:|:&};:") || compact.contains("(){$0|$0&};") {
        a.report.add(Risk::Forbidden, "fork bomb");
    }
    if cmd.chars().any(hidden_char) {
        a.report.add(
            Risk::Dangerous,
            "contains control or invisible characters (what is shown may differ from what runs)",
        );
    }
    a.program_text(cmd, true);
    if cmd.chars().any(hidden_char) {
        for op in &mut a.report.operations {
            op.risk = op.risk.max(Risk::Dangerous);
        }
    }
    if !a.sudo_inserts.is_empty() {
        a.report.rewritten = Some(insert_sudo_n(cmd, &a.sudo_inserts));
    }
    a.report
}

/// Control characters (except newline and tab), bidirectional overrides and
/// zero-width characters: they make a terminal show something else.
pub(crate) fn hidden_char(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t')
        || matches!(
            c as u32,
            0x061c | 0x200b..=0x200f | 0x202a..=0x202e | 0x2066..=0x2069 | 0xfeff
        )
}

fn insert_sudo_n(cmd: &str, char_positions: &[usize]) -> String {
    let mut byte_pos: Vec<usize> = char_positions
        .iter()
        .filter_map(|&c| {
            if c == cmd.chars().count() {
                Some(cmd.len())
            } else {
                cmd.char_indices().nth(c).map(|(b, _)| b)
            }
        })
        .collect();
    byte_pos.sort_unstable();
    byte_pos.dedup();
    let mut out = cmd.to_string();
    for p in byte_pos.into_iter().rev() {
        out.insert_str(p, " -n");
    }
    out
}

struct Analyzer<'a> {
    ctx: &'a Context,
    lookup: &'a ProgramLookup<'a>,
    report: RiskReport,
    operation: Option<usize>,
    redirecting: bool,
    readonly: HashMap<String, Option<String>>,
    unknown_values: HashSet<String>,
    depth: usize,
    /// Positions in the current text refer to the original command line.
    top: bool,
    cwd: PathBuf,
    cwd_unknown: bool,
    expanding: HashSet<String>,
    /// Functions defined in the analyzed text: name → body.
    local_funcs: HashMap<String, Rc<ast::FunctionBody>>,
    sudo_inserts: Vec<usize>,
    opts: ParserOptions,
    /// Runs in a child shell (a script, `bash -c`): `exit`/`exec` end that
    /// process, not the user's session.
    child: bool,
    /// Script bytes that may still be read for analysis.
    script_budget: usize,
    /// Loop variables that only hold workspace paths, with the cwd they are
    /// relative to (`for f in *.txt`).
    bound_vars: HashMap<String, PathBuf>,
    /// `$0` while analyzing a script: the path it was run by.
    script_path: Option<String>,
    /// Known session/execute-environment values and sequential assignments.
    /// Nonlinear control flow prevents trusting them as exact arguments.
    vars: HashMap<String, String>,
    /// Variables a child process (script, `bash -c`) inherits.
    exported: HashSet<String>,
    /// `$1`… inside a function or script called with known arguments.
    positional: Option<Vec<String>>,
    /// Re-analyzing a function body at a call (see `replay`).
    replaying: bool,
    /// Function calls that may still be re-analyzed.
    calls_left: usize,
}

const INTERPRETERS: &[&str] = &[
    "sh",
    "bash",
    "zsh",
    "dash",
    "ksh",
    "ash",
    "mksh",
    "fish",
    "csh",
    "tcsh",
    "python",
    "python2",
    "python3",
    "perl",
    "ruby",
    "node",
    "php",
    "lua",
    "pwsh",
    "Rscript",
    "osascript",
    "deno",
    "bun",
];
const SHELLS: &[&str] = &[
    "sh", "bash", "zsh", "dash", "ksh", "ash", "mksh", "fish", "csh", "tcsh",
];
/// Shells whose scripts brush-parser can read (bash/POSIX syntax).
const SH_SYNTAX: &[&str] = &[
    "sh", "bash", "dash", "ksh", "ash", "mksh", "zsh", "posh", "yash",
];

fn basename(name: &str) -> &str {
    name.rsplit('/').next().unwrap_or(name)
}

/// Directories whose programs are taken to be the ones the rule table knows
/// by name; scripts there are not analyzed.
const SYSTEM_BIN_DIRS: &[&str] = &[
    "/bin",
    "/sbin",
    "/usr/bin",
    "/usr/sbin",
    "/usr/local/bin",
    "/usr/local/sbin",
    "/usr/libexec",
    "/opt/homebrew/bin",
    "/opt/homebrew/sbin",
    "/home/linuxbrew/.linuxbrew/bin",
    "/snap/bin",
    "/run/current-system/sw/bin",
];

pub(crate) fn in_system_bin_dir(name: &str) -> bool {
    name.rsplit_once('/')
        .is_some_and(|(dir, _)| SYSTEM_BIN_DIRS.contains(&dir))
}

/// The text of a readable shell script at `path`, if small enough to
/// analyze. Run directly (`./x`), a file counts as shell when its `#!` names
/// a POSIX-family shell or when it has none (the shell then runs it itself);
/// run by a shell (`bash x`), any text file does.
fn read_script(path: &std::path::Path, by_shell: bool, budget: usize) -> Option<String> {
    use std::io::Read;
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > SCRIPT_BYTES || meta.len() as usize > budget {
        return None;
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .ok()?
        .take(SCRIPT_BYTES + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    if bytes.len() as u64 > SCRIPT_BYTES || bytes.len() > budget || bytes.contains(&0) {
        return None;
    }
    let text = String::from_utf8(bytes).ok()?;
    if !by_shell && let Some(line) = text.lines().next().and_then(|l| l.strip_prefix("#!")) {
        let mut words = line.split_whitespace();
        let mut interp = basename(words.next()?);
        if interp == "env" {
            interp = basename(words.find(|w| !w.starts_with('-') && !w.contains('='))?);
        }
        if !SH_SYNTAX.contains(&interp) {
            return None;
        }
    }
    Some(text)
}

fn has_escape_obfuscation(src: &str) -> bool {
    let b = src.as_bytes();
    b.windows(2)
        .any(|w| w[0] == b'\\' && matches!(w[1], b'x' | b'u' | b'U' | b'0'..=b'7' | b'c'))
}

fn decode_ansi_c(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars().peekable();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('e') | Some('E') => out.push('\x1b'),
            Some('a') => out.push('\x07'),
            Some('b') => out.push('\x08'),
            Some('f') => out.push('\x0c'),
            Some('v') => out.push('\x0b'),
            Some('x') => {
                let mut h = String::new();
                while h.len() < 2 && it.peek().is_some_and(|c| c.is_ascii_hexdigit()) {
                    h.push(it.next().unwrap());
                }
                if let Some(ch) = u32::from_str_radix(&h, 16).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(d @ '0'..='7') => {
                let mut o = String::from(d);
                while o.len() < 3 && it.peek().is_some_and(|c| ('0'..='7').contains(c)) {
                    o.push(it.next().unwrap());
                }
                if let Some(ch) = u32::from_str_radix(&o, 8).ok().and_then(char::from_u32) {
                    out.push(ch);
                }
            }
            Some(other) => out.push(other),
            None => out.push('\\'),
        }
    }
    out
}

fn param_name(src: &str) -> Option<&str> {
    let s = src.strip_prefix('$')?;
    let s = s.strip_prefix('{').unwrap_or(s);
    let first = s.chars().next()?;
    if matches!(first, '$' | '?' | '#' | '!' | '@' | '*' | '-' | '0'..='9') {
        return Some(&s[..first.len_utf8()]);
    }
    let end = s
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    if end == 0 { None } else { Some(&s[..end]) }
}

/// The variable of `$v` or `${v}` (a plain expansion of a named variable).
fn plain_expansion(src: &str) -> Option<&str> {
    bound_expansion(src).filter(|n| src == format!("${n}") || src == format!("${{{n}}}"))
}

/// The index of `$1`…`$9` or `${N}`.
fn positional_index(src: &str) -> Option<usize> {
    let s = src.strip_prefix('$')?;
    let digits = match s.strip_prefix('{') {
        Some(r) => r.strip_suffix('}')?,
        None if s.len() == 1 => s,
        None => return None,
    };
    digits
        .bytes()
        .all(|b| b.is_ascii_digit())
        .then(|| digits.parse::<usize>().ok())
        .flatten()
        .filter(|&i| i > 0)
}

/// Argument values for `$1`…: up to the first one that is unknown or may
/// expand to several words (or none), which shifts the ones after it.
fn known_values(args: &[Arg]) -> Vec<String> {
    args.iter()
        .map_while(|a| {
            let v = a.resolved()?;
            (!a.glob && !has_brace_expansion(&a.value)).then(|| v.to_string())
        })
        .collect()
}

/// `{a,b}` or `{1..3}` (not `${…}`): brace expansion makes several words.
fn has_brace_expansion(s: &str) -> bool {
    s.char_indices().any(|(i, c)| {
        c == '{'
            && !s[..i].ends_with('$')
            && s[i..].contains('}')
            && (s[i..].contains(',') || s[i..].contains(".."))
    })
}

/// Appends literal text (the same at runtime) to `arg`.
fn push_lit(arg: &mut Arg, s: &str) {
    if !s.is_empty() {
        arg.may_disappear = false;
    }
    arg.value.push_str(s);
    if let Some(k) = &mut arg.known {
        k.push_str(s);
    }
}

/// Appends an expansion (`src`) to `arg`, with its value when known.
fn push_expansion(arg: &mut Arg, src: &str, value: Option<&str>) {
    arg.dynamic = true;
    arg.value.push_str(src);
    match (value, &mut arg.known) {
        (Some(v), Some(k)) => k.push_str(v),
        _ => arg.known = None,
    }
}

/// The variable of `$v`, `${v}`, `${v%suffix}` or `${v%%suffix}`: expansions
/// that yield the value or a prefix of it (so no new `/` or `..`).
fn bound_expansion(src: &str) -> Option<&str> {
    let s = src.strip_prefix('$')?;
    let (inner, braced) = match s.strip_prefix('{') {
        Some(r) => (r.strip_suffix('}')?, true),
        None => (s, false),
    };
    let end = inner
        .char_indices()
        .find(|(_, c)| !(c.is_ascii_alphanumeric() || *c == '_'))
        .map_or(inner.len(), |(i, _)| i);
    if end == 0 || inner.as_bytes()[0].is_ascii_digit() {
        return None;
    }
    let (name, rest) = inner.split_at(end);
    (rest.is_empty() || braced && rest.starts_with('%')).then_some(name)
}

/// Literal text of a runtime-chosen path (`\u{1}` marks the chosen part):
/// it stays below the directory it is relative to.
fn stays_below(lit: &str) -> bool {
    lit.contains('\u{1}')
        && !lit.starts_with('/')
        && !lit.starts_with('~')
        && !lit.split('/').any(|c| c == "..")
}

fn shell_join(args: &[Arg]) -> String {
    args.iter()
        .map(|a| a.value.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

const INLINE_CODE_DANGER: &[&str] = &[
    "os.system",
    "subprocess",
    "rmtree",
    "os.remove",
    "os.unlink",
    "unlink(",
    "unlink ",
    "system(",
    "exec(",
    "popen",
    "child_process",
    "fs.rm",
    "rm -rf",
    "`",
    "shutil.move",
    "os.rename",
    "eval(",
    "Runtime.getRuntime",
    "File.delete",
];

impl Analyzer<'_> {
    fn add(&mut self, risk: Risk, reason: impl Into<String>) {
        self.report.add(risk, reason);
        if let Some(i) = self.operation {
            self.report.operations[i].risk = self.report.operations[i].risk.max(risk);
        }
    }

    fn uncertain(&mut self, risk: Risk, reason: impl Into<String>) {
        self.report.uncertain(risk, reason);
        if let Some(i) = self.operation {
            self.report.operations[i].risk = self.report.operations[i].risk.max(risk);
        }
    }

    fn finish_operation(&mut self) {
        if let Some(i) = self.operation
            && self.report.operations[i].payload
        {
            let opaque = self.report.incomplete
                || self.report.operations[i + 1..].iter().any(|op| op.opaque);
            self.report.operations[i].opaque |= opaque;
        }
    }

    fn begin_operation(&mut self, argv: &[Arg]) -> Option<usize> {
        let previous = self.operation;
        self.operation = Some(self.report.operations.len());
        self.report.operations.push(Operation {
            tool: "run_command".into(),
            parent: if self.redirecting {
                previous.and_then(|i| self.report.operations[i].parent)
            } else {
                previous
            },
            argv: argv
                .iter()
                .map(|a| a.resolved().unwrap_or(&a.value).to_string())
                .collect(),
            known: argv
                .iter()
                .map(|a| {
                    a.resolved().is_some()
                        && !a.glob
                        && (!a.dynamic
                            || (!self.report.incomplete && !self.child && !self.replaying))
                })
                .collect(),
            may_disappear: argv.iter().map(|arg| arg.may_disappear).collect(),
            cwd: self.cwd.clone(),
            cwd_known: !self.cwd_unknown && !self.report.incomplete,
            opaque: (self.child || self.replaying) && argv.iter().any(|arg| arg.dynamic),
            ..Operation::default()
        });
        previous
    }

    fn opaque(&mut self) {
        if let Some(i) = self.operation {
            self.report.operations[i].opaque = true;
        } else {
            self.report.incomplete = true;
        }
    }

    fn variable(&mut self, name: &str, value: Option<String>) {
        if let Some(i) = self.operation {
            self.report.operations[i]
                .variables
                .push((name.into(), value));
        }
    }

    fn path_access(&mut self, target: &Target, kind: AccessKind) {
        let lexical = if target.glob
            || self.cwd_unknown
            || self.report.incomplete && (target.dynamic || !Path::new(&target.path).is_absolute())
        {
            None
        } else {
            match (target.dynamic, target.known.as_deref()) {
                (false, _) => Some(self.resolve(&target.path)),
                (true, Some(value)) => Some(self.resolve(value)),
                _ => None,
            }
        };
        let resolved = lexical.as_ref().map(|_| {
            self.target_path(
                target.known.as_deref().unwrap_or(&target.path),
                kind != AccessKind::Delete,
            )
        });
        let access = PathAccess {
            kind,
            lexical,
            resolved,
            extra: self.redirecting,
        };
        if let Some(i) = self.operation {
            self.report.operations[i].paths.push(access);
        } else {
            self.report.operations.push(Operation {
                tool: "run_command".into(),
                cwd: self.cwd.clone(),
                paths: vec![access],
                ..Operation::default()
            });
        }
    }

    fn parse(&self, text: &str) -> Result<ast::Program, String> {
        let mut p = brush_parser::Parser::new(std::io::BufReader::new(text.as_bytes()), &self.opts);
        p.parse_program().map_err(|e| e.to_string())
    }

    fn program_text(&mut self, text: &str, top: bool) {
        if self.depth >= MAX_DEPTH {
            self.report.incomplete = true;
            self.add(Risk::Mutating, "command nesting too deep to analyze");
            return;
        }
        match self.parse(text) {
            Ok(prog) => {
                let prev = self.top;
                self.top = top;
                self.depth += 1;
                for cl in &prog.complete_commands {
                    self.compound_list(cl);
                }
                self.depth -= 1;
                self.top = prev;
            }
            Err(e) => {
                self.report.incomplete = true;
                if top {
                    self.report.syntax_error = Some(e.clone());
                }
                self.add(Risk::Mutating, format!("could not parse the command ({e})"));
            }
        }
    }

    fn compound_list(&mut self, cl: &ast::CompoundList) {
        for ast::CompoundListItem(aol, sep) in &cl.0 {
            if matches!(sep, ast::SeparatorOperator::Async) {
                self.report.incomplete = true;
                self.add(Risk::Mutating, "starts a background job");
            }
            self.pipeline(&aol.first);
            for next in &aol.additional {
                match next {
                    ast::AndOr::And(p) | ast::AndOr::Or(p) => self.pipeline(p),
                }
            }
        }
    }

    fn pipeline(&mut self, p: &ast::Pipeline) {
        for (i, c) in p.seq.iter().enumerate() {
            if i > 0 && self.is_stdin_interpreter(c) {
                self.add(
                    Risk::Dangerous,
                    "pipes data into an interpreter (e.g. curl … | sh)",
                );
            }
            self.command(c);
        }
    }

    fn static_word(&self, w: &ast::Word) -> Option<String> {
        let pieces = word::parse(&w.value, &self.opts).ok()?;
        let mut s = String::new();
        fn walk(pieces: &[WordPieceWithSource], s: &mut String) -> bool {
            for p in pieces {
                match &p.piece {
                    WordPiece::Text(t) | WordPiece::SingleQuotedText(t) => s.push_str(t),
                    WordPiece::EscapeSequence(t) => s.push_str(t.strip_prefix('\\').unwrap_or(t)),
                    WordPiece::DoubleQuotedSequence(inner) => {
                        if !walk(inner, s) {
                            return false;
                        }
                    }
                    _ => return false,
                }
            }
            true
        }
        walk(&pieces, &mut s).then_some(s)
    }

    fn is_stdin_interpreter(&self, c: &ast::Command) -> bool {
        let ast::Command::Simple(sc) = c else {
            return false;
        };
        let mut words: Vec<String> = Vec::new();
        if let Some(w) = &sc.word_or_name {
            words.push(self.static_word(w).unwrap_or_default());
        }
        if let Some(suffix) = &sc.suffix {
            for item in &suffix.0 {
                if let ast::CommandPrefixOrSuffixItem::Word(w) = item {
                    words.push(self.static_word(w).unwrap_or_else(|| "$dyn".into()));
                }
            }
        }
        let mut i = 0;
        while i < words.len()
            && matches!(
                basename(&words[i]),
                "sudo" | "doas" | "env" | "nohup" | "command" | "exec" | "time" | "nice"
            )
        {
            i += 1;
            while i < words.len() && (words[i].starts_with('-') || words[i].contains('=')) {
                i += 1;
            }
        }
        let Some(name) = words.get(i) else {
            return false;
        };
        let base = basename(name);
        let is_interp =
            INTERPRETERS.contains(&base) || base.starts_with("python3.") || base == "busybox";
        if !is_interp {
            return false;
        }
        let rest = &words[i + 1..];
        if rest.iter().any(|a| {
            a == "-c"
                || a == "-e"
                || a == "-m"
                || a == "--command"
                || (a.starts_with('-')
                    && !a.starts_with("--")
                    && a.contains('c')
                    && SHELLS.contains(&base))
        }) {
            return false;
        }
        match rest.iter().find(|a| !a.starts_with('-')) {
            None => true,
            Some(first) => first == "-" || (base == "busybox" && SHELLS.contains(&first.as_str())),
        }
    }

    fn command(&mut self, c: &ast::Command) {
        match c {
            ast::Command::Simple(sc) => self.simple(sc),
            ast::Command::Compound(cc, redirs) => {
                let previous = self.begin_operation(&[]);
                if let Some(i) = self.operation {
                    self.report.operations[i].transparent = true;
                }
                self.compound(cc);
                if let Some(r) = redirs {
                    for x in &r.0 {
                        self.redirect(x);
                    }
                }
                self.operation = previous;
            }
            ast::Command::Function(fd) => self.function_def(fd),
            ast::Command::ExtendedTest(e, redirs) => {
                self.ext_test(&e.expr);
                if let Some(r) = redirs {
                    for x in &r.0 {
                        self.redirect(x);
                    }
                }
            }
        }
    }

    fn compound(&mut self, cc: &ast::CompoundCommand) {
        use ast::CompoundCommand as C;
        if !matches!(cc, C::BraceGroup(_) | C::Subshell(_)) {
            self.report.incomplete = true;
        }
        match cc {
            C::Arithmetic(a) => {
                self.unbind_in(&a.expr.value);
                self.text_substitutions(&a.expr.value);
            }
            C::ArithmeticForClause(f) => {
                for e in [&f.initializer, &f.condition, &f.updater]
                    .into_iter()
                    .flatten()
                {
                    self.unbind_in(&e.value);
                    self.text_substitutions(&e.value);
                }
                self.compound_list(&f.body.list);
            }
            C::BraceGroup(b) => self.compound_list(&b.list),
            C::Subshell(s) => self.in_child(|a| a.compound_list(&s.list)),
            C::ForClause(f) => {
                let binding = self.loop_binding(f.values.as_deref());
                if let Some(vals) = &f.values {
                    for w in vals {
                        self.word(w);
                    }
                }
                let prev = match binding {
                    Some(cwd) => self.bound_vars.insert(f.variable_name.clone(), cwd),
                    None => self.bound_vars.remove(&f.variable_name),
                };
                self.vars.remove(&f.variable_name);
                self.compound_list(&f.body.list);
                match prev {
                    Some(p) => self.bound_vars.insert(f.variable_name.clone(), p),
                    None => self.bound_vars.remove(&f.variable_name),
                };
            }
            C::CaseClause(c) => {
                self.word(&c.value);
                for item in &c.cases {
                    for p in &item.patterns {
                        self.word(p);
                    }
                    if let Some(cmd) = &item.cmd {
                        self.compound_list(cmd);
                    }
                }
            }
            C::IfClause(i) => {
                self.compound_list(&i.condition);
                self.compound_list(&i.then);
                if let Some(elses) = &i.elses {
                    for e in elses {
                        if let Some(c) = &e.condition {
                            self.compound_list(c);
                        }
                        self.compound_list(&e.body);
                    }
                }
            }
            C::WhileClause(w) | C::UntilClause(w) => {
                self.compound_list(&w.0);
                self.compound_list(&w.1.list);
            }
            C::Coprocess(c) => {
                self.add(Risk::Mutating, "starts a coprocess");
                self.command(&c.body);
            }
        }
    }

    fn ext_test(&mut self, e: &ast::ExtendedTestExpr) {
        use ast::ExtendedTestExpr as E;
        match e {
            E::And(a, b) | E::Or(a, b) => {
                self.ext_test(a);
                self.ext_test(b);
            }
            E::Not(a) | E::Parenthesized(a) => self.ext_test(a),
            E::UnaryTest(_, w) => {
                self.word(w);
            }
            E::BinaryTest(_, a, b) => {
                self.word(a);
                self.word(b);
            }
        }
    }

    fn text_substitutions(&mut self, s: &str) {
        if s.contains("$(") || s.contains('`') {
            let _ = self.word_text(s);
        }
    }

    fn word(&mut self, w: &ast::Word) -> Arg {
        self.word_text(&w.value)
    }

    fn word_text(&mut self, raw: &str) -> Arg {
        match word::parse(raw, &self.opts) {
            Ok(pieces) => {
                let mut arg = Arg {
                    value: String::new(),
                    quoted: false,
                    may_disappear: true,
                    dynamic: false,
                    glob: false,
                    bound: false,
                    known: Some(String::new()),
                };
                self.pieces(raw, &pieces, &mut arg, false);
                arg.may_disappear &= arg.dynamic && !arg.quoted;
                if !arg.dynamic {
                    arg.known = None;
                }
                if arg.dynamic && !arg.glob && !self.bound_vars.is_empty() && !self.cwd_unknown {
                    let mut lit = String::new();
                    arg.bound = self.bound_pieces(raw, &pieces, &mut lit) && stays_below(&lit);
                }
                arg
            }
            Err(_) => Arg {
                value: raw.to_string(),
                quoted: false,
                may_disappear: false,
                dynamic: true,
                glob: false,
                bound: false,
                known: None,
            },
        }
    }

    /// Collects a word's literal text into `lit` when its only dynamic parts
    /// are bound loop variables (plain, or with a suffix removed), each written
    /// as `\u{1}`.
    fn bound_pieces(&self, raw: &str, pieces: &[WordPieceWithSource], lit: &mut String) -> bool {
        for p in pieces {
            let src = raw.get(p.start_index..p.end_index).unwrap_or("");
            match &p.piece {
                WordPiece::Text(s) | WordPiece::SingleQuotedText(s) => lit.push_str(s),
                WordPiece::EscapeSequence(s) => lit.push_str(s.strip_prefix('\\').unwrap_or(s)),
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    if !self.bound_pieces(raw, inner, lit) {
                        return false;
                    }
                }
                WordPiece::ParameterExpansion(_) => {
                    match bound_expansion(src).and_then(|n| self.bound_vars.get(n)) {
                        Some(cwd) if *cwd == self.cwd => lit.push('\u{1}'),
                        _ => return false,
                    }
                }
                _ => return false,
            }
        }
        true
    }

    /// The cwd the values of `for VAR in VALUES` are relative to, when every
    /// value is a static path or glob inside the workspace that cannot name
    /// `..` (`*.txt`, `src/*.rs`, `a.txt b.txt`). `for f; do` (the positional
    /// parameters) and computed values are not bound.
    fn loop_binding(&self, values: Option<&[ast::Word]>) -> Option<PathBuf> {
        let values = values.filter(|v| !v.is_empty())?;
        if self.cwd_unknown {
            return None;
        }
        for w in values {
            let s = self.static_word(w)?;
            let globby = |c: &str| c.contains(['*', '?', '[']);
            if s.is_empty()
                || s.starts_with('~')
                || s.split('/')
                    .any(|c| c == ".." || (c.starts_with('.') && globby(c)))
            {
                return None;
            }
            let dir = match s.find(['*', '?', '[']) {
                Some(cut) => match s[..cut].rfind('/') {
                    Some(0) => "/".to_string(),
                    Some(i) => s[..i].to_string(),
                    None => ".".to_string(),
                },
                None => s.clone(),
            };
            let (class, _) = classify_path_real(&self.resolve(&dir), self.ctx, true);
            if class != PathClass::Workspace {
                return None;
            }
        }
        Some(self.cwd.clone())
    }

    fn cwd_in_workspace(&self) -> bool {
        !self.cwd_unknown && classify_path_real(&self.cwd, self.ctx, true).0 == PathClass::Workspace
    }

    /// Assignments end a loop variable's binding (`f=/x`, `read f`, `let f=1`);
    /// builtins that assign make the value unknown.
    fn unbind(&mut self, assigns: &[String], argv: &[Arg]) {
        for n in assigns {
            self.bound_vars.remove(n);
        }
        let cmd = argv.first().map_or("", |c| basename(&c.value));
        let assigning = matches!(
            cmd,
            "read"
                | "mapfile"
                | "readarray"
                | "printf"
                | "declare"
                | "typeset"
                | "local"
                | "export"
                | "readonly"
                | "unset"
                | "getopts"
                | "let"
        );
        if assigning {
            // `export NAME` and `readonly NAME` keep the value.
            let keeps = matches!(cmd, "export" | "readonly");
            for a in argv[1..].iter().filter(|a| !keeps || a.value.contains('=')) {
                let n = a.value.split(['=', '[']).next().unwrap_or("");
                self.bound_vars.remove(n);
                self.vars.remove(n);
            }
            // Assigned without being named.
            for n in ["REPLY", "OPTARG", "OPTIND"] {
                self.vars.remove(n);
            }
        }
    }

    /// Arithmetic may assign to a loop variable: `(( f = 1 ))`.
    fn unbind_in(&mut self, text: &str) {
        self.bound_vars.retain(|n, _| !text.contains(n.as_str()));
        self.vars.retain(|n, _| !text.contains(n.as_str()));
    }

    fn pieces(&mut self, raw: &str, pieces: &[WordPieceWithSource], arg: &mut Arg, quoted: bool) {
        for p in pieces {
            let src = raw.get(p.start_index..p.end_index).unwrap_or("");
            match &p.piece {
                WordPiece::Text(s) => {
                    if !quoted && s.contains(['*', '?', '[']) {
                        arg.glob = true;
                    }
                    push_lit(arg, s);
                }
                WordPiece::SingleQuotedText(s) => {
                    arg.quoted = true;
                    push_lit(arg, s);
                }
                WordPiece::AnsiCQuotedText(s) => {
                    arg.quoted = true;
                    if has_escape_obfuscation(src) || has_escape_obfuscation(s) {
                        self.add(
                            Risk::Dangerous,
                            "uses $'\\x..' escape sequences that can hide the real command",
                        );
                    }
                    push_lit(arg, &decode_ansi_c(s));
                }
                WordPiece::DoubleQuotedSequence(inner)
                | WordPiece::GettextDoubleQuotedSequence(inner) => {
                    arg.quoted = true;
                    self.pieces(raw, inner, arg, true);
                }
                WordPiece::TildeExpansion(t) => {
                    arg.may_disappear = false;
                    match t {
                        TildeExpr::Home => {
                            if let Some(home) = self.vars.get("HOME") {
                                push_lit(arg, home);
                            } else if self.unknown_values.contains("HOME") || self.report.incomplete
                            {
                                push_expansion(arg, "~", None);
                            } else if let Some(home) =
                                self.ctx.user_home.as_ref().or(self.ctx.home.as_ref())
                            {
                                push_lit(arg, &home.to_string_lossy());
                            } else {
                                push_expansion(arg, "~", None);
                            }
                        }
                        TildeExpr::UserHome(u) => push_expansion(arg, &format!("~{u}"), None),
                        _ => push_expansion(arg, "~+", None),
                    }
                }
                WordPiece::ParameterExpansion(_) => {
                    if src.contains("$(") || src.contains('`') {
                        self.add(
                            Risk::Dangerous,
                            "command substitution hidden inside a parameter expansion",
                        );
                    }
                    match param_name(src) {
                        _ if self.script_path.is_some()
                            && matches!(
                                src,
                                "$0" | "${0}"
                                    | "$BASH_SOURCE"
                                    | "${BASH_SOURCE}"
                                    | "${BASH_SOURCE[0]}"
                            ) =>
                        {
                            push_lit(arg, self.script_path.as_deref().unwrap_or_default())
                        }
                        _ => {
                            // Unquoted, a value with spaces or glob characters is
                            // split and globbed: not one known word.
                            let v = self.known_expansion(src).filter(|v| {
                                quoted || !v.contains([' ', '\t', '\n', '*', '?', '['])
                            });
                            push_expansion(arg, src, v.as_deref());
                        }
                    }
                }
                WordPiece::CommandSubstitution(s) | WordPiece::BackquotedCommandSubstitution(s) => {
                    let prev = self.top;
                    self.in_child(|a| a.program_text(s, false));
                    self.top = prev;
                    match self.static_subst(s) {
                        Some(v) => push_lit(arg, &v),
                        None => push_expansion(arg, "$(…)", None),
                    }
                }
                WordPiece::EscapeSequence(s) => {
                    push_lit(arg, s.strip_prefix('\\').unwrap_or(s));
                }
                WordPiece::ArithmeticExpression(e) => {
                    arg.may_disappear = false;
                    push_expansion(arg, "$((…))", None);
                    self.text_substitutions(&e.value);
                }
            }
        }
    }

    fn simple(&mut self, sc: &ast::SimpleCommand) {
        let declaration = sc
            .word_or_name
            .as_ref()
            .and_then(|word| self.static_word(word))
            .is_some_and(|name| {
                matches!(
                    name.as_str(),
                    "export" | "declare" | "typeset" | "local" | "readonly"
                )
            });
        let mut assigns: Vec<String> = Vec::new();
        let mut values: Vec<Option<String>> = Vec::new();
        let mut redirects: Vec<&ast::IoRedirect> = Vec::new();
        if let Some(prefix) = &sc.prefix {
            for item in &prefix.0 {
                match item {
                    ast::CommandPrefixOrSuffixItem::IoRedirect(r) => redirects.push(r),
                    ast::CommandPrefixOrSuffixItem::AssignmentWord(a, _) => {
                        let name = match &a.name {
                            ast::AssignmentName::VariableName(n) => n.clone(),
                            ast::AssignmentName::ArrayElementName(n, _) => n.clone(),
                        };
                        match &a.value {
                            ast::AssignmentValue::Scalar(w) => {
                                let v = self.word(w);
                                values.push(v.resolved().map(str::to_string));
                            }
                            ast::AssignmentValue::Array(items) => {
                                for (k, v) in items {
                                    if let Some(k) = k {
                                        self.word(k);
                                    }
                                    self.word(v);
                                }
                                values.push(None);
                            }
                        }
                        if matches!(a.name, ast::AssignmentName::ArrayElementName(..)) {
                            *values.last_mut().unwrap() = None;
                        }
                        if a.append {
                            // `NAME+=value`: known when both parts are.
                            let v = values.last_mut().unwrap();
                            *v = v
                                .take()
                                .and_then(|v| Some(format!("{}{v}", self.vars.get(&name)?)));
                        }
                        assigns.push(name);
                    }
                    ast::CommandPrefixOrSuffixItem::Word(w) => {
                        self.word(w);
                    }
                    ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, sub) => {
                        self.compound_list(&sub.list)
                    }
                }
            }
        }
        let mut argv: Vec<Arg> = Vec::new();
        let mut name_end: Option<usize> = None;
        if let Some(w) = &sc.word_or_name {
            name_end = w.loc.as_ref().map(|l| l.end.index);
            argv.push(self.word(w));
        }
        if let Some(suffix) = &sc.suffix {
            for item in &suffix.0 {
                match item {
                    ast::CommandPrefixOrSuffixItem::Word(w)
                    | ast::CommandPrefixOrSuffixItem::AssignmentWord(_, w) => {
                        let assignment = declaration
                            .then(|| w.value.split_once('='))
                            .flatten()
                            .filter(|(name, _)| {
                                !name.is_empty()
                                    && name
                                        .trim_end_matches('+')
                                        .chars()
                                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                            });
                        if let Some((name, value)) = assignment {
                            let mut arg = self.word_text(value);
                            arg.may_disappear = false;
                            arg.value = format!("{name}={}", arg.value);
                            arg.known = arg.known.map(|value| format!("{name}={value}"));
                            argv.push(arg);
                        } else {
                            argv.push(self.word(w));
                        }
                    }
                    ast::CommandPrefixOrSuffixItem::IoRedirect(r) => redirects.push(r),
                    ast::CommandPrefixOrSuffixItem::ProcessSubstitution(_, sub) => {
                        self.compound_list(&sub.list);
                        argv.push(Arg::lit("/dev/fd/63"));
                    }
                }
            }
        }
        let disappears = |arg: &Arg| arg.dynamic && !arg.quoted && arg.resolved() == Some("");
        if argv.first().is_some_and(disappears) {
            name_end = None;
        }
        argv.retain(|arg| !disappears(arg));
        let previous_operation = self.begin_operation(&argv);
        if let Some(i) = self.operation {
            self.report.operations[i].extra_variables = !argv.is_empty() && !assigns.is_empty();
        }
        for (name, value) in assigns.iter().zip(&values) {
            self.variable(name, value.clone());
        }
        for r in redirects {
            self.redirect(r);
        }
        if argv.is_empty() {
            // `NAME=value` on its own sets the variable for what follows.
            for (n, v) in assigns.iter().zip(&values) {
                self.set_var(n, v.as_deref());
            }
        }
        let known_before: Vec<_> = self.vars.keys().cloned().collect();
        self.unbind(&assigns, &argv);
        self.unknown_values.extend(
            known_before
                .into_iter()
                .filter(|name| !self.vars.contains_key(name)),
        );
        if argv.is_empty() {
            for n in &assigns {
                if let Some((risk, why)) = rules::var_assignment_risk(n) {
                    self.add(risk, why);
                    self.report.changes_session = true;
                }
            }
            if !assigns.is_empty() {
                self.push_command(format!("{}=…", assigns.join("=… ")));
                self.add(Risk::Mutating, "sets session variables");
                self.report.changes_session = true;
            }
            self.operation = previous_operation;
            return;
        }
        for n in &assigns {
            if matches!(n.as_str(), "LD_PRELOAD" | "LD_AUDIT" | "BASH_ENV" | "ENV") {
                self.add(
                    Risk::Dangerous,
                    format!("runs a command with {n} set (code injection)"),
                );
            }
        }
        self.push_command(shell_join(&argv));
        let name_end = if self.top && self.depth == 1 {
            name_end
        } else {
            None
        };
        // `NAME=value cmd`: a function, script or child shell it runs sees
        // NAME (exported); afterwards NAME is as before.
        let before: Vec<(String, Option<String>, bool)> = assigns
            .iter()
            .map(|n| {
                (
                    n.clone(),
                    self.vars.get(n).cloned(),
                    self.exported.contains(n),
                )
            })
            .collect();
        for (n, v) in assigns.iter().zip(&values) {
            self.set_var(n, v.as_deref());
            self.exported.insert(n.clone());
        }
        self.exec_inner(argv, name_end);
        self.finish_operation();
        for (n, v, exported) in before {
            self.set_var(&n, v.as_deref());
            if !exported {
                self.exported.remove(&n);
            }
        }
        self.operation = previous_operation;
    }

    /// Records a simple command of the line (once).
    fn push_command(&mut self, c: String) {
        if !self.report.commands.contains(&c) {
            self.report.commands.push(c);
        }
    }

    fn redirect(&mut self, r: &ast::IoRedirect) {
        use ast::IoFileRedirectKind as K;
        use ast::IoFileRedirectTarget as T;
        let previous = self.redirecting;
        self.redirecting = true;
        match r {
            ast::IoRedirect::File(_, kind, target) => {
                let t = match target {
                    T::Filename(w) => Some(self.word(w)),
                    T::ProcessSubstitution(_, sub) => {
                        self.compound_list(&sub.list);
                        None
                    }
                    T::Duplicate(w) => {
                        self.word(w);
                        None
                    }
                    T::Fd(_) => None,
                };
                if let Some(t) = t {
                    match kind {
                        K::Write | K::Append | K::Clobber | K::ReadAndWrite => {
                            let v = Verdict::new(Risk::Mutating, "redirects output to a file");
                            self.write_effect(&target_of(&t), &v);
                            let class = self.class_of(&t);
                            if !matches!(class, Some(PathClass::Null)) {
                                self.add(Risk::Mutating, format!("writes {}", t.value));
                            }
                        }
                        K::Read => self.read_effect(&target_of(&t)),
                        K::DuplicateInput | K::DuplicateOutput => {}
                    }
                }
            }
            ast::IoRedirect::HereDocument(_, doc) => {
                if doc.requires_expansion {
                    self.text_substitutions(&doc.doc.value);
                }
            }
            ast::IoRedirect::HereString(_, w) => {
                self.word(w);
            }
            ast::IoRedirect::OutputAndError(w, _) => {
                let t = self.word(w);
                let v = Verdict::new(Risk::Mutating, "redirects output to a file");
                self.write_effect(&target_of(&t), &v);
                if !matches!(self.class_of(&t), Some(PathClass::Null)) {
                    self.add(Risk::Mutating, format!("writes {}", t.value));
                }
            }
        }
        self.redirecting = previous;
    }

    fn class_of(&self, a: &Arg) -> Option<PathClass> {
        if a.dynamic {
            return None;
        }
        Some(classify_path_real(&self.resolve(&a.value), self.ctx, true).0)
    }

    fn resolve(&self, p: &str) -> PathBuf {
        crate::paths::resolve_literal(p, &self.cwd)
    }

    fn target_path(&self, path: &str, follow_last: bool) -> PathBuf {
        let path = Path::new(path);
        let path = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.cwd.join(path)
        };
        let real = crate::real_path(&path, follow_last).unwrap_or(path);
        crate::paths::resolve_literal(&real.to_string_lossy(), Path::new("/"))
    }

    fn function_def(&mut self, fd: &ast::FunctionDefinition) {
        self.report.incomplete = true;
        let name = self
            .static_word(&fd.fname)
            .unwrap_or_else(|| fd.fname.value.clone());
        let previous = self.begin_operation(&[]);
        self.variable(&format!("function:{name}"), Some(fd.body.to_string()));
        self.opaque();
        self.add(Risk::Mutating, format!("defines shell function {name}"));
        self.report.changes_session = true;
        self.operation = previous;
        let mut calls = 0usize;
        let mut async_calls = false;
        count_calls(&fd.body.0, &name, &mut calls, &mut async_calls, self);
        if calls >= 2 || (calls >= 1 && async_calls) {
            self.add(
                Risk::Forbidden,
                format!("fork bomb (function {name} spawns itself)"),
            );
        }
        self.local_funcs
            .insert(name.clone(), Rc::new(fd.body.clone()));
        // Checked here once without arguments (`$1`… unknown); each call
        // checks it again with its own (see `replay`).
        let key = format!("fn:{name}");
        let outer = self.positional.take();
        let fresh = self.expanding.insert(key.clone());
        self.compound(&fd.body.0);
        if fresh {
            self.expanding.remove(&key);
        }
        self.positional = outer;
    }

    fn write_effect(&mut self, t: &Target, v: &Verdict) {
        self.path_access(
            t,
            if v.deletes {
                AccessKind::Delete
            } else {
                AccessKind::Write
            },
        );
        if v.recursive
            && v.deletes
            && !t.dynamic
            && !t.glob
            && matches!(
                crate::classify_path(&self.resolve(&t.path), self.ctx),
                PathClass::Root | PathClass::Home
            )
        {
            self.add(
                Risk::Forbidden,
                "recursively deletes the root or a home directory",
            );
        }
        if t.dynamic && t.bound && !v.deletes && self.cwd_in_workspace() {
            // Chosen at runtime, but only among workspace files (`for f in
            // *.txt; do mv …`, `find . -exec cp {} {}.bak`). Deletes stay below.
            self.add(
                v.risk.max(Risk::Mutating),
                "writes workspace files chosen at runtime",
            );
            return;
        }
        if t.dynamic || self.cwd_unknown && !t.path.starts_with('/') && !t.path.starts_with('~') {
            let why = if v.deletes {
                "deletes files chosen at runtime"
            } else {
                "writes to a path computed at runtime"
            };
            self.uncertain(v.risk.max(Risk::Mutating).bump(), why);
            self.report.writes_outside_workspace = true;
            return;
        }
        let mut p = t.path.clone();
        if t.glob {
            // Classify the directory part before the first glob.
            let cut = p.find(['*', '?', '[']).unwrap_or(p.len());
            p = match p[..cut].rfind('/') {
                Some(0) => "/".into(),
                Some(i) => p[..i].to_string(),
                None => ".".into(),
            };
        }
        let lexical = self.resolve(&p);
        if v.recursive
            && v.deletes
            && self
                .vars
                .get("HOME")
                .is_some_and(|home| self.resolve(home) == lexical)
        {
            self.add(
                Risk::Forbidden,
                "recursively deletes the effective home directory",
            );
        }
        let changes = if v.deletes { "deletes" } else { "modifies" };
        // `rm` removes a symlink itself; writes go through it.
        let resolved = self.target_path(&p, !v.deletes);
        let class = match classify_path_real(&lexical, self.ctx, !v.deletes).0 {
            protected @ PathClass::Protected(_) => protected,
            _ => crate::classify_path(&resolved, self.ctx),
        };
        match class {
            PathClass::Null | PathClass::Workspace | PathClass::Temp => {}
            PathClass::Protected(l) => {
                if v.recursive && v.deletes && is_top_level(&resolved) {
                    self.add(
                        Risk::Forbidden,
                        format!("recursively deletes system directory {l}"),
                    );
                } else {
                    self.add(Risk::Dangerous, format!("{changes} protected path {l}"));
                }
            }
            PathClass::Root => {
                if v.recursive && v.deletes {
                    self.add(Risk::Forbidden, "recursively deletes the root filesystem");
                } else if v.recursive {
                    self.add(Risk::Dangerous, "recursively changes the root filesystem");
                } else {
                    self.add(Risk::Dangerous, format!("{changes} /"));
                }
                self.report.writes_outside_workspace = true;
            }
            PathClass::Home => {
                if v.recursive && v.deletes {
                    self.add(Risk::Forbidden, "recursively deletes the home directory");
                } else {
                    self.add(
                        Risk::Dangerous,
                        format!("{changes} the home directory itself"),
                    );
                }
                self.report.writes_outside_workspace = true;
            }
            PathClass::System => {
                let top_level = is_top_level(&resolved);
                if v.recursive && v.deletes && top_level {
                    self.add(
                        Risk::Forbidden,
                        format!(
                            "recursively deletes system directory {}",
                            resolved.display()
                        ),
                    );
                } else {
                    self.add(
                        Risk::Dangerous,
                        format!("{changes} system path {}", resolved.display()),
                    );
                }
                self.report.writes_outside_workspace = true;
            }
            PathClass::Outside => {
                self.add(
                    v.risk.max(Risk::Mutating).bump(),
                    format!(
                        "{changes} files outside the workspace ({})",
                        resolved.display()
                    ),
                );
                self.report.writes_outside_workspace = true;
            }
        }
    }

    fn read_effect(&mut self, t: &Target) {
        self.path_access(t, AccessKind::Read);
        let path = match (&t.known, t.dynamic) {
            (_, false) => t.path.as_str(),
            (Some(k), true) => k.as_str(),
            (None, true) => {
                self.opaque();
                self.uncertain(Risk::Mutating, "read target cannot be determined");
                return;
            }
        };
        if path.is_empty() {
            return;
        }
        let class = match classify_path_real(&self.resolve(path), self.ctx, true).0 {
            protected @ PathClass::Protected(_) => protected,
            _ => crate::classify_path(&self.target_path(path, true), self.ctx),
        };
        if let PathClass::Protected(l) = class {
            self.add(Risk::Mutating, format!("{PROTECTED_READ} {l}"));
            self.report.reads_protected = true;
        }
    }

    fn apply(&mut self, v: Verdict) {
        if v.unlisted {
            self.opaque();
        }
        self.add(v.risk, v.reason.clone());
        if v.network {
            self.report.network = true;
            if let Some(i) = self.operation {
                self.report.operations[i].network = true;
                self.report.operations[i].hosts = network_hosts(&self.report.operations[i]);
            }
        }
        if v.session {
            self.report.changes_session = true;
            if self
                .operation
                .is_some_and(|i| self.report.operations[i].variables.is_empty())
            {
                self.opaque();
            }
        }
        for w in &v.writes {
            self.write_effect(w, &v);
        }
        for r in &v.reads {
            self.read_effect(r);
        }
    }

    /// Analyzes the shell script at `path` (run as a child process with
    /// `args`) with the same rules. Only its Dangerous and Forbidden findings
    /// are taken over, so a script is Mutating unless it contains something
    /// destructive. Returns false when the file is not a readable, small
    /// enough shell script.
    fn script(
        &mut self,
        shown: &str,
        path: &std::path::Path,
        by_shell: bool,
        args: &[Arg],
    ) -> bool {
        let key = format!("script:{}", path.display());
        if self.expanding.contains(&key) {
            // A script that runs itself: its contents are being analyzed already.
            return true;
        }
        let Some(text) = read_script(path, by_shell, self.script_budget) else {
            self.opaque();
            return false;
        };
        if let Some(i) = self.operation {
            self.report.operations[i].payload = true;
        }
        self.read_effect(&Target {
            path: shown.to_string(),
            dynamic: false,
            glob: false,
            bound: false,
            known: None,
        });
        self.script_budget -= text.len();
        // A child process sees neither the session's aliases nor its
        // functions, and of its variables only the exported ones.
        let child_ctx = Context {
            aliases: HashMap::new(),
            functions: HashMap::new(),
            ..self.ctx.clone()
        };
        let mut sub = Analyzer {
            ctx: &child_ctx,
            lookup: self.lookup,
            report: RiskReport::default(),
            operation: None,
            redirecting: false,
            readonly: HashMap::new(),
            unknown_values: self.unknown_values.clone(),
            depth: self.depth,
            top: false,
            cwd: self.cwd.clone(),
            cwd_unknown: self.cwd_unknown,
            expanding: self.expanding.clone(),
            local_funcs: HashMap::new(),
            sudo_inserts: Vec::new(),
            opts: self.opts.clone(),
            child: true,
            script_budget: self.script_budget,
            bound_vars: HashMap::new(),
            script_path: Some(shown.to_string()),
            vars: self.exported_vars(),
            exported: self.exported.clone(),
            positional: Some(known_values(args)),
            replaying: false,
            calls_left: self.calls_left,
        };
        sub.expanding.insert(key);
        sub.program_text(&text, false);
        self.script_budget = sub.script_budget;
        self.calls_left = sub.calls_left;
        for f in sub.report.findings {
            // Protected reads ask as they would on the command line.
            if f.risk >= Risk::Dangerous || f.reason.contains(PROTECTED_READ) {
                if f.uncertain {
                    self.uncertain(f.risk, format!("{shown}: {}", f.reason));
                } else {
                    self.add(f.risk, format!("{shown}: {}", f.reason));
                }
            }
        }
        self.report.reads_protected |= sub.report.reads_protected;
        self.report.network |= sub.report.network;
        self.report.writes_outside_workspace |= sub.report.writes_outside_workspace;
        self.report.incomplete |= sub.report.incomplete;
        let offset = self.report.operations.len();
        for mut op in sub.report.operations {
            op.parent = op.parent.map(|i| i + offset).or(self.operation);
            self.report.operations.push(op);
        }
        self.report.scripts.push((path.to_path_buf(), text));
        self.report.scripts.extend(sub.report.scripts);
        true
    }

    /// The known variables a child process inherits.
    fn exported_vars(&self) -> HashMap<String, String> {
        self.vars
            .iter()
            .filter(|(k, _)| self.exported.contains(*k))
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The value of `$NAME`, `${NAME}` or `$1`… when it is known.
    fn known_expansion(&self, src: &str) -> Option<String> {
        if let Some(i) = positional_index(src) {
            return self.positional.as_ref()?.get(i - 1).cloned();
        }
        let name = plain_expansion(src)?;
        if self.bound_vars.contains_key(name) {
            return None;
        }
        if let Some(value) = self.readonly.get(name) {
            return value.clone();
        }
        self.vars.get(name).cloned().or_else(|| {
            (self.ctx.variables_complete
                && !self.child
                && !self.replaying
                && !self.report.incomplete
                && !self.unknown_values.contains(name))
            .then(String::new)
        })
    }

    /// Records what an assignment leaves in `name`: a known value or unknown.
    fn set_var(&mut self, name: &str, value: Option<&str>) {
        self.bound_vars.remove(name);
        if self.readonly.contains_key(name) {
            return;
        }
        match value {
            Some(v) => {
                self.unknown_values.remove(name);
                self.vars.insert(name.to_string(), v.to_string());
            }
            None => {
                self.unknown_values.insert(name.into());
                self.vars.remove(name);
            }
        }
    }

    /// Runs `f` as a child shell (subshell, `$(…)`, `bash -c`): `exit`/`exec`,
    /// `cd`, variables, functions and session settings stay in it.
    fn in_child(&mut self, f: impl FnOnce(&mut Self)) {
        let readonly = self.readonly.clone();
        let unknown = self.unknown_values.clone();
        let saved = (
            self.child,
            self.cwd.clone(),
            self.cwd_unknown,
            self.report.changes_session,
            self.local_funcs.clone(),
            self.bound_vars.clone(),
            self.vars.clone(),
            self.exported.clone(),
            self.positional.clone(),
        );
        self.child = true;
        f(self);
        (
            self.child,
            self.cwd,
            self.cwd_unknown,
            self.report.changes_session,
            self.local_funcs,
            self.bound_vars,
            self.vars,
            self.exported,
            self.positional,
        ) = saved;
        self.readonly = readonly;
        self.unknown_values = unknown;
    }

    /// The output of a command substitution that needs no execution:
    /// `pwd`, `dirname P`, `realpath P`, `readlink -f P`, optionally after
    /// `cd P &&` (`$(cd "$(dirname "$0")" && pwd)`), with static `P`.
    fn static_subst(&mut self, text: &str) -> Option<String> {
        let prog = self.parse(text).ok()?;
        let [cl] = prog.complete_commands.as_slice() else {
            return None;
        };
        let [ast::CompoundListItem(aol, _)] = cl.0.as_slice() else {
            return None;
        };
        let mut steps = vec![&aol.first];
        for next in &aol.additional {
            match next {
                ast::AndOr::And(p) => steps.push(p),
                ast::AndOr::Or(_) => return None,
            }
        }
        let mut cwd = self.cwd.clone();
        let mut out = None;
        for p in steps {
            let [ast::Command::Simple(sc)] = p.seq.as_slice() else {
                return None;
            };
            if sc.prefix.is_some() || out.is_some() {
                return None;
            }
            let mut argv = vec![sc.word_or_name.as_ref()?.value.clone()];
            for item in sc.suffix.iter().flat_map(|s| &s.0) {
                match item {
                    ast::CommandPrefixOrSuffixItem::Word(w) => argv.push(w.value.clone()),
                    _ => return None,
                }
            }
            let mut words = Vec::new();
            for raw in &argv {
                let a = self.word_text(raw);
                if a.dynamic || a.glob {
                    return None;
                }
                words.push(a.value);
            }
            if self.ctx.aliases.contains_key(&words[0])
                || self.ctx.functions.contains_key(&words[0])
                || self.local_funcs.contains_key(&words[0])
            {
                return None;
            }
            match words[0].as_str() {
                "cd" if words.len() == 2
                    && self.vars.get("CDPATH").is_none_or(String::is_empty) =>
                {
                    let target = crate::paths::resolve_literal(&words[1], &cwd);
                    if !target.is_dir() {
                        return None;
                    }
                    cwd = target;
                }
                "pwd" if words.len() == 1 || words.len() == 2 && words[1] == "-L" => {
                    out = Some(cwd.display().to_string())
                }
                "pwd" if words.len() == 2 && words[1] == "-P" => {
                    out = Some(
                        std::fs::canonicalize(&cwd)
                            .ok()?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                "dirname" if words.len() == 2 && !words[1].starts_with('-') => {
                    let p = words[1].trim_end_matches('/');
                    out = Some(match p.rfind('/') {
                        _ if p.is_empty() => "/".to_string(),
                        Some(0) => "/".to_string(),
                        Some(i) => p[..i].to_string(),
                        None => ".".to_string(),
                    });
                }
                "realpath" if words.len() == 2 && !words[1].starts_with('-') => {
                    out = Some(
                        std::fs::canonicalize(cwd.join(&words[1]))
                            .ok()?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                "readlink"
                    if words.len() == 3 && matches!(words[1].as_str(), "-f" | "-e" | "-m") =>
                {
                    out = Some(
                        std::fs::canonicalize(cwd.join(&words[2]))
                            .ok()?
                            .to_string_lossy()
                            .into_owned(),
                    );
                }
                _ => return None,
            }
        }
        out
    }

    /// Analyzes a session function's body for one call, `$1`… being the
    /// known arguments.
    fn call_function(&mut self, key: &str, body: &str, args: &[Arg]) {
        self.expanding.insert(key.to_string());
        let outer = self.positional.replace(known_values(args));
        self.program_text(body, false);
        self.positional = outer;
        self.expanding.remove(key);
    }

    /// Re-analyzes, at a call, a function defined in the analyzed text with
    /// the call's arguments as `$1`… and the variables known there: what it
    /// reads through them. Its definition already applied the rest (its
    /// directory changes are not applied twice).
    fn replay(&mut self, key: String, body: &ast::FunctionBody, args: &[Arg]) {
        if self.calls_left == 0 || self.expanding.contains(&key) {
            return;
        }
        self.calls_left -= 1;
        let saved = (
            self.cwd.clone(),
            self.cwd_unknown,
            self.positional.replace(known_values(args)),
            self.replaying,
        );
        self.replaying = true;
        self.expanding.insert(key.clone());
        self.compound(&body.0);
        self.expanding.remove(&key);
        (self.cwd, self.cwd_unknown, self.positional, self.replaying) = saved;
    }

    /// Dispatches one simple command (`argv[0]` is the command name).
    fn exec(&mut self, argv: Vec<Arg>, name_end: Option<usize>) {
        let previous = self.begin_operation(&argv);
        self.exec_inner(argv, name_end);
        self.finish_operation();
        self.operation = previous;
    }

    fn exec_inner(&mut self, argv: Vec<Arg>, name_end: Option<usize>) {
        let Some(name_arg) = argv.first() else {
            return;
        };
        if name_arg.dynamic && (name_arg.resolved().is_none() || self.report.incomplete) {
            self.opaque();
            self.uncertain(
                Risk::Dangerous,
                format!("command name is computed at runtime ({})", name_arg.value),
            );
            return;
        }
        let name = name_arg.resolved().unwrap_or(&name_arg.value).to_string();
        let args: Vec<Arg> = argv[1..]
            .iter()
            .map(|arg| {
                if !self.report.incomplete
                    && !arg.glob
                    && let Some(value) = arg.resolved()
                {
                    return Arg::lit(value);
                }
                arg.clone()
            })
            .collect();

        if !name.contains('/') {
            if let Some(alias) = self.ctx.aliases.get(&name).cloned() {
                let key = format!("alias:{name}");
                if !self.expanding.contains(&key) {
                    if let Some(i) = self.operation {
                        self.report.operations[i].transparent = true;
                    }
                    self.expanding.insert(key.clone());
                    self.expand_alias(&alias, args);
                    self.expanding.remove(&key);
                    return;
                }
            }
            if let Some(body) = self.local_funcs.get(&name).cloned() {
                if let Some(i) = self.operation {
                    self.report.operations[i].transparent = true;
                }
                self.replay(format!("fn:{name}"), &body, &args);
                return;
            }
            if let Some(body) = self.ctx.functions.get(&name).cloned() {
                if let Some(i) = self.operation {
                    self.report.operations[i].transparent = true;
                }
                let key = format!("fn:{name}");
                if self.expanding.contains(&key) {
                    self.add(Risk::Mutating, format!("recursive function {name}"));
                } else {
                    self.call_function(&key, &body, &args);
                }
                return;
            }
        }

        let base = basename(&name).to_string();
        if !name.contains('/') {
            let executable = (self.lookup)(
                &name,
                &self.cwd,
                self.vars.get("PATH").map(String::as_str),
                !self.child,
            );
            if let Some(path) = executable {
                let real = crate::real_path(&path, true).unwrap_or_else(|| path.clone());
                let workspace = crate::real_path(&self.ctx.workspace, true)
                    .unwrap_or_else(|| self.ctx.workspace.clone());
                let venv = self
                    .vars
                    .get("VIRTUAL_ENV")
                    .map(|root| self.target_path(root, true).join("bin"));
                let local = real.starts_with(&workspace)
                    && !in_system_bin_dir(&path.to_string_lossy())
                    && venv
                        .as_ref()
                        .is_none_or(|bin| path.parent() != Some(bin.as_path()));
                if let Some(i) = self.operation {
                    self.report.operations[i].executable = Some(path.clone());
                    self.report.operations[i].local_program = local;
                }
                if local {
                    self.script(&path.to_string_lossy(), &path, false, &args);
                    self.opaque();
                    self.add(
                        Risk::Mutating,
                        format!("runs workspace program {}", path.display()),
                    );
                    return;
                }
            }
        }
        if name.contains('/') && !in_system_bin_dir(&name) {
            // `./x.sh`, `/path/x.sh`, `~/bin/x.sh`: a shell script is analyzed.
            let path = self.target_path(&name, true);
            if self.script(&name, &path, false, &args) {
                self.add(Risk::Mutating, format!("runs shell script {name}"));
                return;
            }
            self.opaque();
            self.add(Risk::Mutating, format!("runs local program {name}"));
            return;
        }
        if let Some(i) = self.operation {
            self.report.operations[i].transparent |= matches!(
                base.as_str(),
                "command" | "builtin" | "env" | "timeout" | "time" | "nice" | "nohup" | "stdbuf"
            );
        }
        match base.as_str() {
            "shift" => self.positional = None,
            "set"
                if args
                    .iter()
                    .any(|a| !a.value.starts_with(['-', '+']) || a.value == "--") =>
            {
                self.positional = None
            }
            _ => {}
        }
        match base.as_str() {
            // In a script or `bash -c`, these end that child shell only.
            "exec" if self.child => self.exec_after_options(&args, &["-a"]),
            "exit" | "logout" if self.child => self.add(Risk::Safe, "ends the child shell"),
            "exec" => self.add(
                Risk::Forbidden,
                "agent may not use exec (it replaces the shell session)",
            ),
            "exit" | "logout" => self.add(Risk::Forbidden, "agent may not exit the shell session"),
            "sudo" => self.sudo(&args, name_end),
            "doas" | "pkexec" | "run0" => {
                self.add(
                    Risk::Dangerous,
                    format!("runs a command with elevated privileges ({base})"),
                );
                self.exec_after_options(&args, &["-u", "-C", "--user"]);
            }
            "su" => {
                self.add(Risk::Dangerous, "switches to another user (su)");
                if let Some(c) = opt_value(&args, Some('c'), &["command"]).first() {
                    if c.dynamic {
                        self.add(Risk::Dangerous, "su -c with a command built at runtime");
                    } else {
                        let s = c.value.clone();
                        self.program_text(&s, false);
                    }
                }
            }
            "env" => self.env(&args),
            "source" | "." => {
                self.add(Risk::Mutating, "sources a script into the session");
                self.report.changes_session = true;
                if let Some(i) = self.operation {
                    self.report.operations[i].payload = true;
                }
                if let Some(arg) = args.first() {
                    self.read_effect(&target_of(arg));
                }
                if let Some(value) = args.first().filter(|a| !a.glob).and_then(Arg::resolved) {
                    let path = self.target_path(value, true);
                    if let Some(text) = read_script(&path, true, self.script_budget) {
                        self.script_budget -= text.len();
                        let positional = self.positional.clone();
                        if args.len() > 1 {
                            self.positional = Some(known_values(&args[1..]));
                        }
                        self.program_text(&text, false);
                        self.positional = positional;
                        self.report.scripts.push((path, text));
                    } else {
                        self.report.incomplete = true;
                        self.vars.clear();
                        self.bound_vars.clear();
                    }
                } else {
                    self.report.incomplete = true;
                    self.vars.clear();
                    self.bound_vars.clear();
                }
            }
            "command" => {
                if has_flag(&args, &['v', 'V'], &[]) {
                    self.add(Risk::Safe, "command lookup");
                } else {
                    self.exec_after_options(&args, &[]);
                }
            }
            "builtin" | "nohup" | "setsid" | "unbuffer" | "time" | "caffeinate" | "catchsegv"
            | "xvfb-run" | "firejail" | "proxychains" | "proxychains4" | "tsocks" | "torsocks"
            | "numactl" | "nocache" | "chronic" => {
                self.exec_after_options(&args, &["-o", "--output", "-f", "--format"]);
            }
            "nice" | "renice_run" => self.exec_after_options(&args, &["-n", "--adjustment"]),
            "ionice" => self.exec_after_options(&args, &["-c", "-n", "--class", "--classdata"]),
            "stdbuf" => self
                .exec_after_options(&args, &["-i", "-o", "-e", "--input", "--output", "--error"]),
            "timeout" => {
                let rest = skip_options(&args, &["-s", "--signal", "-k", "--kill-after"]);
                if rest.len() > 1 {
                    self.exec(rest[1..].to_vec(), None);
                }
            }
            "chrt" | "taskset" => {
                let rest = skip_options(&args, &["-c", "--cpu-list"]);
                let skip = usize::from(rest.first().is_some_and(|a| {
                    a.value
                        .chars()
                        .all(|c| c.is_ascii_hexdigit() || c == ',' || c == '-' || c == 'x')
                }));
                if rest.len() > skip {
                    self.exec(rest[skip..].to_vec(), None);
                }
            }
            "flock" => {
                if let Some(c) = opt_value(&args, Some('c'), &["command"]).first() {
                    let s = c.value.clone();
                    self.program_text(&s, false);
                } else {
                    let rest =
                        skip_options(&args, &["-w", "--timeout", "-E", "--conflict-exit-code"]);
                    if rest.len() > 1 {
                        self.exec(rest[1..].to_vec(), None);
                    }
                }
            }
            "watch" => {
                self.add(
                    Risk::Mutating,
                    "repeats a command until interrupted (watch)",
                );
                let rest = skip_options(&args, &["-n", "--interval", "-d", "--differences"]);
                if !rest.is_empty() {
                    let s = shell_join(&rest);
                    self.program_text(&s, false);
                }
            }
            "strace" | "ltrace" | "valgrind" | "perf" | "gdb" | "lldb" => {
                self.add(Risk::Mutating, format!("runs a program under {base}"));
                let rest = skip_options(&args, &["-o", "-p", "-e", "-s", "-u", "--output"]);
                let rest = if base == "perf" {
                    rest.into_iter().skip(1).collect()
                } else {
                    rest
                };
                if !rest.is_empty() {
                    self.exec(rest, None);
                }
            }
            "systemd-run" | "script" => {
                self.add(Risk::Mutating, format!("runs a command via {base}"));
                if let Some(c) = opt_value(&args, Some('c'), &["command"]).first() {
                    let s = c.value.clone();
                    self.program_text(&s, false);
                } else if base == "systemd-run" {
                    self.exec_after_options(
                        &args,
                        &[
                            "-u",
                            "--unit",
                            "-p",
                            "--property",
                            "--uid",
                            "--gid",
                            "-E",
                            "--setenv",
                        ],
                    );
                }
            }
            "xargs" | "parallel" => self.xargs(&args),
            "find" => self.find(&args),
            "eval" => {
                if args.iter().any(|a| a.dynamic) {
                    self.add(Risk::Dangerous, "eval of a string built at runtime");
                } else {
                    let s = shell_join(&args);
                    self.program_text(&s, false);
                }
            }
            b if SHELLS.contains(&b) || b == "busybox" => {
                if let Some(i) = self.operation {
                    self.report.operations[i].payload = true;
                    self.report.operations[i].transparent =
                        SH_SYNTAX.contains(&b) && args.first().is_some_and(|a| a.value == "-c");
                }
                if ["ENV", "BASH_ENV"]
                    .iter()
                    .any(|name| self.exported.contains(*name))
                {
                    self.opaque();
                    self.uncertain(
                        Risk::Dangerous,
                        "child shell startup code is not part of the visible command",
                    );
                }
                self.shell_c(b, &args);
            }
            "python" | "python2" | "python3" | "perl" | "ruby" | "node" | "php" | "lua"
            | "deno" | "bun" | "Rscript" | "julia" => {
                let code = opt_value(&args, Some('c'), &[])
                    .into_iter()
                    .chain(opt_value(&args, Some('e'), &["eval"]))
                    .next()
                    .map(|a| a.value.clone());
                if let Some(code) = code {
                    self.opaque();
                    if INLINE_CODE_DANGER.iter().any(|p| code.contains(p)) {
                        self.add(
                            Risk::Dangerous,
                            format!("inline {base} code runs commands or deletes files"),
                        );
                    } else {
                        self.add(Risk::Mutating, format!("runs inline {base} code"));
                    }
                } else {
                    self.apply(rules::classify(&base, &args));
                }
            }
            "ulimit" => {
                let set = rules::operands(&args)
                    .iter()
                    .any(|a| a.value.chars().all(|c| c.is_ascii_digit()) || a.value == "unlimited");
                self.apply(rules::classify(
                    if set { "ulimit_set" } else { "ulimit" },
                    &args,
                ));
            }
            "umask" => {
                let set = !rules::operands(&args).is_empty();
                self.apply(rules::classify(
                    if set { "umask_set" } else { "umask" },
                    &args,
                ));
            }
            "export" | "declare" | "typeset" | "local" | "readonly" => self.declare(&base, &args),
            "unset" => self.unset(&args),
            "cd" | "pushd" => self.cd(&args),
            "popd" => {
                self.add(Risk::Mutating, "changes the directory stack");
                self.report.changes_session = true;
                self.opaque();
                self.cwd_unknown = true;
                self.vars.remove("OLDPWD");
            }
            _ => {
                let v = rules::classify(&base, &args);
                // `rustc --version`: an unlisted program run by name (or from
                // a system bin directory) asked only for its version or usage.
                // Local programs (`./x --help`, `/tmp/x --version`) stay unknown.
                if v.unlisted
                    && (!name.contains('/') || in_system_bin_dir(&name))
                    && rules::asks_version_or_help(&args)
                {
                    self.add(Risk::Safe, "prints version or usage");
                } else {
                    self.apply(v);
                }
            }
        }
        if let Some(i) = self.operation
            && self.report.operations[i].risk >= Risk::Mutating
            && matches!(
                base.as_str(),
                "cargo"
                    | "go"
                    | "npm"
                    | "pnpm"
                    | "yarn"
                    | "make"
                    | "gmake"
                    | "cmake"
                    | "ninja"
                    | "ctest"
                    | "pytest"
                    | "gradle"
                    | "mvn"
                    | "python"
                    | "python3"
                    | "node"
                    | "java"
                    | "dotnet"
                    | "tsc"
                    | "eslint"
                    | "pytest-3"
                    | "gradlew"
                    | "meson"
                    | "bazel"
                    | "sbt"
                    | "just"
                    | "task"
                    | "rake"
                    | "scons"
                    | "ant"
            )
        {
            self.report.operations[i].opaque = true;
        }
    }

    fn expand_alias(&mut self, alias: &str, args: Vec<Arg>) {
        match self.single_simple_argv(alias) {
            Some(mut argv) => {
                argv.extend(args);
                self.exec(argv, None);
            }
            None => {
                self.program_text(alias, false);
                if !args.is_empty() {
                    self.add(
                        Risk::Mutating,
                        "alias with a compound body receives arguments",
                    );
                }
            }
        }
    }

    /// argv of `text` if it is exactly one plain simple command.
    fn single_simple_argv(&mut self, text: &str) -> Option<Vec<Arg>> {
        let prog = self.parse(text).ok()?;
        let [cl] = prog.complete_commands.as_slice() else {
            return None;
        };
        let [ast::CompoundListItem(aol, ast::SeparatorOperator::Sequence)] = cl.0.as_slice() else {
            return None;
        };
        if !aol.additional.is_empty() || aol.first.seq.len() != 1 {
            return None;
        }
        let ast::Command::Simple(sc) = &aol.first.seq[0] else {
            return None;
        };
        if sc.prefix.is_some() {
            return None;
        }
        let mut argv = vec![self.word(sc.word_or_name.as_ref()?)];
        if let Some(suffix) = &sc.suffix {
            for item in &suffix.0 {
                match item {
                    ast::CommandPrefixOrSuffixItem::Word(w) => argv.push(self.word(w)),
                    _ => return None,
                }
            }
        }
        Some(argv)
    }

    fn exec_after_options(&mut self, args: &[Arg], with_value: &[&str]) {
        let rest = skip_options(args, with_value);
        if !rest.is_empty() {
            self.exec(rest, None);
        }
    }

    fn sudo(&mut self, args: &[Arg], name_end: Option<usize>) {
        self.add(
            Risk::Dangerous,
            "runs with root privileges (sudo, rewritten to sudo -n)",
        );
        let already_n = args
            .iter()
            .take_while(|a| a.value.starts_with('-'))
            .any(|a| {
                a.value == "-n"
                    || a.value == "--non-interactive"
                    || (!a.value.starts_with("--") && a.value.contains('n'))
            });
        match name_end {
            _ if self.replaying => {}
            Some(p) if !already_n => self.sudo_inserts.push(p),
            None if !already_n => self.add(
                Risk::Dangerous,
                "nested sudo cannot be rewritten to sudo -n",
            ),
            _ => {}
        }
        let mut i = 0;
        while i < args.len() {
            let v = args[i].value.as_str();
            match v {
                "--" => {
                    i += 1;
                    break;
                }
                "-u" | "-g" | "-h" | "-p" | "-C" | "-D" | "-r" | "-t" | "-T" | "-U" | "--user"
                | "--group" | "--host" | "--prompt" | "--close-from" | "--chdir" | "--role"
                | "--type" | "--command-timeout" | "--other-user" => i += 2,
                "-e" | "--edit" => {
                    self.add(Risk::Dangerous, "edits files as root (sudoedit)");
                    return;
                }
                "-s" | "-i" | "--shell" | "--login" if args.len() == i + 1 => {
                    self.add(Risk::Dangerous, "opens a root shell");
                    return;
                }
                x if x.starts_with('-') => i += 1,
                _ => break,
            }
        }
        if i < args.len() {
            self.exec(args[i..].to_vec(), None);
        }
    }

    fn env(&mut self, args: &[Arg]) {
        let readonly = std::mem::take(&mut self.readonly);
        let unknown = self.unknown_values.clone();
        let saved = (
            self.vars.clone(),
            self.exported.clone(),
            self.cwd.clone(),
            self.cwd_unknown,
        );
        self.env_inner(args);
        (self.vars, self.exported, self.cwd, self.cwd_unknown) = saved;
        self.readonly = readonly;
        self.unknown_values = unknown;
    }

    fn env_inner(&mut self, args: &[Arg]) {
        let mut i = 0;
        while i < args.len() {
            let v = args[i].value.clone();
            match v.as_str() {
                "-i" | "--ignore-environment" | "-" => {
                    self.opaque();
                    self.add(Risk::Mutating, "clears the child process environment");
                    self.vars.clear();
                    self.exported.clear();
                    i += 1;
                }
                "-0" | "--null" | "-v" | "--debug" => i += 1,
                "-u" | "--unset" => {
                    self.opaque();
                    self.add(Risk::Mutating, "removes a child process variable");
                    if let Some(name) = args.get(i + 1) {
                        self.vars.remove(&name.value);
                        self.exported.remove(&name.value);
                    }
                    i += 2;
                }
                "-C" | "--chdir" => {
                    self.opaque();
                    self.add(Risk::Mutating, "changes the child process directory");
                    if let Some(path) = args.get(i + 1).and_then(Arg::resolved) {
                        self.cwd = self.resolve(path);
                    } else {
                        self.cwd_unknown = true;
                    }
                    i += 2;
                }
                "-S" | "--split-string" => {
                    if let Some(s) = args.get(i + 1) {
                        if let Some(s) = s.resolved().filter(|_| !self.report.incomplete) {
                            let s = s.to_string();
                            self.program_text(&s, false);
                        } else {
                            self.opaque();
                            self.uncertain(
                                Risk::Dangerous,
                                "env -S with a string built at runtime",
                            );
                        }
                    }
                    return;
                }
                "--" => {
                    i += 1;
                    break;
                }
                x if x.starts_with('-') => i += 1,
                x if x.contains('=') && !x.starts_with('=') => {
                    let name = x.split('=').next().unwrap_or("");
                    let value = args[i]
                        .resolved()
                        .and_then(|s| s.split_once('='))
                        .map(|(_, value)| value.to_string());
                    self.variable(name, value.clone());
                    self.set_var(name, value.as_deref());
                    self.exported.insert(name.to_string());
                    self.add(Risk::Mutating, "sets a child process variable");
                    if matches!(name, "LD_PRELOAD" | "LD_AUDIT" | "BASH_ENV" | "ENV") {
                        self.add(
                            Risk::Dangerous,
                            format!("runs a command with {name} set (code injection)"),
                        );
                    }
                    i += 1;
                }
                _ => break,
            }
        }
        if i >= args.len() {
            self.add(Risk::Safe, "prints the environment");
        } else {
            self.exec(args[i..].to_vec(), None);
        }
    }

    fn xargs(&mut self, args: &[Arg]) {
        let with_value = [
            "-a",
            "--arg-file",
            "-d",
            "--delimiter",
            "-E",
            "-e",
            "-I",
            "-i",
            "-L",
            "-l",
            "-n",
            "--max-args",
            "-P",
            "--max-procs",
            "-s",
            "--max-chars",
            "--process-slot-var",
            "-j",
            "--jobs",
        ];
        let rest = skip_options(args, &with_value);
        if rest.is_empty() {
            self.add(Risk::Safe, "xargs echo");
            return;
        }
        let mut argv = rest;
        argv.push(Arg {
            value: "<stdin items>".into(),
            quoted: false,
            may_disappear: true,
            dynamic: true,
            glob: false,
            bound: false,
            known: None,
        });
        self.exec(argv, None);
    }

    fn find(&mut self, args: &[Arg]) {
        self.apply(rules::classify("find", args));
        // `find [-H|-L|-P|-D x|-Ox] [start…] [expression]`
        let mut i = 0;
        while i < args.len() && matches!(args[i].value.as_str(), "-H" | "-L" | "-P" | "-D") {
            i += 1 + usize::from(args[i].value == "-D");
        }
        while i < args.len() && args[i].value.starts_with("-O") {
            i += 1;
        }
        let starts: Vec<Target> = args[i..]
            .iter()
            .take_while(|a| !a.value.starts_with('-') && a.value != "(" && a.value != "!")
            .map(target_of)
            .collect();
        let starts = if starts.is_empty() {
            vec![target_of(&Arg::lit("."))]
        } else {
            starts
        };
        if args.iter().any(|a| a.value == "-delete") {
            let mut v = Verdict::new(Risk::Dangerous, "deletes matching files (find -delete)");
            v.recursive = true;
            v.deletes = true;
            self.add(v.risk, v.reason.clone());
            for t in &starts {
                self.write_effect(t, &v);
            }
        }
        // `{}` names files below the start directories: bound when those are
        // static workspace paths and symlinks are not followed out of them.
        let follows = args
            .iter()
            .any(|a| matches!(a.value.as_str(), "-L" | "-H" | "-follow"));
        let bound_starts = !follows
            && self.cwd_in_workspace()
            && starts.iter().all(|t| {
                !t.dynamic
                    && !t.glob
                    && classify_path_real(&self.resolve(&t.path), self.ctx, true).0
                        == PathClass::Workspace
            });
        let mut i = 0;
        while i < args.len() {
            match args[i].value.as_str() {
                "-exec" | "-execdir" | "-ok" | "-okdir" => {
                    let mut inner = Vec::new();
                    i += 1;
                    while i < args.len() && args[i].value != ";" && args[i].value != "+" {
                        let a = &args[i];
                        inner.push(if a.value.contains("{}") {
                            Arg {
                                value: a.value.clone(),
                                quoted: a.quoted,
                                may_disappear: false,
                                dynamic: true,
                                glob: false,
                                bound: bound_starts
                                    && !a.dynamic
                                    && stays_below(&a.value.replace("{}", "\u{1}")),
                                known: None,
                            }
                        } else {
                            a.clone()
                        });
                        i += 1;
                    }
                    self.exec(inner, None);
                }
                _ => {}
            }
            i += 1;
        }
    }

    fn shell_c(&mut self, base: &str, args: &[Arg]) {
        if base == "busybox" {
            if !args.is_empty() {
                self.exec(args.to_vec(), None);
            }
            return;
        }
        let c_idx = args.iter().position(|a| {
            let v = a.value.as_str();
            v == "-c"
                || (v.starts_with('-')
                    && !v.starts_with("--")
                    && v.len() > 1
                    && v[1..].chars().all(|c| c.is_ascii_alphabetic())
                    && v.contains('c'))
        });
        if let Some(ci) = c_idx {
            match args[ci + 1..].iter().find(|a| !rules::is_opt(a)) {
                Some(s) if s.dynamic => self.add(
                    Risk::Dangerous,
                    format!("{base} -c with a command string built at runtime"),
                ),
                Some(s) => {
                    let text = s.value.clone();
                    // A new process: exported variables only, and `$0`, `$1`…
                    // are the words after the command string.
                    let after = args
                        .iter()
                        .position(|a| std::ptr::eq(a, s))
                        .map_or(&[][..], |i| &args[i + 1..]);
                    let zero = after.first().and_then(|a| a.resolved()).map(str::to_string);
                    let positional = known_values(after.get(1..).unwrap_or_default());
                    let vars = self.exported_vars();
                    self.in_child(|a| {
                        a.vars = vars;
                        a.positional = Some(positional);
                        let outer = std::mem::replace(&mut a.script_path, zero);
                        a.program_text(&text, false);
                        a.script_path = outer;
                    });
                }
                None => {}
            }
            return;
        }
        let ops = rules::operands(args);
        match ops.first() {
            Some(script) if script.value != "-" => {
                self.add(
                    Risk::Mutating,
                    format!("runs shell script {}", script.value),
                );
                self.read_effect(&target_of(script));
                if !script.dynamic && SH_SYNTAX.contains(&base) {
                    let path = self.target_path(&script.value, true);
                    // Everything after the script is its arguments.
                    let after = args
                        .iter()
                        .position(|a| std::ptr::eq(a, *script))
                        .map_or(&[][..], |i| &args[i + 1..]);
                    self.script(&script.value, &path, true, after);
                }
            }
            _ => self.add(Risk::Mutating, format!("starts {base}")),
        }
    }

    fn declare(&mut self, base: &str, args: &[Arg]) {
        let flags: Vec<&str> = args
            .iter()
            .filter(|a| a.value.starts_with('-') || a.value.starts_with('+'))
            .map(|a| a.value.as_str())
            .collect();
        let assigns: Vec<&Arg> = args
            .iter()
            .filter(|a| !a.value.starts_with('-') && !a.value.starts_with('+'))
            .collect();
        if base == "readonly" && !assigns.is_empty() {
            self.add(Risk::Mutating, "marks variables read-only");
            self.report.changes_session = true;
        }
        if base == "export" && flags.iter().any(|f| f.contains('f') || f.contains('n')) {
            self.add(
                Risk::Mutating,
                "exports or un-exports shell functions/variables",
            );
            self.report.changes_session = true;
        }
        let mut any = false;
        // Arrays, namerefs and case or integer conversion: the value seen
        // here is not what the variable holds.
        let converts = flags
            .iter()
            .any(|f| f.starts_with('-') && f.contains(['a', 'A', 'n', 'l', 'u', 'c', 'i']));
        let functions = flags.iter().any(|f| f.contains('f'));
        let exports = !functions
            && match base {
                "export" => !flags.iter().any(|f| f.contains('n')),
                _ => flags.iter().any(|f| f.starts_with('-') && f.contains('x')),
            };
        let unexports = !functions
            && (base == "export" && flags.iter().any(|f| f.contains('n'))
                || flags.iter().any(|f| f.starts_with('+') && f.contains('x')));
        for a in &assigns {
            let raw_name = a.value.split('=').next().unwrap_or("");
            let name = raw_name.trim_end_matches('+');
            if a.value.contains('=') || exports || unexports {
                self.variable(
                    name,
                    a.resolved()
                        .and_then(|v| v.split_once('=').map(|(_, v)| v.to_string())),
                );
                self.add(Risk::Mutating, "changes session variables");
                self.report.changes_session = true;
            }
            if let Some((risk, why)) = rules::var_assignment_risk(name) {
                self.add(risk, why);
                self.report.changes_session = true;
                any = true;
            }
            if functions {
                continue;
            }
            if a.value.contains('=') {
                let mut value = a
                    .resolved()
                    .filter(|_| !a.glob && !converts)
                    .and_then(|r| r.split_once('='))
                    .map(|(_, v)| v.to_string());
                if raw_name.ends_with('+') {
                    value = value.and_then(|value| {
                        (!self.unknown_values.contains(name)).then(|| {
                            format!(
                                "{}{value}",
                                self.vars.get(name).map(String::as_str).unwrap_or("")
                            )
                        })
                    });
                }
                self.set_var(name, value.as_deref());
            }
            if exports {
                self.exported.insert(name.to_string());
            } else if unexports {
                self.exported.remove(name);
            }
            if base == "readonly"
                || flags
                    .iter()
                    .any(|flag| flag.starts_with('-') && flag.contains('r'))
            {
                self.readonly
                    .entry(name.to_string())
                    .or_insert_with(|| self.vars.get(name).cloned());
            }
        }
        if !any {
            self.add(Risk::Safe, format!("{base} (variables)"));
        }
    }

    fn unset(&mut self, args: &[Arg]) {
        if has_flag(args, &['f'], &[]) {
            self.add(Risk::Mutating, "removes shell functions");
            self.report.changes_session = true;
        }
        for a in rules::operands(args) {
            self.variable(&a.value, None);
            self.add(Risk::Mutating, "unsets a session variable");
            self.report.changes_session = true;
            if rules::KEY_VARS.contains(&a.value.as_str()) || a.dynamic {
                self.add(Risk::Mutating, format!("unsets key variable {}", a.value));
                self.report.changes_session = true;
            }
            self.exported.remove(&a.value);
            if !self.readonly.contains_key(&a.value) {
                self.unknown_values.remove(&a.value);
            }
        }
        self.add(Risk::Safe, "unset");
    }

    fn cd(&mut self, args: &[Arg]) {
        self.add(Risk::Mutating, "changes directory");
        self.report.changes_session = true;
        self.vars.remove("OLDPWD");
        match rules::operands(args).first() {
            None => {
                if let Some(home) = self
                    .known_expansion("$HOME")
                    .filter(|home| !home.is_empty())
                {
                    self.cwd = self.resolve(&home);
                } else {
                    self.cwd_unknown = true;
                }
            }
            Some(a) if a.dynamic || a.value == "-" => self.cwd_unknown = true,
            Some(a) => {
                self.cwd = if has_flag(args, &['P'], &[]) {
                    self.target_path(&a.value, true)
                } else {
                    self.resolve(&a.value)
                };
            }
        }
        if rules::operands(args).len() > 1
            || !self.cwd.is_dir()
            || self
                .vars
                .get("CDPATH")
                .is_some_and(|value| !value.is_empty())
            || args.iter().take_while(|arg| arg.value != "--").any(|arg| {
                arg.value.starts_with('-') && !matches!(arg.value.as_str(), "-L" | "-P" | "-")
            })
        {
            self.cwd_unknown = true;
        }
        if self.cwd_unknown {
            self.set_var("PWD", None);
        } else {
            let pwd = self.cwd.to_string_lossy().into_owned();
            self.set_var("PWD", Some(&pwd));
        }
        self.variable(
            "PWD",
            (!self.cwd_unknown).then(|| self.cwd.to_string_lossy().into_owned()),
        );
    }
}

fn target_of(a: &Arg) -> Target {
    Target {
        path: a.value.clone(),
        dynamic: a.dynamic,
        glob: a.glob,
        bound: a.bound,
        known: a.known.clone(),
    }
}

fn network_hosts(op: &Operation) -> Vec<String> {
    if op.known.iter().any(|known| !known) {
        return Vec::new();
    }
    let name = op.argv.first().map(|a| basename(a)).unwrap_or("");
    if matches!(name, "ping" | "ping6") {
        let mut hosts = Vec::new();
        let mut i = 1;
        while i < op.argv.len() {
            let word = &op.argv[i];
            if matches!(
                word.as_str(),
                "-c" | "-W" | "-w" | "-t" | "-I" | "-S" | "-s" | "-i" | "-l" | "-p" | "-m"
            ) {
                i += 2;
                continue;
            }
            if !word.starts_with('-') {
                hosts.push(word.clone());
            }
            i += 1;
        }
        return hosts;
    }
    if name == "curl" {
        let mut hosts = Vec::new();
        for arg in op.argv.iter().skip(1) {
            if matches!(
                arg.as_str(),
                "-s" | "-S"
                    | "-f"
                    | "-I"
                    | "-sS"
                    | "-fsS"
                    | "-sSf"
                    | "--silent"
                    | "--show-error"
                    | "--fail"
                    | "--head"
            ) {
                continue;
            }
            // Other URL forms and curl's configuration/redirect options stay
            // unresolved rather than being mistaken for a scoped destination.
            let Some(url) = arg
                .strip_prefix("https://")
                .or_else(|| arg.strip_prefix("http://"))
            else {
                return Vec::new();
            };
            let authority = url.split(['/', '?', '#']).next().unwrap_or("");
            let mut parts = authority.split(':');
            let host = parts.next().unwrap_or("");
            if host.is_empty()
                || !host
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
            {
                return Vec::new();
            }
            if let Some(port) = parts.next()
                && port.parse::<u16>().ok().filter(|p| *p != 0).is_none()
            {
                return Vec::new();
            }
            if parts.next().is_some() {
                return Vec::new();
            }
            hosts.push(host.to_ascii_lowercase());
        }
        return hosts;
    }
    Vec::new()
}

/// Skips leading options (consuming values of `with_value` options).
fn skip_options(args: &[Arg], with_value: &[&str]) -> Vec<Arg> {
    let mut i = 0;
    while i < args.len() {
        let v = args[i].value.as_str();
        if v == "--" {
            i += 1;
            break;
        }
        if !v.starts_with('-') || v == "-" || args[i].dynamic {
            break;
        }
        if with_value.contains(&v) {
            i += 2;
        } else {
            i += 1;
        }
    }
    args.get(i..).map(<[Arg]>::to_vec).unwrap_or_default()
}

/// Counts calls to `name` inside a function body (for fork-bomb detection).
fn count_calls(
    cc: &ast::CompoundCommand,
    name: &str,
    calls: &mut usize,
    async_calls: &mut bool,
    a: &Analyzer,
) {
    fn list(
        cl: &ast::CompoundList,
        name: &str,
        calls: &mut usize,
        async_calls: &mut bool,
        a: &Analyzer,
    ) {
        for ast::CompoundListItem(aol, sep) in &cl.0 {
            let is_async = matches!(sep, ast::SeparatorOperator::Async);
            let mut pipes = vec![&aol.first];
            for x in &aol.additional {
                match x {
                    ast::AndOr::And(p) | ast::AndOr::Or(p) => pipes.push(p),
                }
            }
            for p in pipes {
                for c in &p.seq {
                    match c {
                        ast::Command::Simple(sc) => {
                            if sc
                                .word_or_name
                                .as_ref()
                                .and_then(|w| a.static_word(w))
                                .as_deref()
                                == Some(name)
                            {
                                *calls += 1;
                                if is_async {
                                    *async_calls = true;
                                }
                            }
                        }
                        ast::Command::Compound(inner, _) => {
                            count_calls(inner, name, calls, async_calls, a)
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    match cc {
        ast::CompoundCommand::BraceGroup(b) => list(&b.list, name, calls, async_calls, a),
        ast::CompoundCommand::Subshell(s) => list(&s.list, name, calls, async_calls, a),
        ast::CompoundCommand::WhileClause(w) | ast::CompoundCommand::UntilClause(w) => {
            list(&w.0, name, calls, async_calls, a);
            list(&w.1.list, name, calls, async_calls, a);
        }
        ast::CompoundCommand::IfClause(i) => {
            list(&i.then, name, calls, async_calls, a);
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn param_names() {
        assert_eq!(param_name("$HOME"), Some("HOME"));
        assert_eq!(param_name("${HOME:-x}"), Some("HOME"));
        assert_eq!(param_name("$$"), Some("$"));
        assert_eq!(param_name("$1"), Some("1"));
    }

    #[test]
    fn ansi_c_decoding() {
        assert_eq!(decode_ansi_c(r"\x72\x6d"), "rm");
        assert_eq!(decode_ansi_c(r"a\nb"), "a\nb");
        assert_eq!(decode_ansi_c(r"\162\155"), "rm");
        assert!(has_escape_obfuscation(r"$'\x72'"));
        assert!(!has_escape_obfuscation(r"$'a\nb'"));
    }

    #[test]
    fn sudo_rewrite_positions() {
        assert_eq!(insert_sudo_n("sudo ls", &[4]), "sudo -n ls");
        assert_eq!(insert_sudo_n("中 && sudo ls", &[9]), "中 && sudo -n ls");
    }
}
