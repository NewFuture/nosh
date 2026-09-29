//! Command identity and bounded help queries; never executes arbitrary shell text.

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nosh_llm::{CancelHandle, ToolCall};
use nosh_permissions::{
    Context, Decision, Risk, SessionAllowList, assess_command_with_lookup, evaluate,
};
use nosh_shell::{CommandSnapshot, Resolution};
use serde_json::json;

use crate::{AgentConfig, tools};

pub(crate) fn query(
    commands: &CommandSnapshot,
    context: &Context,
    cfg: &AgentConfig,
    call: &ToolCall,
    cancel: &CancelHandle,
) -> Result<String, String> {
    if call
        .args
        .keys()
        .any(|key| key != "name" && key != "query" && key != "topic")
    {
        return Err("unknown command_info parameter".into());
    }
    let name = call
        .str_arg("name")
        .filter(|name| {
            name.len() <= 512 && !name.chars().any(|c| c.is_whitespace() || c.is_control())
        })
        .ok_or("invalid command name")?;
    let kind = call.str_arg("query").ok_or("missing query")?;
    let topic = match call.args.get("topic") {
        None => None,
        Some(serde_json::Value::String(topic))
            if kind == "help" && !topic.is_empty() && topic.len() <= 120 =>
        {
            Some(topic.to_lowercase())
        }
        _ => return Err("topic must be a nonempty help filter of at most 120 bytes".into()),
    };
    if kind == "list" {
        let (names, complete, reason) = commands.candidates(name);
        return Ok(
            json!({"candidates": names, "complete": complete, "reason": reason}).to_string(),
        );
    }
    if name.is_empty() {
        return Err("empty command name".into());
    }
    let resolution = commands.resolve(name);
    if kind == "resolve" {
        return Ok(match resolution {
            Resolution::File(path) => json!({"name": name, "kind": "external", "path": path}),
            Resolution::Alias(_) => json!({"name": name, "kind": "alias"}),
            Resolution::Function => json!({"name": name, "kind": "function"}),
            Resolution::Builtin => json!({"name": name, "kind": "builtin"}),
            Resolution::Keyword => json!({"name": name, "kind": "keyword"}),
            Resolution::NotFound => json!({"name": name, "kind": "not_found"}),
        }
        .to_string());
    }
    let flag = match kind {
        "help" => "--help",
        "version" => "--version",
        _ => return Err("query must be list, resolve, help or version".into()),
    };
    let Resolution::File(path) = resolution else {
        return Err("help/version execution requires an external program; aliases and functions are not executed".into());
    };
    let executable = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve query program: {error}"))?;
    let quoted = format!("'{}' {flag}", name.replace('\'', "'\\''"));
    let report = assess_command_with_lookup(&quoted, context, &|candidate, _, _, _| {
        (candidate == name).then(|| executable.clone())
    });
    if report.risk() != Risk::Safe
        || report.incomplete
        || report.changes_session
        || report.reads_protected
        || report.network
        || report
            .operations
            .iter()
            .any(|op| op.local_program || op.opaque)
        || !matches!(
            evaluate(&report, cfg.mode, &cfg.rules, &SessionAllowList::default()).decision,
            Decision::Allow
        )
    {
        return Err("help/version query is not authorized; no program was executed".into());
    }
    if cancel.is_cancelled() {
        return Err("query cancelled".into());
    }
    let mut command = Command::new(&executable);
    command
        .arg0(&path)
        .arg(flag)
        .current_dir(commands.cwd())
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    for name in &context.exported {
        if let Some(value) = context.variables.get(name) {
            command.env(name, value);
        }
    }
    for (name, value) in nosh_shell::backend::ANTI_HANG_ENV {
        command.env(name, value);
    }
    let child = command
        .spawn()
        .map_err(|e| format!("help query could not start: {e}"))?;
    let mut child = OwnedQuery {
        child,
        reaped: false,
    };
    let mut stdout = child.child.stdout.take().ok_or("missing query stdout")?;
    let mut stderr = child.child.stderr.take().ok_or("missing query stderr")?;
    nonblocking(&stdout)?;
    nonblocking(&stderr)?;
    let started = Instant::now();
    let mut out = Vec::new();
    let mut err = Vec::new();
    let mut remaining = 64 * 1024;
    let mut out_done = false;
    let mut err_done = false;
    loop {
        if cancel.is_cancelled() {
            return Err("query cancelled".into());
        }
        if started.elapsed() >= Duration::from_secs(3).min(cfg.command_timeout) {
            return Err("help query timed out".into());
        }
        if !out_done {
            out_done = drain(&mut stdout, &mut out, &mut remaining)?;
        }
        if !err_done {
            err_done = drain(&mut stderr, &mut err, &mut remaining)?;
        }
        if out.len() + err.len() >= 64 * 1024 {
            return Ok(format!(
                "[query output truncated]\n{}",
                tools::truncate_middle(
                    &format!(
                        "{}\n{}",
                        String::from_utf8_lossy(&out),
                        String::from_utf8_lossy(&err)
                    ),
                    1800
                )
                .0
            ));
        }
        // Keep the child unreaped while pipes may still belong to descendants,
        // so cancellation always targets our original process group.
        if out_done
            && err_done
            && let Some(status) = child.child.try_wait().map_err(|e| e.to_string())?
        {
            child.reaped = true;
            let body = format!(
                "{}\n{}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&err)
            );
            let body = if let Some(topic) = &topic {
                let selected = body
                    .lines()
                    .filter(|line| line.to_lowercase().contains(topic))
                    .collect::<Vec<_>>()
                    .join("\n");
                if selected.is_empty() {
                    "[no help lines match topic]".into()
                } else {
                    selected
                }
            } else {
                body
            };
            let (body, truncated) = tools::truncate_middle(&body, 1800);
            let selection = topic
                .as_ref()
                .map(|topic| format!(" topic={}", json!(topic)))
                .unwrap_or_default();
            return Ok(format!(
                "[query program={} exit={} truncated={}{selection}]\n{body}",
                path.display(),
                status.code().map_or("signal".into(), |c| c.to_string()),
                truncated
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn nonblocking(pipe: &impl AsRawFd) -> Result<(), String> {
    // SAFETY: the descriptor belongs to a live pipe owned by this query.
    let flags = unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_GETFL) };
    if flags < 0
        || unsafe { libc::fcntl(pipe.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0
    {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(())
}

fn drain(pipe: &mut impl Read, bytes: &mut Vec<u8>, remaining: &mut usize) -> Result<bool, String> {
    let mut buffer = [0; 4096];
    for _ in 0..4 {
        if *remaining == 0 {
            return Ok(false);
        }
        let limit = buffer.len().min(*remaining);
        match pipe.read(&mut buffer[..limit]) {
            Ok(0) => return Ok(true),
            Ok(n) => {
                bytes.extend_from_slice(&buffer[..n]);
                *remaining -= n;
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => return Ok(false),
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("query output read failed: {e}")),
        }
    }
    Ok(false)
}

struct OwnedQuery {
    child: Child,
    reaped: bool,
}

impl Drop for OwnedQuery {
    fn drop(&mut self) {
        if self.reaped {
            return;
        }
        // SAFETY: this process group was created exclusively for the owned query.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        if let Err(error) = self.child.wait() {
            eprintln!("nosh: help query cleanup: {error}");
        }
    }
}
