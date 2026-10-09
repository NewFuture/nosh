//! Tool definitions and the built-in read-only tools (design §5.5).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{self, BufRead, Read};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use nosh_engine::{ToolCall, ToolSpec};
use nosh_shell::{CommandResult, OutputState, UserOutput};
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
    format!(
        "[user_output {}]\n{}\n[/user_output]",
        output_metadata(output),
        output_body(output)
    )
}

pub(crate) fn format_assist_output(output: &UserOutput) -> String {
    let mut metadata = match output.state {
        OutputState::NotCaptured => json!({"state": "not_captured", "reason": "capture_disabled"}),
        OutputState::Unavailable(reason) => {
            json!({"state": "unavailable", "reason": reason.reason()})
        }
        OutputState::Captured => json!({}),
    };
    for (key, active) in [
        ("truncated", output.truncated),
        ("incomplete", output.incomplete),
        ("concurrent_output", output.mixed),
    ] {
        if active {
            metadata[key] = json!(true);
        }
    }
    let fields = format_key_values(&metadata);
    let quality = if fields.is_empty() {
        String::new()
    } else {
        format!("{fields}\n\n")
    };
    format!(
        "Terminal output (stdout/stderr not separated):\n{quality}{}",
        text_block(output_body(output))
    )
}

pub(crate) fn format_key_values(metadata: &serde_json::Value) -> String {
    metadata
        .as_object()
        .expect("metadata is an object")
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub(crate) fn text_block(text: &str) -> String {
    fenced_block(text, "text")
}

pub(crate) fn shell_block(text: &str) -> String {
    fenced_block(text, "bash")
}

fn fenced_block(text: &str, language: &str) -> String {
    let longest = text.split(|c| c != '`').map(str::len).max().unwrap_or(0);
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{language}\n{text}\n{fence}")
}

pub(crate) fn output_metadata(output: &UserOutput) -> serde_json::Value {
    let (state, reason) = match output.state {
        OutputState::NotCaptured => ("not_captured", Some("capture_disabled")),
        OutputState::Unavailable(reason) => ("unavailable", Some(reason.reason())),
        OutputState::Captured => ("captured", None),
    };
    json!({
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
    })
}

fn output_body(output: &UserOutput) -> &str {
    if output.has_body() {
        if output.text.is_empty() {
            if output.observed_bytes == Some(0) {
                "(Capture succeeded: no terminal output.)"
            } else {
                "(Terminal bytes were captured, but no text remained after display cleanup.)"
            }
        } else {
            &output.text
        }
    } else if output.mixed {
        "(Known concurrent output: content omitted; do not attribute it to this command.)"
    } else {
        "(No captured output is available. Do not invent error text.)"
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
        &nosh_platform::paths::state_dir().join("outputs"),
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
    nosh_platform::paths::ensure_private_dir(dir).ok()?;
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
mod tests;
