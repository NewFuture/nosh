//! Tool definitions and the built-in read-only tools (design §5.5).

use std::borrow::Cow;
use std::fmt::Write as _;
use std::io::{BufRead, Read};
use std::path::{Path, PathBuf};

use nosh_llm::{ToolCall, ToolSpec};
use nosh_shell::CommandResult;
use serde_json::json;

/// Characters of tool output fed back per call (~1.5K tokens).
pub const OUTPUT_CHARS: usize = 6000;
pub const READ_FILE_LINES: usize = 400;
pub const LIST_DIR_ENTRIES: usize = 300;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolSet {
    /// run_command, read_file, list_dir, propose_command.
    Full,
    /// Piped attachments: read_file and list_dir only.
    ReadOnly,
    /// Suggestions: propose_command only.
    Suggest,
}

pub fn run_command_spec() -> ToolSpec {
    ToolSpec {
        name: "run_command".into(),
        description: "Run a bash command in the user's shell session and return its output.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string", "description": "The bash command line"},
                "timeout_sec": {"type": "integer", "description": "Timeout in seconds (default 60, max 600)"}
            },
            "required": ["command"]
        }),
    }
}

pub fn read_file_spec() -> ToolSpec {
    ToolSpec {
        name: "read_file".into(),
        description: "Read a text file (not a directory) with line numbers, at most 400 lines."
            .into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "start_line": {"type": "integer"},
                "end_line": {"type": "integer"}
            },
            "required": ["path"]
        }),
    }
}

pub fn list_dir_spec() -> ToolSpec {
    ToolSpec {
        name: "list_dir".into(),
        description: "List file names and sizes in a directory (respects .gitignore). For counting lines or searching, use run_command.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "path": {"type": "string", "description": "Directory (default: cwd)"},
                "depth": {"type": "integer", "description": "1-3 (default 1)"}
            },
            "required": []
        }),
    }
}

pub fn propose_command_spec() -> ToolSpec {
    ToolSpec {
        name: "propose_command".into(),
        description: "Put a command into the user's input line for them to review and run. Use for suggestions and for commands that need a terminal or a password.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "explanation": {"type": "string"}
            },
            "required": ["command"]
        }),
    }
}

pub fn specs(set: ToolSet) -> Vec<ToolSpec> {
    match set {
        ToolSet::Full => vec![
            run_command_spec(),
            read_file_spec(),
            list_dir_spec(),
            propose_command_spec(),
        ],
        ToolSet::ReadOnly => vec![read_file_spec(), list_dir_spec()],
        ToolSet::Suggest => vec![propose_command_spec()],
    }
}

/// Keeps the first 60% and last 40% of `s` within `max` characters.
pub fn truncate_middle(s: &str, max: usize) -> (String, bool) {
    let n = s.chars().count();
    if n <= max {
        return (s.to_string(), false);
    }
    let head = max * 6 / 10;
    let tail = max - head;
    let chars: Vec<char> = s.chars().collect();
    let omitted = n - head - tail;
    let mut out: String = chars[..head].iter().collect();
    let _ = write!(out, "\n[… {omitted} characters omitted …]\n");
    out.extend(&chars[n - tail..]);
    (out, true)
}

fn secs(d: std::time::Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// Tool result text for `run_command` (plain header + raw output).
pub fn format_command_result(r: &CommandResult, full_log: Option<&Path>) -> String {
    let body_budget = OUTPUT_CHARS;
    let out_len = r.stdout.chars().count();
    let err_len = r.stderr.chars().count();
    let (out, err, truncated) = if out_len + err_len <= body_budget {
        (r.stdout.clone(), r.stderr.clone(), r.truncated)
    } else {
        // Give stderr up to a third of the budget, stdout the rest.
        let err_budget = err_len.min(body_budget / 3);
        let out_budget = body_budget - err_budget;
        let (o, _) = truncate_middle(&r.stdout, out_budget);
        let (e, _) = truncate_middle(&r.stderr, err_budget.max(1));
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
        s.push_str("[note] the command tried to read from the terminal (e.g. a password prompt) and was stopped; use propose_command so the user can run it\n");
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
        return Err(format!("{} is a directory; use list_dir", path.display()));
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

const SIZE_UNITS: &[&str] = &["bytes", "KB", "MB", "GB", "TB"];

fn size_unit(n: u64) -> usize {
    let mut v = n;
    let mut i = 0;
    while v >= 1024 && i < SIZE_UNITS.len() - 1 {
        v /= 1024;
        i += 1;
    }
    i
}

/// `n` expressed in `SIZE_UNITS[unit]`. One unit per listing lets a small model
/// compare sizes directly (it ranked "781.2 KB" above "11.4 MB").
fn size_in(n: u64, unit: usize) -> String {
    if unit == 0 || n == 0 {
        return format!("{n} bytes");
    }
    let v = n as f64 / 1024f64.powi(unit as i32);
    if v < 0.05 {
        format!("<0.1 {}", SIZE_UNITS[unit])
    } else {
        format!("{v:.1} {}", SIZE_UNITS[unit])
    }
}

/// `list_dir`: tree listing honouring .gitignore, depth ≤ 3.
pub fn list_dir(call: &ToolCall, cwd: &Path) -> Result<String, String> {
    let root = tool_path(call, cwd);
    if !root.is_dir() {
        return Err(format!("{} is not a directory", root.display()));
    }
    let depth = call.int_arg("depth").unwrap_or(1).clamp(1, 3) as usize;
    let walker = ignore::WalkBuilder::new(&root)
        .max_depth(Some(depth))
        .hidden(false)
        .git_ignore(true)
        .git_exclude(true)
        .parents(true)
        .filter_entry(|e| e.file_name() != ".git")
        .sort_by_file_path(|a, b| a.cmp(b))
        .build();
    let mut out = format!("[{}]\n", root.display());
    let mut entries: Vec<(usize, String, Option<u64>)> = Vec::new();
    let mut more = 0usize;
    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        if entries.len() >= LIST_DIR_ENTRIES {
            more += 1;
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        let size = (!is_dir).then(|| entry.metadata().map(|m| m.len()).unwrap_or(0));
        entries.push((entry.depth(), name, size));
    }
    let unit = size_unit(entries.iter().filter_map(|e| e.2).max().unwrap_or(0));
    for (depth, name, size) in &entries {
        let indent = "  ".repeat(depth - 1);
        match size {
            None => {
                let _ = writeln!(out, "{indent}{name}/");
            }
            Some(n) => {
                let _ = writeln!(out, "{indent}{name}  ({})", size_in(*n, unit));
            }
        }
    }
    if entries.is_empty() {
        out.push_str("(empty)\n");
    }
    if more > 0 {
        let _ = writeln!(out, "[… {more} more entries]");
    }
    Ok(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn call(name: &str, args: Value) -> ToolCall {
        ToolCall {
            name: name.into(),
            args: args.as_object().cloned().unwrap(),
        }
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
    fn read_and_list() {
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

        let l = list_dir(&call("list_dir", json!({"depth": 2})), &dir).unwrap();
        assert!(l.contains("src/\n  main.rs"), "{l}");
        assert!(l.contains("a.txt"));
        assert!(!l.contains("ignored.log"), "{l}");
        assert!(!l.contains(".git/"), "{l}");
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
    #[test]
    fn list_dir_sizes_share_one_unit() {
        let dir = std::env::temp_dir().join(format!("nosh-sizes-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (name, len) in [
            ("big", 21_000_000),
            ("mid", 800_000),
            ("tiny", 9),
            ("empty", 0),
        ] {
            std::fs::write(dir.join(name), vec![b'x'; len]).unwrap();
        }
        let l = list_dir(&call("list_dir", json!({})), &dir).unwrap();
        assert!(l.contains("big  (20.0 MB)"), "{l}");
        assert!(l.contains("mid  (0.8 MB)"), "{l}");
        assert!(l.contains("tiny  (<0.1 MB)"), "{l}");
        assert!(l.contains("empty  (0 bytes)"), "{l}");
        std::fs::remove_file(dir.join("big")).unwrap();
        std::fs::remove_file(dir.join("mid")).unwrap();
        let l = list_dir(&call("list_dir", json!({})), &dir).unwrap();
        assert!(l.contains("tiny  (9 bytes)"), "{l}");
        let _ = std::fs::remove_dir_all(dir);
    }
}
