//! Tool definitions and the built-in read-only tools (design §5.5).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nosh_llm::{ToolCall, ToolSpec};
use nosh_shell::{CommandResult, EmbeddedShell, OutputState, UserOutput};
use serde_json::json;

/// Characters of tool output fed back per call (~1.5K tokens).
pub const OUTPUT_CHARS: usize = 6000;
pub const READ_FILE_LINES: usize = 400;
pub const GREP_MATCHES: usize = 200;
const GREP_ENTRIES: usize = 10_000;
const GREP_BYTES: usize = 64 * 1024 * 1024;
const GREP_READ_CHUNK: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSet {
    /// exec, read_file, grep.
    Full,
    /// Piped attachments: read_file and grep only.
    ReadOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BuiltinTool {
    Exec,
    ReadFile,
    Grep,
}

impl BuiltinTool {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::ReadFile => "read_file",
            Self::Grep => "grep",
        }
    }

    fn spec(self) -> ToolSpec {
        match self {
            Self::Exec => exec_spec(),
            Self::ReadFile => read_file_spec(),
            Self::Grep => grep_spec(),
        }
    }
}

impl ToolSet {
    pub(crate) fn tools(self) -> &'static [BuiltinTool] {
        match self {
            Self::Full => &[BuiltinTool::Exec, BuiltinTool::ReadFile, BuiltinTool::Grep],
            Self::ReadOnly => &[BuiltinTool::ReadFile, BuiltinTool::Grep],
        }
    }

    pub(crate) fn resolve(self, name: &str) -> Option<BuiltinTool> {
        self.tools().iter().copied().find(|t| t.name() == name)
    }
}

pub fn exec_spec() -> ToolSpec {
    ToolSpec {
        name: BuiltinTool::Exec.name().into(),
        description: "Run a command in the current shell session. Returns output and exit code."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "Shell code to execute."},
                "timeout_sec": {"type": "integer", "description": "Timeout in seconds (1-600; default: configured timeout)."}
            },
            "required": ["command"]
        }),
    }
}

pub fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: BuiltinTool::ReadFile.name().into(),
        description: "Read a text file with line numbers.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "File path."},
                "start_line": {"type": "integer", "description": "First line (1-based; default: 1)."},
                "end_line": {"type": "integer", "description": "Last line, inclusive. Omit for up to 400 lines from start_line."}
            },
            "required": ["path"]
        }),
    }
}

pub fn grep_spec() -> ToolSpec {
    ToolSpec {
        name: BuiltinTool::Grep.name().into(),
        description:
            "Search file contents recursively. Directory searches skip ignored and hidden files."
                .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "pattern": {"type": "string", "description": "Regular expression."},
                "path": {"type": "string", "description": "File or directory (default: current directory)."},
                "glob": {"type": "string", "description": "File filter, e.g. *.rs."}
            },
            "required": ["pattern"]
        }),
    }
}

pub fn specs(set: ToolSet) -> Vec<ToolSpec> {
    set.tools().iter().map(|t| t.spec()).collect()
}

pub(crate) fn format_user_output(output: &UserOutput) -> String {
    format_output_evidence(output, true)
}

/// CommandAssist already supplies command, cwd and exit in its execution record.
pub(crate) fn format_assist_output(output: &UserOutput) -> String {
    format_output_evidence(output, false)
}

fn format_output_evidence(output: &UserOutput, include_execution: bool) -> String {
    let (state, reason) = match output.state {
        OutputState::NotCaptured => ("not_captured", Some("capture_disabled")),
        OutputState::Unavailable(reason) => ("unavailable", Some(reason.reason())),
        OutputState::Captured => ("captured", None),
    };
    let mut metadata = json!({
        "command_id": output.command_id,
        "command": output.command,
        "execution_cwd": output.cwd,
        "exit": output.exit,
        "duration_ms": output.duration.as_millis(),
        "source": if output.terminal_source { "terminal" } else { "none" },
        "state": state,
        "reason": reason,
        "observed_bytes": output.observed_bytes,
        "retained_bytes": output.text.len(),
        "truncated": output.truncated,
        "incomplete": output.incomplete,
        "mixed": output.mixed,
        "command_truncated": output.command_truncated,
        "cwd_truncated": output.cwd_truncated,
    });
    if !include_execution {
        let fields = metadata
            .as_object_mut()
            .expect("output metadata is an object");
        for key in ["command", "execution_cwd", "exit"] {
            let _ = fields.shift_remove(key);
        }
    }
    let mut result = format!("[user_output {metadata}]\n");
    if output.has_body() {
        if output.text.is_empty() {
            result.push_str(if output.observed_bytes == Some(0) {
                "(Capture succeeded: no terminal output.)"
            } else {
                "(Terminal bytes were captured, but no text remained after display cleanup.)"
            });
        } else {
            result.push_str(&output.text);
        }
    } else if output.mixed {
        result.push_str(
            "(Known concurrent output: content omitted; do not attribute it to this command.)",
        );
    } else {
        result.push_str("(No captured output is available. Do not invent error text.)");
    }
    result.push_str("\n[/user_output]");
    result
}

/// Internal implementation for the planned `get_last_output` model tool.
///
/// This function is intentionally absent from [`BuiltinTool`] and every
/// [`ToolSet`], so the model cannot call it until its policy is finalized.
#[allow(dead_code)]
pub(crate) fn get_last_output(shell: &EmbeddedShell) -> String {
    match shell.last_user_output() {
        Some(output) => format_user_output(output),
        None => {
            let metadata = json!({
                "state": "unavailable",
                "reason": "no_completed_user_command",
            });
            format!(
                "[user_output {metadata}]\n\
                 (No completed user command is available.)\n\
                 [/user_output]"
            )
        }
    }
}

/// Keeps the first 60% and last 40% of `s` within `max` characters.
pub fn truncate_middle(s: &str, max: usize) -> (String, bool) {
    let (text, truncated) = truncate_counted(s, max, s.chars().count());
    (text.into_owned(), truncated)
}

fn truncate_counted(s: &str, max: usize, n: usize) -> (Cow<'_, str>, bool) {
    if n <= max {
        return (Cow::Borrowed(s), false);
    }
    let head = max * 6 / 10;
    let tail = max - head;
    let head_end = s.char_indices().nth(head).map_or(s.len(), |(i, _)| i);
    let tail_start = s
        .char_indices()
        .rev()
        .take(tail)
        .last()
        .map_or(s.len(), |(i, _)| i);
    let omitted = n - max;
    let mut out = String::with_capacity(head_end + s.len() - tail_start + 64);
    out.push_str(&s[..head_end]);
    let _ = write!(out, "\n[… {omitted} characters omitted …]\n");
    out.push_str(&s[tail_start..]);
    (Cow::Owned(out), true)
}

fn secs(d: std::time::Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// Tool result text for `exec` (plain header + raw output).
pub fn format_command_result(r: &CommandResult, full_log: Option<&Path>) -> String {
    let body_budget = OUTPUT_CHARS;
    let out_len = r.stdout.chars().count();
    let err_len = r.stderr.chars().count();
    let (out, err, truncated) = if out_len + err_len <= body_budget {
        (
            Cow::Borrowed(r.stdout.as_str()),
            Cow::Borrowed(r.stderr.as_str()),
            r.truncated,
        )
    } else {
        // Give stderr up to a third of the budget, stdout the rest.
        let err_budget = err_len.min(body_budget / 3);
        let out_budget = body_budget - err_budget;
        let (o, _) = truncate_counted(&r.stdout, out_budget, out_len);
        let (e, _) = truncate_counted(&r.stderr, err_budget.max(1), err_len);
        (o, e, true)
    };
    let mut s = format!(
        "[exit_code={} duration={} truncated={}",
        r.exit_code,
        secs(r.duration),
        if truncated { "yes" } else { "no" }
    );
    if r.timed_out {
        s.push_str(" timed_out=yes");
    }
    if r.interrupted {
        s.push_str(" interrupted=yes");
    }
    s.push_str("]\n");
    if !r.diff.is_empty() {
        let _ = writeln!(s, "[state] {}", r.diff.describe());
    }
    if r.needed_terminal {
        s.push_str("[note] the command needs a terminal and was stopped; handed back to the user for review, not automatically retried\n");
    }
    if r.timed_out {
        s.push_str("[note] the command timed out and was stopped\n");
    }
    if r.interrupted {
        s.push_str("[note] the user interrupted the command (Ctrl-C)\n");
    }
    if truncated && let Some(p) = full_log {
        let _ = writeln!(s, "[full output: {}]", p.display());
    }
    s.push_str("--- stdout ---\n");
    s.push_str(if out.is_empty() { "(empty)\n" } else { &out });
    if !out.is_empty() && !out.ends_with('\n') {
        s.push('\n');
    }
    s.push_str("--- stderr ---\n");
    s.push_str(if err.is_empty() {
        "(empty)"
    } else {
        err.trim_end()
    });
    s
}

/// Filters text on its way to disk (full command output under
/// `state/outputs/`, and later history and audit logs). The local agent is
/// trusted, so nosh uses [`NoRedact`]; a remote agent can supply its own
/// rules through [`crate::Agent::with_redactor`].
pub trait Redactor: Send + Sync {
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str>;
}

/// Writes text unchanged, without copying it.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoRedact;

impl Redactor for NoRedact {
    fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
        Cow::Borrowed(text)
    }
}

/// Saves the full output of a truncated command under `state/outputs/`.
pub fn save_output(
    id: usize,
    command: &str,
    r: &CommandResult,
    redactor: &dyn Redactor,
) -> Option<PathBuf> {
    save_output_in(
        &nosh_hub::paths::state_dir().join("outputs"),
        id,
        command,
        r,
        redactor,
    )
}

fn save_output_in(
    dir: &Path,
    id: usize,
    command: &str,
    r: &CommandResult,
    redactor: &dyn Redactor,
) -> Option<PathBuf> {
    nosh_hub::paths::ensure_private_dir(dir).ok()?;
    let pid = std::process::id();
    let path = dir.join(format!("{pid}-{id}.log"));
    let text = format!(
        "$ {command}\n[exit_code={}]\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
        r.exit_code, r.stdout, r.stderr
    );
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    use std::io::Write;
    let mut f = opts.open(&path).ok()?;
    f.write_all(redactor.redact(&text).as_bytes()).ok()?;
    Some(path)
}

/// `~`-expanded, absolute and lexically normalized (`a/../b` → `b`), so the
/// path that is checked is the path that is opened.
fn resolve(cwd: &Path, p: &str) -> PathBuf {
    let expanded = if p == "~" || p.starts_with("~/") {
        match std::env::var_os("HOME") {
            Some(h) => PathBuf::from(h).join(p.trim_start_matches('~').trim_start_matches('/')),
            None => PathBuf::from(p),
        }
    } else {
        PathBuf::from(p)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut out = PathBuf::from("/");
    for c in abs.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::Normal(n) => out.push(n),
            _ => {}
        }
    }
    out
}

pub fn tool_path(call: &ToolCall, cwd: &Path) -> PathBuf {
    resolve(cwd, call.str_arg("path").unwrap_or("."))
}

pub(crate) fn prepare_read(
    call: &ToolCall,
    ctx: &nosh_permissions::Context,
) -> Result<ToolCall, String> {
    let path = match call.args.get("path") {
        Some(serde_json::Value::String(path)) if !path.is_empty() => path.as_str(),
        None if call.name == "grep" => ".",
        _ => return Err("missing or invalid parameter 'path'".into()),
    };
    let mut prepared = call.clone();
    prepared
        .args
        .insert("path".into(), json!(ctx.resolve(path).to_string_lossy()));
    Ok(prepared)
}

struct GrepBudget<'a> {
    cancelled: &'a dyn Fn() -> bool,
    started: Instant,
    timeout: Duration,
    entries_left: usize,
    bytes_left: usize,
    truncated: Option<&'static str>,
}

impl<'a> GrepBudget<'a> {
    fn new(timeout: Duration, cancelled: &'a dyn Fn() -> bool) -> Self {
        Self {
            cancelled,
            started: Instant::now(),
            timeout,
            entries_left: GREP_ENTRIES,
            bytes_left: GREP_BYTES,
            truncated: None,
        }
    }

    fn check(&mut self) -> Result<bool, String> {
        if (self.cancelled)() {
            return Err("grep cancelled by the user".into());
        }
        if self.truncated.is_none() && self.started.elapsed() >= self.timeout {
            self.truncated = Some("time limit reached");
        }
        Ok(self.truncated.is_none())
    }
}

struct GrepReader<'a, 'b, R> {
    inner: R,
    budget: &'a mut GrepBudget<'b>,
}

impl<R: Read> Read for GrepReader<'_, '_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        self.budget.check().map_err(io::Error::other)?;
        if self.budget.bytes_left == 0 {
            self.budget.truncated.get_or_insert("byte limit reached");
        }
        if let Some(reason) = self.budget.truncated {
            // A limit is not EOF: a cut-off line must not become a match.
            return Err(io::Error::other(reason));
        }
        let len = buf.len().min(self.budget.bytes_left).min(GREP_READ_CHUNK);
        let read = self.inner.read(&mut buf[..len])?;
        self.budget.bytes_left -= read;
        Ok(read)
    }
}

/// Searches contents; ignore-rule loading is metadata, directory/content reads are authorized.
pub fn grep(
    call: &ToolCall,
    cwd: &Path,
    timeout: Duration,
    cancelled: &dyn Fn() -> bool,
    authorize: impl FnMut(&Path) -> Result<(), String>,
) -> Result<String, String> {
    grep_with_budget(
        call,
        cwd,
        &mut GrepBudget::new(timeout, cancelled),
        authorize,
    )
}

fn grep_with_budget(
    call: &ToolCall,
    cwd: &Path,
    scan: &mut GrepBudget<'_>,
    mut authorize: impl FnMut(&Path) -> Result<(), String>,
) -> Result<String, String> {
    use grep_searcher::{BinaryDetection, SearcherBuilder, Sink, SinkMatch};

    if !scan.check()? {
        return Ok(format_grep_result(0, "", scan.truncated));
    }
    let pattern = call
        .str_arg("pattern")
        .ok_or("missing required parameter 'pattern'")?;
    let matcher =
        grep_regex::RegexMatcher::new(pattern).map_err(|e| format!("invalid regex: {e}"))?;
    let root = tool_path(call, cwd);
    let meta = std::fs::metadata(&root).map_err(|e| format!("{}: {e}", root.display()))?;
    if !meta.is_file() && !meta.is_dir() {
        return Err(format!(
            "{}: not a regular file or directory",
            root.display()
        ));
    }
    if meta.is_dir()
        && std::fs::symlink_metadata(&root)
            .map_err(|e| e.to_string())?
            .is_symlink()
    {
        return Err(format!(
            "{}: directory symlinks are not followed",
            root.display()
        ));
    }
    let base = if meta.is_dir() {
        root.as_path()
    } else {
        root.parent().unwrap_or(cwd)
    };
    // Filtering separately keeps positive globs from overriding ignore/hidden rules.
    let glob = call
        .str_arg("glob")
        .map(|glob| {
            let mut builder = ignore::overrides::OverrideBuilder::new(base);
            builder
                .add(glob)
                .map_err(|e| format!("invalid glob: {e}"))?;
            builder.build().map_err(|e| format!("invalid glob: {e}"))
        })
        .transpose()?;
    let mut ignores = if meta.is_dir() {
        authorize(&root)?;
        if !scan.check()? {
            return Ok(format_grep_result(0, "", scan.truncated));
        }
        let mut builder = ignore::WalkBuilder::new(&root);
        builder.hidden(true).follow_links(false).require_git(false);
        Some(
            builder
                .build_matchers()
                .pop()
                .expect("one configured grep root"),
        )
    } else {
        None
    };

    struct Matches<'a> {
        path: &'a Path,
        text: String,
        count: usize,
        remaining: usize,
        budget: usize,
        truncated: bool,
    }
    impl Sink for Matches<'_> {
        type Error = std::io::Error;

        fn matched(
            &mut self,
            _: &grep_searcher::Searcher,
            m: &SinkMatch<'_>,
        ) -> Result<bool, Self::Error> {
            if self.count >= self.remaining {
                self.truncated = true;
                return Ok(false);
            }
            let line_number = m
                .line_number()
                .ok_or_else(|| std::io::Error::other("grep line number is unavailable"))?;
            let line = String::from_utf8_lossy(m.bytes());
            let prefix = format!("{}:{line_number}:", self.path.display());
            let available = self.budget.saturating_sub(self.text.chars().count());
            let mut rendered = prefix
                .chars()
                .chain(line.trim_end_matches(['\r', '\n']).chars())
                .peekable();
            self.text
                .extend(rendered.by_ref().take(available.saturating_sub(1)));
            self.text.push('\n');
            self.count += 1;
            if rendered.peek().is_some() {
                self.truncated = true;
                return Ok(false);
            }
            Ok(true)
        }

        fn binary_data(
            &mut self,
            _: &grep_searcher::Searcher,
            _: u64,
        ) -> Result<bool, Self::Error> {
            self.text.clear();
            self.count = 0;
            self.truncated = false;
            Ok(false)
        }
    }
    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(0))
        .heap_limit(Some(8 * 1024 * 1024))
        .build();
    let mut output = String::new();
    let mut count = 0;
    let budget = OUTPUT_CHARS - 120;
    let mut pending = vec![root.clone()];
    'scan: while let Some(path) = pending.pop() {
        if !scan.check()? {
            break;
        }
        let metadata = std::fs::symlink_metadata(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        if path != root {
            if !metadata.is_dir() && !metadata.is_file() {
                continue;
            }
            if let Some(ignores) = &mut ignores {
                // Use the library's cached ignore metadata before deciding whether to descend.
                let relative = path
                    .strip_prefix(&root)
                    .expect("grep descendants stay under root");
                let (matched, error) = ignores.matched_with_errors(relative, metadata.is_dir());
                if let Some(error) = error {
                    return Err(format!("grep ignore metadata: {error}"));
                }
                if matched.is_ignore() {
                    continue;
                }
            }
        }
        if metadata.is_dir() {
            if path != root {
                authorize(&path)?;
            }
            if !scan.check()? {
                break;
            }
            let mut entries =
                std::fs::read_dir(&path).map_err(|error| format!("{}: {error}", path.display()))?;
            let mut children = Vec::new();
            loop {
                if !scan.check()? {
                    break 'scan;
                }
                if scan.entries_left == 0 {
                    scan.truncated = Some("entry limit reached");
                    break 'scan;
                }
                let Some(entry) = entries.next() else {
                    break;
                };
                let entry = entry.map_err(|error| format!("{}: {error}", path.display()))?;
                scan.entries_left -= 1;
                children.push(entry.path());
            }
            children.sort();
            pending.extend(children.into_iter().rev());
            continue;
        }
        if !metadata.is_file() && !(path == root && meta.is_file()) {
            continue;
        }
        if glob
            .as_ref()
            .is_some_and(|g| g.matched(&path, false).is_ignore())
        {
            continue;
        }
        authorize(&path)?;
        if !scan.check()? {
            break;
        }
        let mut matches = Matches {
            path: path.strip_prefix(base).unwrap_or(&path),
            text: String::new(),
            count: 0,
            remaining: GREP_MATCHES - count,
            budget: budget.saturating_sub(output.chars().count()),
            truncated: false,
        };
        let file =
            std::fs::File::open(&path).map_err(|error| format!("{}: {error}", path.display()))?;
        let result = searcher.search_reader(
            &matcher,
            GrepReader {
                inner: file,
                budget: scan,
            },
            &mut matches,
        );
        if let Err(error) = result
            && scan.truncated.is_none()
        {
            return Err(format!("{}: {error}", path.display()));
        }
        output.push_str(&matches.text);
        count += matches.count;
        if matches.truncated {
            scan.truncated.get_or_insert("match/output limit reached");
        }
        if !scan.check()? {
            break;
        }
    }
    scan.check()?;
    Ok(format_grep_result(count, &output, scan.truncated))
}

fn format_grep_result(count: usize, output: &str, truncated: Option<&str>) -> String {
    let mut result = format!(
        "[{count} matching lines; truncated={}]\n",
        if truncated.is_some() { "yes" } else { "no" }
    );
    if count == 0 {
        result.push_str(if truncated.is_some() {
            "(no matches in the scanned portion; search incomplete)"
        } else {
            "(no matches)"
        });
    } else {
        result.push_str(output.trim_end());
    }
    if let Some(reason) = truncated {
        let _ = write!(
            result,
            "\n[truncated: {reason}; refine pattern, path or glob]"
        );
    }
    result
}

/// `read_file`: numbered lines, binary files refused.
pub fn read_file(call: &ToolCall, cwd: &Path) -> Result<String, String> {
    read_file_with(call, cwd, COUNT_BUDGET)
}

/// Bytes kept per displayed line (at most 400 characters are shown).
const LINE_BYTES: usize = 2048;
/// Bytes read past the returned lines to count the file's total.
const COUNT_BUDGET: u64 = 64 * 1024 * 1024;

/// Streams the file: skips to `start_line` however far it is, keeps only the
/// returned lines (bounded in count, per-line bytes and total characters),
/// then counts the remaining lines within `count_budget` bytes.
fn read_file_with(call: &ToolCall, cwd: &Path, count_budget: u64) -> Result<String, String> {
    let path = tool_path(call, cwd);
    let err = |e: std::io::Error| format!("{}: {e}", path.display());
    let meta = std::fs::metadata(&path).map_err(err)?;
    if meta.is_dir() {
        return Err(format!(
            "{} is a directory; read_file requires a text file. Use grep to search file contents under a directory.",
            path.display()
        ));
    }
    if !meta.is_file() {
        return Err(format!("{} is not a regular text file", path.display()));
    }
    let start = call.int_arg("start_line").unwrap_or(1).max(1) as usize;
    let end_req = call
        .int_arg("end_line")
        .map(|e| e.max(1) as usize)
        .unwrap_or(start + READ_FILE_LINES - 1);
    if end_req < start {
        return Err(format!("end_line {end_req} is before start_line {start}"));
    }
    let last = end_req.min(start + READ_FILE_LINES - 1);
    let mut f = std::fs::File::open(&path).map_err(err)?;
    let mut sniff = Vec::with_capacity(8192);
    f.by_ref().take(8192).read_to_end(&mut sniff).map_err(err)?;
    if sniff.contains(&0) {
        return Ok(format!("[binary file, {} bytes; not shown]", meta.len()));
    }
    let mut r = std::io::BufReader::with_capacity(64 * 1024, std::io::Cursor::new(sniff).chain(f));

    // Room for the entries within OUTPUT_CHARS once header and footer are added.
    let room = OUTPUT_CHARS.saturating_sub(path.as_os_str().len() + 120);
    let mut shown: Vec<String> = Vec::new();
    let mut shown_chars = 0usize;
    let mut collecting = true;
    let mut line = 1usize;
    let mut cur: Vec<u8> = Vec::new();
    let mut cur_cut = false;
    // Bytes since the last newline (a final line without one still counts).
    let mut pending = false;
    let mut after = 0u64;
    let mut eof = false;
    let mut finish = |line: usize, cur: &mut Vec<u8>, cut: bool, shown: &mut Vec<String>| {
        let s = String::from_utf8_lossy(cur);
        let s = s.strip_suffix('\r').unwrap_or(&s);
        let text: String = if cut || s.chars().count() > 400 {
            s.chars().take(400).chain("…".chars()).collect()
        } else {
            s.to_string()
        };
        cur.clear();
        let entry = format!("{line:>5}  {text}\n");
        if shown_chars + entry.len() > room && !shown.is_empty() {
            return false;
        }
        shown_chars += entry.len();
        shown.push(entry);
        true
    };
    loop {
        let buf = r.fill_buf().map_err(err)?;
        if buf.is_empty() {
            eof = true;
            break;
        }
        let n = buf.len();
        let mut i = 0;
        while i < n {
            let want = collecting && line >= start;
            let nl = memchr::memchr(b'\n', &buf[i..]);
            let piece = &buf[i..nl.map_or(n, |p| i + p)];
            if want {
                let keep = piece.len().min(LINE_BYTES.saturating_sub(cur.len()));
                cur.extend_from_slice(&piece[..keep]);
                cur_cut |= keep < piece.len();
            }
            match nl {
                Some(p) => {
                    if want {
                        collecting = finish(line, &mut cur, cur_cut, &mut shown) && line < last;
                        cur_cut = false;
                    }
                    line += 1;
                    pending = false;
                    i += p + 1;
                }
                None => {
                    pending = true;
                    i = n;
                }
            }
        }
        r.consume(n);
        if !collecting {
            after += n as u64;
            if after > count_budget {
                break;
            }
        }
    }
    if eof && pending && collecting && line >= start {
        finish(line, &mut cur, cur_cut, &mut shown);
    }
    // Complete lines seen, plus a final line without a newline.
    let counted = line - 1 + usize::from(pending);
    let total = if eof {
        format!("{counted} lines")
    } else {
        format!("≥ {counted} lines (stopped counting)")
    };
    let mut out = format!("[{} · {total}]\n", path.display());
    if eof && counted == 0 {
        out.push_str("(empty file)");
        return Ok(out);
    }
    if eof && start > counted {
        return Err(format!(
            "start_line {start} is past the end ({counted} lines)"
        ));
    }
    let shown_to = start - 1 + shown.len();
    for e in &shown {
        out.push_str(e);
    }
    if !eof || shown_to < counted {
        let _ = write!(
            out,
            "[showing lines {start}-{shown_to}; continue with start_line={}]",
            shown_to + 1
        );
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;
    use std::cell::Cell;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            name: name.into(),
            args: args.as_object().cloned().unwrap(),
        }
    }

    fn grep(
        call: &ToolCall,
        cwd: &Path,
        authorize: impl FnMut(&Path) -> Result<(), String>,
    ) -> Result<String, String> {
        super::grep(call, cwd, Duration::from_secs(60), &|| false, authorize)
    }

    #[test]
    fn tool_catalog_matches_the_advertised_schema_and_order() {
        let command = exec_spec();
        assert!(command.description.contains("current shell session"));
        assert!(!command.description.contains("Bash"));
        for (set, names) in [
            (ToolSet::Full, vec!["exec", "read_file", "grep"]),
            (ToolSet::ReadOnly, vec!["read_file", "grep"]),
        ] {
            let advertised = specs(set);
            assert_eq!(
                advertised
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>(),
                names
            );
            for spec in advertised {
                let tool = set.resolve(&spec.name).unwrap();
                assert_eq!(tool.spec(), spec);
            }

            for name in [
                "exec",
                "run_command",
                "read_file",
                "grep",
                "list_dir",
                "get_last_output",
                "search",
                "search_text",
                "propose_command",
                "READ_FILE",
                "",
            ] {
                assert_eq!(set.resolve(name).is_some(), names.contains(&name));
            }
        }
    }

    #[test]
    fn model_visible_tools_have_described_parameters_and_unambiguous_examples() {
        let mut specs = specs(ToolSet::Full);
        specs.push(crate::command_help::spec());
        specs.push(crate::user_input::spec());
        let rendered = nosh_llm::template::tool_definitions(&specs);
        for spec in &specs {
            assert!(!spec.description.trim().is_empty(), "{}", spec.name);
            for (name, parameter) in spec.parameters["properties"].as_object().unwrap() {
                assert!(
                    parameter["description"]
                        .as_str()
                        .is_some_and(|text| !text.trim().is_empty()),
                    "{}.{name} has no explanation",
                    spec.name
                );
            }
            assert!(rendered.contains(&spec.name));
            assert!(rendered.contains(&spec.description));
        }
        assert!(!rendered.contains("command_info"));
        assert!(!rendered.contains("\"topic\""));
        assert!(!rendered.contains("default 60"));
        let help = crate::command_help::spec();
        assert_eq!(help.parameters["required"], json!(["name"]));
        assert!(help.parameters["properties"]["query"].get("enum").is_none());
        for (raw, name, args) in [
            (
                "<function name=\"command_help\"><param name=\"name\">tar</param><param name=\"query\">gzip</param></function>",
                "command_help",
                json!({"name":"tar","query":"gzip"}),
            ),
            (
                "<function name=\"command_help\"><param name=\"name\">tar</param></function>",
                "command_help",
                json!({"name":"tar"}),
            ),
            (
                "<function name=\"command_help\"><param name=\"name\">git commit</param><param name=\"query\">--amend</param></function>",
                "command_help",
                json!({"name":"git commit","query":"--amend"}),
            ),
            (
                "<function name=\"read_file\"><param name=\"path\">src/main.rs</param><param name=\"start_line\">10</param><param name=\"end_line\">20</param></function>",
                "read_file",
                json!({"path":"src/main.rs","start_line":10,"end_line":20}),
            ),
            (
                "<function name=\"grep\"><param name=\"pattern\">(?i)todo</param><param name=\"glob\">*.rs</param></function>",
                "grep",
                json!({"pattern":"(?i)todo","glob":"*.rs"}),
            ),
            (
                "<function name=\"ask_user\"><param name=\"question\">Format?</param><param name=\"choices\">[\"tar.gz\",\"zip\"]</param></function>",
                "ask_user",
                json!({"question":"Format?","choices":["tar.gz","zip"]}),
            ),
            (
                "<function name=\"exec\"><param name=\"command\">pwd</param><param name=\"timeout_sec\">5</param></function>",
                "exec",
                json!({"command":"pwd","timeout_sec":5}),
            ),
        ] {
            assert_eq!(
                nosh_llm::toolcall::parse_call(raw, &specs).unwrap(),
                call(name, args)
            );
        }
    }

    #[test]
    fn grep_case_flags_and_explicit_file_scope_match_the_descriptions() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".hidden"), "TODO\n").unwrap();
        std::fs::write(dir.path().join("notes"), "TODO\ntodo\n").unwrap();
        let exact = grep(&call("grep", json!({"pattern":"todo"})), dir.path(), |_| {
            Ok(())
        })
        .unwrap();
        assert!(exact.contains("[1 matching lines;"));
        assert!(exact.contains("notes:2:todo"));
        let insensitive = grep(
            &call("grep", json!({"pattern":"(?i)todo"})),
            dir.path(),
            |_| Ok(()),
        )
        .unwrap();
        assert!(insensitive.contains("[2 matching lines;"));
        assert!(!insensitive.contains(".hidden"));
        let explicit = grep(
            &call("grep", json!({"pattern":"TODO","path":".hidden"})),
            dir.path(),
            |_| Ok(()),
        )
        .unwrap();
        assert!(explicit.contains(".hidden:1:TODO"));
    }

    #[test]
    fn read_file_rejects_a_fifo_without_waiting_for_a_writer() {
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("pipe");
        let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        // SAFETY: name is a NUL-terminated path in this test's temporary directory.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let call = ToolCall {
            name: "read_file".into(),
            args: json!({"path":"pipe"}).as_object().unwrap().clone(),
        };
        assert!(
            super::read_file(&call, directory.path())
                .unwrap_err()
                .contains("not a regular text file")
        );
    }

    #[test]
    fn directory_read_error_only_suggests_a_shared_read_only_tool() {
        let directory = tempfile::tempdir().unwrap();
        let call = ToolCall {
            name: "read_file".into(),
            args: json!({"path": "."}).as_object().unwrap().clone(),
        };
        let error = read_file(&call, directory.path()).unwrap_err();
        assert!(error.contains("is a directory; read_file requires a text file"));
        assert!(error.contains("Use grep to search file contents"));
        assert!(!error.contains("exec"));
        assert!(!error.contains("Use ls"));
    }

    #[test]
    fn last_output_accessor_is_implemented_but_not_registered() {
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let empty = get_last_output(&shell);
        assert!(empty.contains("\"reason\":\"no_completed_user_command\""));
        assert!(
            !ToolSet::Full
                .tools()
                .iter()
                .any(|tool| tool.name() == "get_last_output")
        );
        assert!(ToolSet::Full.resolve("get_last_output").is_none());
        assert!(
            specs(ToolSet::Full)
                .iter()
                .all(|spec| spec.name != "get_last_output")
        );

        assert_eq!(shell.run_user_line("true").exit_code, 0);
        let disabled = get_last_output(&shell);
        assert!(disabled.contains("\"state\":\"not_captured\""));
        assert!(disabled.contains("\"command\":\"true\""));

        let mut output = UserOutput {
            command_id: 7,
            command: "cargo build".into(),
            cwd: "/work/app".into(),
            command_truncated: false,
            cwd_truncated: false,
            exit: 101,
            duration: std::time::Duration::from_millis(25),
            state: OutputState::Captured,
            terminal_source: true,
            text: "actual error\n".into(),
            observed_bytes: Some(13),
            truncated: false,
            incomplete: false,
            mixed: false,
        };
        let captured = format_user_output(&output);
        assert!(captured.contains("\"command_id\":7"));
        assert!(captured.contains("actual error"));
        output.mixed = true;
        assert!(!format_user_output(&output).contains("actual error"));
    }

    #[test]
    fn compact_assist_output_preserves_capture_quality_without_repeating_execution() {
        let mut output = UserOutput {
            command_id: 7,
            command: "cargo build".into(),
            cwd: "/work/app".into(),
            command_truncated: false,
            cwd_truncated: false,
            exit: 101,
            duration: Duration::from_millis(25),
            state: OutputState::Captured,
            terminal_source: true,
            text: "actual error\n".into(),
            observed_bytes: Some(13),
            truncated: false,
            incomplete: false,
            mixed: false,
        };
        for state in [
            OutputState::Captured,
            OutputState::NotCaptured,
            OutputState::Unavailable(nosh_shell::OutputUnavailable::FullScreen),
        ] {
            for mixed in [false, true] {
                output.state = state;
                output.mixed = mixed;
                output.truncated = true;
                output.incomplete = true;
                let full = format_user_output(&output);
                let compact = format_assist_output(&output);
                let (full_header, full_body) = full.split_once('\n').unwrap();
                let (header, body) = compact.split_once('\n').unwrap();
                assert_eq!(body, full_body);
                let parse = |header: &str| {
                    serde_json::from_str::<serde_json::Value>(
                        header
                            .strip_prefix("[user_output ")
                            .unwrap()
                            .strip_suffix(']')
                            .unwrap(),
                    )
                    .unwrap()
                };
                let mut expected = parse(full_header);
                for field in ["command", "execution_cwd", "exit"] {
                    let _ = expected.as_object_mut().unwrap().remove(field);
                }
                assert_eq!(parse(header), expected);
                assert!(header.contains("\"command_id\":7"));
                assert!(header.contains("\"truncated\":true"));
                assert!(header.contains("\"incomplete\":true"));
                assert!(!header.contains("\"command\":"));
                assert!(!header.contains("\"execution_cwd\":"));
                assert!(!header.contains("\"exit\":"));
                assert_eq!(compact.contains("actual error"), output.has_body());
            }
        }
        output.state = OutputState::Captured;
        output.mixed = false;
        output.truncated = false;
        output.incomplete = false;
        output.text.clear();
        output.observed_bytes = Some(0);
        assert!(format_assist_output(&output).contains("Capture succeeded: no terminal output"));
        output.observed_bytes = Some(13);
        assert!(format_assist_output(&output).contains("no text remained after display cleanup"));
    }

    #[test]
    fn grep_returns_numbered_content_matches_with_ignore_and_glob_rules() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        for (path, contents) in [
            ("a.rs", "not here\nneedle one\n"),
            ("src/b.rs", "needle two\n"),
            ("c.txt", "needle three\n"),
            (".hidden.rs", "needle\n"),
            ("ignored.rs", "needle\n"),
            ("binary.rs", "needle\0binary\n"),
            (".gitignore", "ignored.rs\n"),
            ("needle-filename.rs", "different contents\n"),
        ] {
            std::fs::write(root.join(path), contents).unwrap();
        }
        let result = grep(
            &call("grep", json!({"pattern": "needle", "glob": "*.rs"})),
            root,
            |_| Ok(()),
        )
        .unwrap();
        assert!(
            result.starts_with("[2 matching lines; truncated=no]"),
            "{result}"
        );
        assert!(result.contains("a.rs:2:needle one"), "{result}");
        assert!(
            result.contains(&format!(
                "{}:1:needle two",
                Path::new("src").join("b.rs").display()
            )),
            "{result}"
        );
        for excluded in ["c.txt", ".hidden", "ignored", "binary", "needle-filename"] {
            assert!(!result.contains(excluded), "{result}");
        }
        let all = grep(
            &call("grep", json!({"pattern": "needle"})),
            root,
            |_| Ok(()),
        )
        .unwrap();
        assert!(all.starts_with("[3 matching lines;"), "{all}");
        let single = grep(
            &call("grep", json!({"pattern": "needle", "path": "src/../a.rs"})),
            root,
            |_| Ok(()),
        )
        .unwrap();
        assert!(single.contains("a.rs:2:needle one"), "{single}");
        let zero = grep(
            &call("grep", json!({"pattern": "absent"})),
            root,
            |_| Ok(()),
        )
        .unwrap();
        assert!(zero.contains("(no matches)"), "{zero}");
    }

    #[test]
    fn grep_authorizes_directory_descent_but_not_ignore_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let child = root.join("nested");
        std::fs::create_dir(&child).unwrap();
        for path in [
            root.join("a.rs"),
            root.join("b.rs"),
            child.join("keep.rs"),
            child.join("skip.rs"),
        ] {
            std::fs::write(path, "needle\n").unwrap();
        }
        let root_ignore = root.join(".gitignore");
        let child_ignore = child.join(".ignore");
        std::fs::write(&root_ignore, "b.rs\n").unwrap();
        std::fs::write(&child_ignore, "keep.rs\n").unwrap();
        let mut entered = false;
        let result = grep(&call("grep", json!({"pattern": "needle"})), &root, |path| {
            assert!(
                path != root_ignore && path != child_ignore,
                "ignore metadata is not a content read"
            );
            if path == child {
                entered = true;
                std::fs::write(&child_ignore, "skip.rs\n").unwrap();
            } else if path.starts_with(&child) {
                assert!(entered);
            }
            Ok(())
        })
        .unwrap();
        assert!(result.contains("a.rs:1:needle"), "{result}");
        assert!(result.contains("keep.rs:1:needle"), "{result}");
        assert!(
            !result.contains("b.rs:") && !result.contains("skip.rs:"),
            "{result}"
        );

        let error = grep(&call("grep", json!({"pattern": "needle"})), &root, |path| {
            if path == child {
                Err("directory denied".into())
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert_eq!(error, "directory denied");
        let error = grep(
            &call("grep", json!({"pattern": ".", "path": ".gitignore"})),
            &root,
            |path| {
                assert_eq!(path, root_ignore);
                Err("content denied".into())
            },
        )
        .unwrap_err();
        assert_eq!(error, "content denied");
    }

    #[test]
    fn grep_keeps_parent_ignore_precedence_and_repository_excludes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let child = root.join("src");
        std::fs::create_dir(&child).unwrap();
        std::fs::create_dir_all(root.join(".git/info")).unwrap();
        std::fs::write(root.join(".gitignore"), "*.rs\n").unwrap();
        std::fs::write(root.join(".ignore"), "!keep.rs\n").unwrap();
        std::fs::write(root.join(".git/info/exclude"), "excluded.txt\n").unwrap();
        std::fs::write(child.join(".gitignore"), "!child.rs\n").unwrap();
        for name in ["keep.rs", "child.rs", "ignored.rs", "excluded.txt"] {
            std::fs::write(child.join(name), "needle\n").unwrap();
        }
        let mut seen = Vec::new();
        let result = grep(
            &call("grep", json!({"pattern": "needle"})),
            &child,
            |path| {
                seen.push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        assert!(!seen.contains(&root.join(".gitignore")));
        assert!(!seen.contains(&root.join(".ignore")));
        assert!(!seen.contains(&root.join(".git/info/exclude")));
        assert!(
            result.contains("keep.rs:1:needle") && result.contains("child.rs:1:needle"),
            "{result}"
        );
        assert!(
            !result.contains("ignored.rs:") && !result.contains("excluded.txt:"),
            "{result}"
        );
    }

    #[test]
    fn grep_global_ignore_probe() {
        let Some(root) = std::env::var_os("NOSH_GREP_GLOBAL_PROBE") else {
            return;
        };
        let root = PathBuf::from(root);
        let config = PathBuf::from(std::env::var_os("GIT_CONFIG_GLOBAL").unwrap());
        let excludes = config.parent().unwrap().join("ignore");
        let mut seen = Vec::new();
        let result = grep(&call("grep", json!({"pattern": "needle"})), &root, |path| {
            seen.push(path.to_path_buf());
            Ok(())
        })
        .unwrap();
        assert!(!seen.contains(&config) && !seen.contains(&excludes));
        assert!(result.contains("keep.txt:1:needle"));
        assert!(!result.contains("excluded.txt:"));
        let error = grep(
            &call("grep", json!({"pattern": ".", "path": config})),
            &root,
            |_| Err("content denied".into()),
        )
        .unwrap_err();
        assert_eq!(error, "content denied");
    }

    #[test]
    fn grep_uses_global_ignore_rules_without_metadata_approval() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("files");
        let home = temp.path().join("home");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&home).unwrap();
        std::fs::write(root.join("keep.txt"), "needle\n").unwrap();
        std::fs::write(root.join("excluded.txt"), "needle\n").unwrap();
        std::fs::write(
            home.join("config"),
            format!("[core]\nexcludesFile={}\n", home.join("ignore").display()),
        )
        .unwrap();
        std::fs::write(home.join("ignore"), "excluded.txt\n").unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tools::tests::grep_global_ignore_probe",
                "--nocapture",
            ])
            .env("NOSH_GREP_GLOBAL_PROBE", &root)
            .env("HOME", &home)
            .env("XDG_CONFIG_HOME", &home)
            .env("GIT_CONFIG_GLOBAL", home.join("config"))
            .env("GIT_CONFIG_SYSTEM", home.join("missing-system-config"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn grep_reports_invalid_inputs_read_errors_and_truncation() {
        let dir = tempfile::tempdir().unwrap();
        for args in [
            json!({}),
            json!({"pattern": "["}),
            json!({"pattern": ".", "path": "missing"}),
            json!({"pattern": ".", "glob": "["}),
        ] {
            assert!(grep(&call("grep", args), dir.path(), |_| Ok(())).is_err());
        }
        std::fs::write(dir.path().join("a"), "x\n".repeat(GREP_MATCHES + 1)).unwrap();
        let result = grep(&call("grep", json!({"pattern": "x"})), dir.path(), |_| {
            Ok(())
        })
        .unwrap();
        assert!(
            result.contains("200 matching lines; truncated=yes"),
            "{result}"
        );
        assert!(
            result.contains("a:200:x") && !result.contains("a:201:x"),
            "{result}"
        );
        std::fs::write(dir.path().join("a"), "x".repeat(20_000)).unwrap();
        let result = grep(&call("grep", json!({"pattern": "x"})), dir.path(), |_| {
            Ok(())
        })
        .unwrap();
        assert!(result.contains("truncated=yes") && result.chars().count() <= OUTPUT_CHARS);
        let error = grep(&call("grep", json!({"pattern": "x"})), dir.path(), |_| {
            Err("protected".into())
        })
        .unwrap_err();
        assert_eq!(error, "protected");
        let error = grep(&call("grep", json!({"pattern": "x"})), dir.path(), |path| {
            if path == dir.path().join("a") {
                std::fs::remove_file(path).unwrap();
            }
            Ok(())
        })
        .unwrap_err();
        assert!(error.contains("a:"), "{error}");
    }

    #[test]
    fn grep_bounds_wide_directory_collection_even_with_no_eligible_files() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..40 {
            std::fs::write(dir.path().join(format!("{n:02}.txt")), "not here\n").unwrap();
        }
        let mut scan = GrepBudget::new(Duration::from_secs(60), &|| false);
        scan.entries_left = 8;
        let mut authorized = Vec::new();
        let result = grep_with_budget(
            &call("grep", json!({"pattern": "absent", "glob": "*.rs"})),
            dir.path(),
            &mut scan,
            |path| {
                authorized.push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(scan.entries_left, 0);
        assert_eq!(scan.bytes_left, GREP_BYTES);
        assert_eq!(authorized, [dir.path()]);
        assert!(result.starts_with("[0 matching lines; truncated=yes]"));
        assert!(result.contains("entry limit reached"), "{result}");
        assert!(
            result.contains("scanned portion; search incomplete"),
            "{result}"
        );
        assert!(!result.contains("(no matches)"), "{result}");
    }

    #[test]
    fn grep_shares_the_entry_budget_across_nonmatching_directories() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..10 {
            let child = dir.path().join(format!("{n:02}"));
            std::fs::create_dir(&child).unwrap();
            for file in 0..5 {
                std::fs::write(child.join(format!("{file}.txt")), "not here\n").unwrap();
            }
        }
        let mut scan = GrepBudget::new(Duration::from_secs(60), &|| false);
        scan.entries_left = 23;
        let mut files = 0;
        let result = grep_with_budget(
            &call("grep", json!({"pattern": "absent"})),
            dir.path(),
            &mut scan,
            |path| {
                files += usize::from(path.is_file());
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(scan.entries_left, 0);
        assert_eq!(files, 10);
        assert_eq!(GREP_BYTES - scan.bytes_left, files * "not here\n".len());
        assert!(result.starts_with("[0 matching lines; truncated=yes]"));
        assert!(result.contains("entry limit reached"), "{result}");
    }

    #[test]
    fn grep_bounds_large_nonmatching_content_and_preserves_earlier_matches() {
        for shared in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            let bytes = if shared {
                std::fs::write(
                    dir.path().join("a.txt"),
                    format!("needle\n{}", "other\n".repeat(8192)),
                )
                .unwrap();
                std::fs::write(dir.path().join("b.txt"), "other\n".repeat(8192)).unwrap();
                GREP_READ_CHUNK
            } else {
                std::fs::write(dir.path().join("a.txt"), "other\n".repeat(128 * 1024)).unwrap();
                2 * GREP_READ_CHUNK
            };
            let mut scan = GrepBudget::new(Duration::from_secs(60), &|| false);
            scan.bytes_left = bytes;
            let result = grep_with_budget(
                &call("grep", json!({"pattern": "needle"})),
                dir.path(),
                &mut scan,
                |_| Ok(()),
            )
            .unwrap();
            assert_eq!(scan.bytes_left, 0);
            assert!(result.contains("truncated=yes"), "{result}");
            assert!(result.contains("byte limit reached"), "{result}");
            if shared {
                assert!(result.contains("1 matching lines"), "{result}");
                assert!(result.contains("a.txt:1:needle"), "{result}");
            } else {
                assert!(result.contains("0 matching lines"), "{result}");
                assert!(result.contains("search incomplete"), "{result}");
            }
            assert!(result.chars().count() <= OUTPUT_CHARS);
        }
    }

    #[test]
    fn grep_does_not_treat_the_byte_limit_as_end_of_line() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a"), "needle suffix\n").unwrap();
        let mut scan = GrepBudget::new(Duration::from_secs(60), &|| false);
        scan.bytes_left = "needle".len();
        let query = call("grep", json!({"pattern": "needle$", "path": "a"}));
        let result = grep_with_budget(&query, dir.path(), &mut scan, |_| Ok(())).unwrap();
        assert_eq!(scan.bytes_left, 0);
        assert!(
            result.starts_with("[0 matching lines; truncated=yes]"),
            "{result}"
        );
        assert!(!result.contains("a:1:"), "{result}");

        std::fs::write(dir.path().join("a"), "needle").unwrap();
        let complete = grep(&query, dir.path(), |_| Ok(())).unwrap();
        assert!(
            complete.starts_with("[1 matching lines; truncated=no]"),
            "{complete}"
        );
    }

    #[test]
    fn grep_reader_bounds_each_read_and_checks_deadlines_between_reads() {
        let mut scan = GrepBudget::new(Duration::from_secs(60), &|| false);
        let mut reader = GrepReader {
            inner: io::repeat(b'x'),
            budget: &mut scan,
        };
        let mut buffer = vec![0; GREP_READ_CHUNK * 2];
        assert_eq!(reader.read(&mut buffer).unwrap(), GREP_READ_CHUNK);
        assert_eq!(reader.budget.bytes_left, GREP_BYTES - GREP_READ_CHUNK);
        reader.budget.started -= Duration::from_secs(60);
        let error = reader.read(&mut buffer).unwrap_err();
        assert_eq!(error.to_string(), "time limit reached");
        assert_eq!(reader.budget.bytes_left, GREP_BYTES - GREP_READ_CHUNK);
    }

    #[test]
    fn grep_cancels_during_directory_enumeration() {
        let dir = tempfile::tempdir().unwrap();
        for n in 0..64 {
            std::fs::write(dir.path().join(format!("{n:02}.txt")), "not here\n").unwrap();
        }
        let checks = Cell::new(0);
        let cancelled = || {
            checks.set(checks.get() + 1);
            checks.get() >= 20
        };
        let mut scan = GrepBudget::new(Duration::from_secs(60), &cancelled);
        let mut authorized = Vec::new();
        let error = grep_with_budget(
            &call("grep", json!({"pattern": "absent"})),
            dir.path(),
            &mut scan,
            |path| {
                authorized.push(path.to_path_buf());
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.contains("cancelled by the user"), "{error}");
        assert!((GREP_ENTRIES - 64..GREP_ENTRIES).contains(&scan.entries_left));
        assert_eq!(scan.bytes_left, GREP_BYTES);
        assert_eq!(authorized, [dir.path()]);
    }

    #[test]
    fn grep_cancels_during_nonmatching_content_reads() {
        let dir = tempfile::tempdir().unwrap();
        let contents = "other\n".repeat(128 * 1024);
        std::fs::write(dir.path().join("a"), &contents).unwrap();
        let reading = Cell::new(false);
        let checks = Cell::new(0);
        let cancelled = || {
            if reading.get() {
                checks.set(checks.get() + 1);
            }
            checks.get() >= 4
        };
        let mut scan = GrepBudget::new(Duration::from_secs(60), &cancelled);
        let error = grep_with_budget(
            &call("grep", json!({"pattern": "absent", "path": "a"})),
            dir.path(),
            &mut scan,
            |_| {
                reading.set(true);
                Ok(())
            },
        )
        .unwrap_err();
        assert!(error.contains("cancelled by the user"), "{error}");
        assert!((1..contents.len()).contains(&(GREP_BYTES - scan.bytes_left)));
        assert_eq!(scan.truncated, None, "cancellation is not truncation");
    }

    #[test]
    fn grep_reports_timeout_but_prioritizes_user_cancellation() {
        let dir = tempfile::tempdir().unwrap();
        let query = call("grep", json!({"pattern": "absent"}));
        let result = super::grep(&query, dir.path(), Duration::ZERO, &|| false, |_| {
            panic!("expired scan must not authorize any reads")
        })
        .unwrap();
        assert!(result.starts_with("[0 matching lines; truncated=yes]"));
        assert!(result.contains("time limit reached"), "{result}");
        assert!(result.contains("search incomplete"), "{result}");
        let error = super::grep(&query, dir.path(), Duration::ZERO, &|| true, |_| {
            panic!("cancelled scan must not authorize any reads")
        })
        .unwrap_err();
        assert_eq!(error, "grep cancelled by the user");
    }

    #[cfg(unix)]
    #[test]
    fn grep_does_not_follow_descendant_symlinks_or_linked_directories() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "needle").unwrap();
        std::os::unix::fs::symlink(outside.path(), dir.path().join("linked-dir")).unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("secret"),
            dir.path().join("linked-file"),
        )
        .unwrap();
        let result = grep(
            &call("grep", json!({"pattern": "needle"})),
            dir.path(),
            |_| Ok(()),
        )
        .unwrap();
        assert!(result.contains("(no matches)"), "{result}");
        assert!(
            grep(
                &call("grep", json!({"pattern": "needle", "path": "linked-dir"})),
                dir.path(),
                |_| Ok(()),
            )
            .is_err()
        );
    }

    #[test]
    fn middle_truncation_keeps_head_and_tail() {
        let s: String = (0..10_000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let (t, cut) = truncate_middle(&s, 1000);
        assert!(cut);
        assert!(t.starts_with(&s[..600]));
        assert!(t.ends_with(&s[s.len() - 400..]));
        assert!(t.contains("9000 characters omitted"));
        assert_eq!(truncate_middle("short", 10), ("short".into(), false));
    }

    #[test]
    fn middle_truncation_preserves_character_budgets_and_utf8() {
        for unit in ["", "a", "abcdef", "中文🙂e\u{301}\r\n尾部"] {
            for repeats in [1, 3, 17] {
                let text = unit.repeat(repeats);
                let chars: Vec<_> = text.chars().collect();
                for max in 0..=chars.len() + 2 {
                    let (actual, truncated) = truncate_middle(&text, max);
                    if chars.len() <= max {
                        assert_eq!(actual, text);
                        assert!(!truncated);
                    } else {
                        let head = max * 6 / 10;
                        let tail = max - head;
                        let expected = format!(
                            "{}\n[… {} characters omitted …]\n{}",
                            chars[..head].iter().collect::<String>(),
                            chars.len() - max,
                            chars[chars.len() - tail..].iter().collect::<String>()
                        );
                        assert_eq!(actual, expected, "{text:?}, max={max}");
                        assert!(truncated);
                    }
                }
            }
        }
        assert!(matches!(
            truncate_counted("short", 10, 5).0,
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn command_result_budgets_count_characters_not_bytes() {
        let mut result = CommandResult {
            stdout: "中".repeat(4500),
            stderr: "错".repeat(1500),
            ..CommandResult::default()
        };
        let output = format_command_result(&result, None);
        assert!(output.contains("truncated=no"));
        assert_eq!(output.matches('中').count(), 4500);
        assert_eq!(output.matches('错').count(), 1500);

        result.stdout.push('中');
        let output = format_command_result(&result, None);
        assert!(output.contains("truncated=yes"));
        assert!(output.contains("1 characters omitted"));
        assert_eq!(output.matches('中').count(), 4500);
        assert_eq!(output.matches('错').count(), 1500);
    }

    #[test]
    fn command_result_format() {
        let r = CommandResult {
            exit_code: 0,
            stdout: "LISTEN 0 511 *:8080\n".into(),
            ..CommandResult::default()
        };
        let s = format_command_result(&r, None);
        assert!(s.starts_with("[exit_code=0 duration=0.00s truncated=no]\n--- stdout ---\nLISTEN"));
        assert!(s.ends_with("--- stderr ---\n(empty)"));
        let big = CommandResult {
            exit_code: 1,
            stdout: "x".repeat(20_000),
            stderr: "boom\n".into(),
            ..CommandResult::default()
        };
        let s = format_command_result(&big, Some(Path::new("/tmp/o.log")));
        assert!(s.contains("truncated=yes"));
        assert!(s.contains("[full output: /tmp/o.log]"));
        assert!(s.len() < OUTPUT_CHARS + 400);
        assert!(s.ends_with("boom"));
    }

    #[test]
    fn saved_output_is_written_as_is_through_the_redactor() {
        let dir = std::env::temp_dir().join(format!("nosh-outputs-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let r = CommandResult {
            exit_code: 1,
            stdout: "API_KEY=\"sk-live 0123456789abcdef\" ghp_ABCDEFGHIJKLMNOP1234\n".into(),
            stderr: "-----BEGIN RSA PRIVATE KEY-----\nxyz\n".into(),
            ..CommandResult::default()
        };
        let p = save_output_in(&dir, 7, "env | grep KEY", &r, &NoRedact).unwrap();
        let text = std::fs::read_to_string(&p).unwrap();
        assert_eq!(
            text,
            format!(
                "$ env | grep KEY\n[exit_code=1]\n--- stdout ---\n{}\n--- stderr ---\n{}\n",
                r.stdout, r.stderr
            )
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&p).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600, "{mode:o}");
            let dmode = std::fs::metadata(&dir).unwrap().permissions().mode();
            assert_eq!(dmode & 0o777, 0o700, "{dmode:o}");
        }
        assert!(matches!(NoRedact.redact("x"), Cow::Borrowed("x")));
        // Whatever redactor the agent is given is what reaches the disk.
        struct Upper;
        impl Redactor for Upper {
            fn redact<'a>(&self, text: &'a str) -> Cow<'a, str> {
                Cow::Owned(text.to_uppercase())
            }
        }
        let p = save_output_in(&dir, 8, "echo hi", &r, &Upper).unwrap();
        assert!(
            std::fs::read_to_string(&p)
                .unwrap()
                .starts_with("$ ECHO HI\n")
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn paths_are_normalized() {
        let c = call("read_file", json!({"path": "src/../../etc/./passwd"}));
        assert_eq!(
            tool_path(&c, Path::new("/home/u/proj")),
            PathBuf::from("/home/u/etc/passwd")
        );
    }

    #[test]
    fn prepared_reads_use_the_live_permission_context() {
        let ctx =
            nosh_permissions::Context::new("/workspace", "/workspace").with_home("/shell-home");
        let read = prepare_read(&call("read_file", json!({"path": "~/file"})), &ctx).unwrap();
        assert_eq!(read.str_arg("path"), Some("/shell-home/file"));
        let grep = prepare_read(&call("grep", json!({"pattern": "needle"})), &ctx).unwrap();
        assert_eq!(grep.str_arg("path"), Some("/workspace"));
    }

    #[test]
    fn read_text_and_binary_files() {
        let dir = std::env::temp_dir().join(format!("nosh-tools-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        std::fs::write(dir.join("bin.dat"), [0u8, 1, 2, 3]).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}\n").unwrap();
        std::fs::write(dir.join(".gitignore"), "ignored.log\n").unwrap();
        std::fs::write(dir.join("ignored.log"), "x").unwrap();
        std::fs::create_dir_all(dir.join(".git")).unwrap();

        let r = read_file(
            &call("read_file", json!({"path": "a.txt", "start_line": 2})),
            &dir,
        )
        .unwrap();
        assert!(r.contains("    2  two\n    3  three"), "{r}");
        assert!(!r.contains("one"));
        let b = read_file(&call("read_file", json!({"path": "bin.dat"})), &dir).unwrap();
        assert!(b.starts_with("[binary file"));
        assert!(read_file(&call("read_file", json!({"path": "nope"})), &dir).is_err());
        assert!(
            read_file(
                &call(
                    "read_file",
                    json!({"path": "a.txt", "start_line": 3, "end_line": 1})
                ),
                &dir
            )
            .is_err()
        );
        let one = read_file(
            &call(
                "read_file",
                json!({"path": "a.txt", "start_line": 2, "end_line": 2}),
            ),
            &dir,
        )
        .unwrap();
        assert!(one.contains("    2  two\n[showing lines 2-2;"), "{one}");
        assert!(!one.contains("three"), "{one}");

        let error = read_file(&call("read_file", json!({"path": "."})), &dir).unwrap_err();
        assert!(error.contains("is a directory"));
        assert!(!error.contains("list_dir"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_file_pages_past_8_mib_and_bounds_what_it_keeps() {
        let dir = std::env::temp_dir().join(format!("nosh-bigread-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // 100,000 lines of 100 bytes: 10 MB, beyond the old 8 MiB prefix.
        let mut s = String::with_capacity(10_000_000);
        for i in 1..=100_000 {
            let _ = writeln!(s, "line {i:06} {}", "x".repeat(87));
        }
        std::fs::write(dir.join("big.log"), &s).unwrap();
        let read = |args: serde_json::Value| read_file(&call("read_file", args), &dir);

        let r = read(json!({"path": "big.log", "start_line": 99_990})).unwrap();
        assert!(
            r.starts_with(&format!(
                "[{} · 100000 lines]",
                dir.join("big.log").display()
            )),
            "{r}"
        );
        assert!(r.contains("99990  line 099990 x"), "{r}");
        assert!(
            r.ends_with(&format!("100000  line 100000 {}", "x".repeat(87))),
            "{r}"
        );
        assert!(!r.contains("continue with"), "{r}");
        let e = read(json!({"path": "big.log", "start_line": 100_001})).unwrap_err();
        assert!(e.contains("past the end (100000 lines)"), "{e}");
        // Output stays bounded however many lines are asked for.
        let r = read(json!({"path": "big.log", "start_line": 50_000, "end_line": 99_000})).unwrap();
        assert!(r.len() <= OUTPUT_CHARS, "{}", r.len());
        assert!(r.contains("50000  line 050000"), "{r}");
        assert!(r.contains("continue with start_line="), "{r}");

        // Counting stops at the budget; the total is then reported as a lower bound.
        let r = read_file_with(
            &call("read_file", json!({"path": "big.log", "end_line": 2})),
            &dir,
            1024 * 1024,
        )
        .unwrap();
        let header = r.lines().next().unwrap();
        assert!(
            header.contains("≥ ") && header.contains("stopped counting"),
            "{header}"
        );
        assert!(
            r.ends_with("[showing lines 1-2; continue with start_line=3]"),
            "{r}"
        );

        // A 9 MiB line is cut when shown and skipped without being kept.
        let mut long = "a".repeat(9 * 1024 * 1024);
        long.push_str("\ntail");
        std::fs::write(dir.join("long.txt"), &long).unwrap();
        let r = read(json!({"path": "long.txt", "start_line": 2})).unwrap();
        assert!(
            r.contains("· 2 lines]") && r.ends_with("    2  tail"),
            "{r}"
        );
        let r = read(json!({"path": "long.txt", "end_line": 1})).unwrap();
        assert!(r.contains(&format!("    1  {}…", "a".repeat(400))), "{r}");
        assert!(r.len() < 1000, "{}", r.len());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn read_file_line_counts_match_str_lines() {
        let dir = std::env::temp_dir().join(format!("nosh-lines-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (i, text) in ["", "\n", "a", "a\n", "a\nb", "a\r\nb\r\n", "\n\nx"]
            .iter()
            .enumerate()
        {
            let name = format!("f{i}.txt");
            std::fs::write(dir.join(&name), text).unwrap();
            let r = read_file(&call("read_file", json!({"path": name})), &dir).unwrap();
            let n = text.lines().count();
            assert!(r.contains(&format!("· {n} lines]")), "{text:?}: {r}");
            if n > 0 {
                let last = text.lines().last().unwrap();
                let want = format!("{n:>5}  {last}");
                assert!(r.ends_with(want.trim_end()), "{text:?}: {r}");
            } else {
                assert!(r.ends_with("(empty file)"), "{r}");
            }
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
