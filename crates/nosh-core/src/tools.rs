//! Tool definitions and the built-in read-only tools (design §5.5).

use std::fmt::Write as _;
use std::io::Read;
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

/// Masks common secrets before anything is written to disk.
pub fn redact(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_pem = false;
    for line in s.split_inclusive('\n') {
        if line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            in_pem = true;
            out.push_str("[REDACTED PRIVATE KEY]\n");
            continue;
        }
        if in_pem {
            if line.contains("-----END") {
                in_pem = false;
            }
            continue;
        }
        let mut l = line.to_string();
        mask_tokens(&mut l);
        mask_assignments(&mut l);
        out.push_str(&l);
    }
    out
}

const REDACTED: &str = "[REDACTED]";
const TOKEN_PREFIXES: &[&str] = &[
    "ghp_",
    "gho_",
    "ghs_",
    "ghu_",
    "github_pat_",
    "AKIA",
    "ASIA",
    "xoxb-",
    "xoxp-",
    "sk-",
];
const SECRET_KEYS: &[&str] = &[
    "password",
    "passwd",
    "passphrase",
    "token",
    "secret",
    "api_key",
    "apikey",
    "access_key",
    "private_key",
    "authorization",
];

fn token_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '-'
}

/// Well-known token formats anywhere on the line.
fn mask_tokens(l: &mut String) {
    let mut from = 0;
    while from < l.len() {
        let Some((at, plen)) = TOKEN_PREFIXES
            .iter()
            .filter_map(|p| l[from..].find(p).map(|i| (from + i, p.len())))
            .min()
        else {
            break;
        };
        let starts_token = l[..at].chars().next_back().is_none_or(|c| !token_char(c));
        let end = l[at..]
            .find(|c: char| !token_char(c))
            .map_or(l.len(), |e| at + e);
        if starts_token && end - at >= plen + 8 {
            l.replace_range(at..end, REDACTED);
            from = at + REDACTED.len();
        } else {
            from = at + plen;
        }
    }
}

/// Values of `password=…`, `token: …`, `"api_key": "…"` and the like.
fn mask_assignments(l: &mut String) {
    let lower = l.to_ascii_lowercase();
    let b = lower.as_bytes();
    let mut spans: Vec<(usize, usize)> = Vec::new();
    for key in SECRET_KEYS {
        let mut from = 0;
        while let Some(i) = lower[from..].find(key) {
            let k = from + i;
            from = k + key.len();
            // Whole words only: `tokens=3` and `tokenizer:` are not secrets.
            let before_ok = lower[..k]
                .chars()
                .next_back()
                .is_none_or(|c| !c.is_ascii_alphanumeric());
            let mut v = k + key.len();
            while v < b.len() && matches!(b[v], b'"' | b'\'' | b' ') {
                v += 1;
            }
            if !before_ok || v >= b.len() || !matches!(b[v], b'=' | b':') {
                continue;
            }
            v += 1;
            while v < b.len() && matches!(b[v], b'"' | b'\'' | b' ') {
                v += 1;
            }
            for scheme in ["bearer ", "basic ", "token "] {
                if lower[v..].starts_with(scheme) {
                    v += scheme.len();
                }
            }
            let end = lower[v..]
                .find(|c: char| c.is_whitespace() || matches!(c, '&' | '"' | '\'' | ',' | ';'))
                .map_or(lower.len(), |e| v + e);
            if end > v {
                spans.push((v, end));
            }
        }
    }
    spans.sort_unstable();
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for (s, e) in spans {
        match merged.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => merged.push((s, e)),
        }
    }
    for (s, e) in merged.into_iter().rev() {
        if l.get(s..e).is_some_and(|v| v != REDACTED) {
            l.replace_range(s..e, REDACTED);
        }
    }
}

/// Saves the full output of a truncated command under `state/outputs/`.
pub fn save_output(id: usize, command: &str, r: &CommandResult) -> Option<PathBuf> {
    let dir = nosh_hub::paths::state_dir().join("outputs");
    nosh_hub::paths::ensure_private_dir(&dir).ok()?;
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
    f.write_all(redact(&text).as_bytes()).ok()?;
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
    let path = tool_path(call, cwd);
    let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!("{} is a directory; use list_dir", path.display()));
    }
    let mut f = std::fs::File::open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut bytes = Vec::new();
    f.by_ref()
        .take(8 * 1024 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.iter().take(8192).any(|&b| b == 0) {
        return Ok(format!("[binary file, {} bytes; not shown]", meta.len()));
    }
    let text = String::from_utf8_lossy(&bytes);
    let total = text.lines().count();
    let start = call.int_arg("start_line").unwrap_or(1).max(1) as usize;
    let end_req = call
        .int_arg("end_line")
        .map(|e| e.max(1) as usize)
        .unwrap_or(start + READ_FILE_LINES - 1);
    if end_req < start {
        return Err(format!("end_line {end_req} is before start_line {start}"));
    }
    let end = end_req.min(start + READ_FILE_LINES - 1).min(total);
    let mut out = format!("[{} · {} lines]\n", path.display(), total);
    if total == 0 {
        out.push_str("(empty file)");
        return Ok(out);
    }
    if start > total {
        return Err(format!(
            "start_line {start} is past the end ({total} lines)"
        ));
    }
    let mut shown_to = start - 1;
    for (i, line) in text
        .lines()
        .enumerate()
        .skip(start - 1)
        .take(end + 1 - start)
    {
        let line: String = if line.chars().count() > 400 {
            line.chars().take(400).chain("…".chars()).collect()
        } else {
            line.to_string()
        };
        let entry = format!("{:>5}  {line}\n", i + 1);
        if out.len() + entry.len() > OUTPUT_CHARS {
            break;
        }
        out.push_str(&entry);
        shown_to = i + 1;
    }
    if shown_to < total {
        let _ = write!(
            out,
            "[showing lines {start}-{shown_to}; continue with start_line={}]",
            shown_to + 1
        );
    }
    Ok(out.trim_end().to_string())
}

fn human(n: u64) -> String {
    const U: &[&str] = &["bytes", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 {
        format!("{n} bytes")
    } else {
        format!("{v:.1} {}", U[i])
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
    let mut count = 0usize;
    let mut more = 0usize;
    for entry in walker.flatten() {
        if entry.depth() == 0 {
            continue;
        }
        if count >= LIST_DIR_ENTRIES {
            more += 1;
            continue;
        }
        count += 1;
        let indent = "  ".repeat(entry.depth() - 1);
        let name = entry.file_name().to_string_lossy();
        let is_dir = entry.file_type().is_some_and(|t| t.is_dir());
        if is_dir {
            let _ = writeln!(out, "{indent}{name}/");
        } else {
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = writeln!(out, "{indent}{name}  ({})", human(size));
        }
    }
    if count == 0 {
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
    fn redaction() {
        let s = redact(
            "token=abc123 ghp_ABCDEFGHIJKLMNOP1234 ok\n-----BEGIN RSA PRIVATE KEY-----\nxyz\n-----END RSA PRIVATE KEY-----\nafter\n",
        );
        assert!(!s.contains("abc123"));
        assert!(!s.contains("ghp_ABCDEFGH"));
        assert!(!s.contains("xyz"));
        assert!(s.contains("after"));
        // A short look-alike earlier on the line must not hide a real key.
        let s = redact("task-1 done; key sk-live0123456789abcdef and task-2\n");
        assert!(!s.contains("sk-live0123456789abcdef"), "{s}");
        assert!(s.contains("task-1") && s.contains("task-2"), "{s}");
        // Every assignment on a line, in several syntaxes.
        let s = redact(
            "password=p1 GITHUB_TOKEN=t2 \"api_key\": \"k3\" Authorization: Bearer b4 tokens=5\n",
        );
        for secret in ["p1", "t2", "k3", "b4"] {
            assert!(!s.contains(secret), "{secret} in {s}");
        }
        assert!(s.contains("tokens=5"), "{s}");
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
}
