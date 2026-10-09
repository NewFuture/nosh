//! Bounded help for literal command names and subcommands; never executes shell text.

use std::io::{ErrorKind, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

use nosh_engine::{CancelHandle, ToolCall, ToolSpec};
use nosh_permissions::{
    Context, Decision, Risk, SessionAllowList, assess_command_with_lookup, assess_read, evaluate,
};
use nosh_shell::{CommandSnapshot, Resolution};
use serde_json::json;

use crate::AgentConfig;

mod help;

pub(crate) fn spec() -> ToolSpec {
    ToolSpec {
        name: "command_help".into(),
        description: "Get usage and option help for a command.".into(),
        parameters: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Command name or path, optionally with subcommands, e.g. tar or git commit."},
                "query": {"type": "string", "description": "Literal text to find, ignoring case, e.g. gz or -C. Omit for an overview."}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
    }
}

struct HelpCommand<'a> {
    name: &'a str,
    program: &'a str,
    subcommands: Vec<&'a str>,
    flag: &'static str,
}

impl<'a> HelpCommand<'a> {
    fn parse(name: &'a str) -> Result<Self, String> {
        if name.len() > 512
            || name
                .chars()
                .any(|c| c.is_control() || nosh_shell::style::is_hidden(c))
        {
            return Err("invalid command name".into());
        }
        let mut words = name.split_whitespace();
        let program = words.next().ok_or("empty command name")?;
        if program.starts_with('-')
            || !program
                .chars()
                .all(|c| c.is_alphanumeric() || "_-./+".contains(c))
        {
            return Err("name must be a command name or path followed by plain subcommand names, not shell code".into());
        }
        let subcommands: Vec<_> = words.collect();
        if subcommands.iter().any(|word| {
            word.starts_with('-')
                || !word
                    .chars()
                    .all(|c| c.is_alphanumeric() || "_-".contains(c))
        }) {
            return Err(
                "subcommands must be plain names, without options, paths or shell syntax".into(),
            );
        }
        // Git's --help launches a documentation viewer; -h prints builtin usage.
        let flag = if !subcommands.is_empty()
            && Path::new(program)
                .file_name()
                .and_then(|name| name.to_str())
                == Some("git")
        {
            "-h"
        } else {
            "--help"
        };
        Ok(Self {
            name,
            program,
            subcommands,
            flag,
        })
    }

    fn arguments(&self) -> impl Iterator<Item = &str> {
        self.subcommands
            .iter()
            .copied()
            .chain(std::iter::once(self.flag))
    }
}

pub(crate) fn query(
    commands: &CommandSnapshot,
    context: &Context,
    cfg: &AgentConfig,
    call: &ToolCall,
    cancel: &CancelHandle,
) -> Result<String, String> {
    if call.args.keys().any(|key| key != "name" && key != "query") {
        return Err(format!("unknown {} parameter", call.name));
    }
    let target = HelpCommand::parse(call.str_arg("name").ok_or("invalid command name")?)?;
    let query = match call.args.get("query") {
        None => None,
        Some(serde_json::Value::String(query))
            if !query.trim().is_empty()
                && query.len() <= 120
                && !query.chars().any(|c| c.is_control() || nosh_shell::style::is_hidden(c)) =>
        {
            Some(query.trim())
        }
        _ => return Err("query must be a nonempty help keyword, phrase or option of at most 120 bytes, without control characters".into()),
    };
    let path = match commands.resolve(target.program) {
        Resolution::File(path) => path,
        Resolution::NotFound => return Err(format!("command not found: {}", target.program)),
        Resolution::Alias(_) => {
            return Err("help unavailable for alias; no program was executed".into());
        }
        Resolution::Function => {
            return Err("help unavailable for function; no program was executed".into());
        }
        Resolution::Builtin | Resolution::Keyword => {
            return Err(
                "help unavailable for shell builtin/keyword; no external substitute was executed"
                    .into(),
            );
        }
    };
    let executable = path
        .canonicalize()
        .map_err(|error| format!("cannot resolve query program: {error}"))?;
    if assess_read(&call.name, &path, None, context).reads_protected {
        return Err("help program is on a protected path; no program was executed".into());
    }
    let quoted = std::iter::once(target.program)
        .chain(target.arguments())
        .map(|word| format!("'{}'", word.replace('\'', "'\\''")))
        .collect::<Vec<_>>()
        .join(" ");
    let report = assess_command_with_lookup(&quoted, context, &|candidate, _, _, _| {
        (candidate == target.program).then(|| executable.clone())
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
        return Err("help query is not authorized; no program was executed".into());
    }
    if cancel.is_cancelled() {
        return Err("query cancelled".into());
    }
    let mut command = Command::new(&executable);
    command
        .arg0(&path)
        .args(target.arguments())
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
    let output = capture(&mut command, cfg.command_timeout, cancel)?;
    help::render(&target, &path, &executable, &output, query)
}

struct Captured {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    status: Option<ExitStatus>,
    complete: bool,
}

fn capture(
    command: &mut Command,
    timeout: Duration,
    cancel: &CancelHandle,
) -> Result<Captured, String> {
    if cancel.is_cancelled() {
        return Err("query cancelled".into());
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
        if started.elapsed() >= Duration::from_secs(3).min(timeout) {
            return Err("help query timed out".into());
        }
        if !out_done {
            out_done = drain(&mut stdout, &mut out, &mut remaining)?;
        }
        if !err_done {
            err_done = drain(&mut stderr, &mut err, &mut remaining)?;
        }
        if out.len() + err.len() >= 64 * 1024 {
            return Ok(Captured {
                stdout: out,
                stderr: err,
                status: None,
                complete: false,
            });
        }
        // Keep the child unreaped while pipes may still belong to descendants,
        // so cancellation always targets our original process group.
        if out_done
            && err_done
            && let Some(status) = child.child.try_wait().map_err(|e| e.to_string())?
        {
            child.reaped = true;
            return Ok(Captured {
                stdout: out,
                stderr: err,
                status: Some(status),
                complete: true,
            });
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

#[cfg(test)]
mod tests;
