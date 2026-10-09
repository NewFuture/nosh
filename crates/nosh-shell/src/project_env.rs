//! Explicit project environment integration, outside the shared shell lock.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use nosh_platform::fs::FileStamp;
use nosh_platform::tr;

use crate::input_assist::worker::OwnedChild;

pub(crate) const MAX_ENV: usize = 4 * 1024 * 1024;
const MAX_DIAGNOSTIC: usize = 64 * 1024;
const MAX_VARIABLES: usize = 16_384;
pub(crate) const TIMEOUT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(20);
const MISE_UNTRUSTED: &str = "__MISE_LAST_UNTRUSTED_CONFIG_WARNING_KEY";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Provider {
    #[default]
    Off,
    Direnv,
    Mise,
}

impl Provider {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "off" => Some(Self::Off),
            "direnv" => Some(Self::Direnv),
            "mise" => Some(Self::Mise),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Direnv => "direnv",
            Self::Mise => "mise",
        }
    }
}

pub(crate) type Patch = BTreeMap<String, Option<String>>;

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Snapshot {
    pub generation: u64,
    pub cwd: PathBuf,
    pub exported: BTreeMap<String, String>,
}

impl Snapshot {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.exported.len() > MAX_VARIABLES
            || self
                .exported
                .iter()
                .try_fold(0usize, |total, (key, value)| {
                    total.checked_add(key.len() + value.len() + 32)
                })
                .is_none_or(|size| size > MAX_ENV)
        {
            return Err("project environment snapshot limit exceeded".into());
        }
        if self
            .exported
            .iter()
            .any(|(key, value)| key.contains(['\0', '=']) || value.contains('\0'))
        {
            return Err("invalid exported environment".into());
        }
        Ok(())
    }
}

struct Job {
    snapshot: Snapshot,
    cancelled: Arc<AtomicBool>,
    verified: Arc<AtomicBool>,
    published: Arc<AtomicBool>,
    thread: JoinHandle<ToolResult>,
    started: Instant,
}

struct ToolResult {
    program: PathBuf,
    stamp: Option<FileStamp>,
    result: Result<Patch, String>,
}

pub(crate) struct Completed {
    pub snapshot: Snapshot,
    pub result: Result<Patch, String>,
}

pub(crate) struct Service {
    pub provider: Provider,
    job: Option<Job>,
    cleanup_fault: bool,
    verified_program: Option<(PathBuf, FileStamp)>,
    attempt_cwd: Option<PathBuf>,
    pub error: Option<String>,
    reported: Option<String>,
    pub ready: Option<Snapshot>,
    pub wakeup: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl Default for Service {
    fn default() -> Self {
        Self::new(Provider::Off)
    }
}

impl Service {
    pub fn new(provider: Provider) -> Self {
        Self {
            provider,
            job: None,
            cleanup_fault: false,
            verified_program: None,
            attempt_cwd: None,
            error: None,
            reported: None,
            ready: None,
            wakeup: None,
        }
    }

    pub fn occupied(&self) -> bool {
        self.job.is_some() || self.cleanup_fault
    }

    pub fn job_cancelled(&self) -> bool {
        self.cleanup_fault
            || self
                .job
                .as_ref()
                .is_some_and(|job| job.cancelled.load(Ordering::Acquire))
    }

    pub fn completed(&self) -> bool {
        self.job
            .as_ref()
            .is_some_and(|job| job.thread.is_finished())
    }

    pub fn notify_if_published(&self) {
        if self
            .job
            .as_ref()
            .is_some_and(|job| job.published.load(Ordering::Acquire))
            && let Some(wakeup) = &self.wakeup
        {
            wakeup();
        }
    }

    pub fn start(&mut self, snapshot: Snapshot, program: PathBuf) -> Result<(), String> {
        if self.occupied() {
            return Err("previous environment task is still being reclaimed".into());
        }
        snapshot.validate()?;
        let input = snapshot.clone();
        let cached = self.verified_program.clone();
        let executable = cached
            .as_ref()
            .map(|(path, _)| path.clone())
            .unwrap_or(program);
        let provider = self.provider;
        let force = self.error.is_some() && self.attempt_cwd.as_ref() == Some(&snapshot.cwd);
        self.attempt_cwd = Some(snapshot.cwd.clone());
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel = cancelled.clone();
        let verified = Arc::new(AtomicBool::new(false));
        let version = verified.clone();
        let published = Arc::new(AtomicBool::new(false));
        let ready = published.clone();
        let wakeup = self.wakeup.clone();
        let started = Instant::now();
        let thread = std::thread::Builder::new()
            .name("nosh-project-env".into())
            .spawn(move || {
                let program = if executable.is_absolute() {
                    Some(executable.clone())
                } else {
                    input.exported.get("PATH").and_then(|path| {
                        path.split(':')
                            .map(|part| input.cwd.join(part).join(&executable))
                            .find(|path| crate::backend::is_executable(path))
                    })
                };
                let output = match program {
                    Some(program) => {
                        let stamp = FileStamp::of(&program);
                        let result = if stamp.is_some() {
                            version.store(cached.as_ref().is_some_and(|(path, previous)| {
                                path == &program && Some(previous) == stamp.as_ref()
                            }), Ordering::Release);
                            refresh(provider, &program, &input, &cancel, &version, force, started)
                        } else {
                            Err("cannot read environment executable identity".into())
                        };
                        let result = if started.elapsed() >= TIMEOUT {
                            Err("project environment deadline exceeded; previous environment retained".into())
                        } else {
                            result
                        };
                        ToolResult { program, stamp, result }
                    }
                    None => ToolResult {
                        program: executable,
                        stamp: None,
                        result: Err(format!(
                            "{} was explicitly enabled but is not executable on PATH",
                            provider.name()
                        )),
                    },
                };
                ready.store(true, Ordering::Release);
                if let Some(wakeup) = wakeup {
                    wakeup();
                }
                output
            })
            .map_err(|error| format!("cannot start environment task: {error}"))?;
        self.job = Some(Job {
            snapshot,
            cancelled,
            verified,
            published,
            thread,
            started,
        });
        Ok(())
    }

    pub fn cancel(&mut self) {
        if let Some(job) = &self.job {
            job.cancelled.store(true, Ordering::Release);
            self.error =
                Some("project environment refresh cancelled; previous environment retained".into());
        }
    }

    pub fn poll(&mut self) -> Option<Completed> {
        let job = self.job.as_ref()?;
        if !job.thread.is_finished() && job.started.elapsed() >= TIMEOUT {
            job.cancelled.store(true, Ordering::Release);
            self.error =
                Some("project environment deadline exceeded; previous environment retained".into());
        }
        if !job.thread.is_finished() {
            return None;
        }
        let job = self.job.take().expect("occupied environment task");
        let result = match job.thread.join() {
            Ok(output) => {
                if job.verified.load(Ordering::Acquire)
                    && let Some(stamp) = output.stamp
                {
                    self.verified_program = Some((output.program, stamp));
                }
                if job.cancelled.load(Ordering::Acquire) {
                    Err(self
                        .error
                        .clone()
                        .unwrap_or_else(|| "environment refresh cancelled".into()))
                } else {
                    output.result
                }
            }
            Err(_) => {
                self.cleanup_fault = true;
                Err(
                    "environment task panicked; cleanup cannot be verified, restart the session"
                        .into(),
                )
            }
        };
        Some(Completed {
            snapshot: job.snapshot,
            result,
        })
    }

    pub fn report(&mut self) {
        if let Some(error) = &self.error {
            if self.reported.as_ref() != Some(error) {
                eprintln!(
                    "nosh: {}: {} ({}; {}: {})",
                    self.provider.name(),
                    crate::style::visible_text(error),
                    tr!(
                        "当前项目环境未生效；人工可继续，agent 已阻断",
                        "project environment is not ready; manual commands remain available, agent execution is blocked"
                    ),
                    tr!("上次验证的项目", "last verified project"),
                    self.ready.as_ref().map_or_else(
                        || tr!("无，保留原环境", "none; original environment retained").into(),
                        |snapshot| crate::style::visible_text(&snapshot.cwd.to_string_lossy())
                            .into_owned()
                    )
                );
                self.reported = Some(error.clone());
            }
        } else {
            self.reported = None;
        }
    }

    pub fn label(&self) -> Option<String> {
        if self.error.is_some() {
            Some(format!(
                "{}: {}",
                self.provider.name(),
                tr!("环境未生效", "environment not ready")
            ))
        } else if self.occupied() {
            Some(format!(
                "{}: {}",
                self.provider.name(),
                tr!("环境加载中", "loading environment")
            ))
        } else if self.provider != Provider::Off && self.ready.is_none() {
            Some(format!(
                "{}: {}",
                self.provider.name(),
                tr!("环境待刷新", "environment pending")
            ))
        } else if self.provider != Provider::Off {
            Some(format!(
                "{}: {}",
                self.provider.name(),
                tr!("环境就绪", "environment ready")
            ))
        } else {
            None
        }
    }
}

impl Drop for Service {
    fn drop(&mut self) {
        self.cancel();
        if let Some(job) = self.job.take() {
            // Give owned process cleanup a bounded opportunity before the CLI
            // exits. A stuck spawn/reaper must never make exit wait indefinitely.
            let until = Instant::now() + Duration::from_millis(250);
            while !job.thread.is_finished() && Instant::now() < until {
                std::thread::sleep(Duration::from_millis(5));
            }
            if job.thread.is_finished() {
                if job.thread.join().is_err() {
                    eprintln!("nosh: environment cleanup task panicked");
                }
            } else {
                eprintln!(
                    "nosh: environment cleanup remains pending; supervisor still owns the task"
                );
            }
        }
    }
}

fn refresh(
    provider: Provider,
    program: &Path,
    snapshot: &Snapshot,
    cancel: &AtomicBool,
    verified: &AtomicBool,
    force: bool,
    started: Instant,
) -> Result<Patch, String> {
    let run = |args: &[&str]| run_tool(provider, program, args, snapshot, cancel, started);
    match provider {
        Provider::Off => Ok(Patch::new()),
        Provider::Direnv => {
            verified.store(true, Ordering::Release);
            if snapshot
                .exported
                .get("DIRENV_DISABLE")
                .is_some_and(|v| matches!(v.as_str(), "1" | "true"))
            {
                return Err("direnv is disabled by DIRENV_DISABLE".into());
            }
            let bytes = run(&["export", "json"])?;
            if bytes.iter().all(u8::is_ascii_whitespace) {
                return if force {
                    Err("direnv returned no update after failure; repair the configuration and run direnv reload".into())
                } else {
                    Ok(Patch::new())
                };
            }
            let patch: Patch = serde_json::from_slice(&bytes)
                .map_err(|_| "invalid direnv JSON environment delta".to_string())?;
            validate_patch(&patch)?;
            Ok(patch)
        }
        Provider::Mise => {
            if !verified.load(Ordering::Acquire) {
                let version = run(&["--version"])?;
                if version.split(u8::is_ascii_whitespace).next() != Some(b"2026.10.5".as_slice()) {
                    return Err(
                        "unsupported mise environment protocol; tested version is 2026.10.5".into(),
                    );
                }
                verified.store(true, Ordering::Release);
            }
            let args = if force {
                vec!["hook-env", "-s", "nu", "--force"]
            } else {
                vec!["hook-env", "-s", "nu"]
            };
            let patch = mise_delta(&run(&args)?)?;
            let untrusted = patch
                .get(MISE_UNTRUSTED)
                .map(|v| v.as_deref())
                .unwrap_or_else(|| snapshot.exported.get(MISE_UNTRUSTED).map(String::as_str));
            if untrusted.is_some_and(|v| !v.is_empty()) {
                return Err(
                    "mise configuration is not trusted; review it and use mise trust explicitly"
                        .into(),
                );
            }
            #[derive(serde::Deserialize)]
            struct Tool {
                installed: bool,
                active: bool,
            }
            let tools: BTreeMap<String, Vec<Tool>> =
                serde_json::from_slice(&run(&["ls", "--current", "--json"])?)
                    .map_err(|_| "invalid mise tool-status JSON".to_string())?;
            if tools
                .values()
                .flatten()
                .any(|tool| !tool.installed || !tool.active)
            {
                return Err(
                    "mise requested tools are missing or inactive; install them explicitly".into(),
                );
            }
            if force && patch.is_empty() {
                return Err("mise returned no verified update after failure".into());
            }
            Ok(patch)
        }
    }
}

fn mise_delta(bytes: &[u8]) -> Result<Patch, String> {
    if !complete_csv(bytes) {
        return Err("incomplete or malformed mise CSV environment delta".into());
    }
    let mut reader = csv::ReaderBuilder::new()
        .has_headers(false)
        .from_reader(bytes);
    let mut patch = Patch::new();
    for record in reader.records() {
        let record = record.map_err(|_| "invalid mise CSV environment delta".to_string())?;
        if record.len() != 3 {
            return Err("unexpected mise environment operation".into());
        }
        let value = match &record[0] {
            "set" => Some(record[2].to_string()),
            "hide" if record[2].is_empty() => None,
            _ => {
                return Err(
                    "unsupported mise output; only environment set/hide is supported".into(),
                );
            }
        };
        patch.insert(record[1].to_string(), value);
        if patch.len() > MAX_VARIABLES {
            return Err("environment variable count limit exceeded".into());
        }
    }
    validate_patch(&patch)?;
    Ok(patch)
}

// csv deliberately accepts some malformed quoting/EOF cases. Tool output must
// instead contain complete records before any of its environment is installed.
fn complete_csv(bytes: &[u8]) -> bool {
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Field {
        Start,
        Bare,
        Quoted,
        Closed,
    }
    let mut field = Field::Start;
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'\r' && field != Field::Quoted {
            if bytes.get(index + 1) != Some(&b'\n') {
                return false;
            }
            continue;
        }
        field = match (field, byte) {
            (Field::Quoted, b'"') => Field::Closed,
            (Field::Quoted, _) | (Field::Closed, b'"') => Field::Quoted,
            (_, b',' | b'\n') => Field::Start,
            (Field::Start, b'"') => Field::Quoted,
            (Field::Closed, _) | (Field::Bare, b'"') => return false,
            _ => Field::Bare,
        };
    }
    bytes.is_empty() || (bytes.last() == Some(&b'\n') && field == Field::Start)
}

pub(crate) fn validate_patch(patch: &Patch) -> Result<(), String> {
    let mut size = 0usize;
    for (key, value) in patch {
        let mut chars = key.chars();
        if !chars
            .next()
            .is_some_and(|c| c == '_' || c.is_ascii_alphabetic())
            || !chars.all(|c| c == '_' || c.is_ascii_alphanumeric())
            || value.as_ref().is_some_and(|v| v.contains('\0'))
        {
            return Err("invalid project environment variable".into());
        }
        if matches!(
            key.as_str(),
            "PWD"
                | "OLDPWD"
                | "SHELLOPTS"
                | "BASHOPTS"
                | "BASH_ENV"
                | "ENV"
                | "NOSH_AGENT_RUN"
                | "PROMPT_COMMAND"
                | "PS0"
                | "PS1"
                | "PS2"
                | "PS3"
                | "PS4"
        ) {
            return Err(format!("project environment cannot modify {key}"));
        }
        size = size
            .checked_add(key.len() + value.as_ref().map_or(0, String::len) + 32)
            .ok_or("environment delta size overflow")?;
        if size > MAX_ENV || patch.len() > MAX_VARIABLES {
            return Err("environment delta limit exceeded".into());
        }
    }
    Ok(())
}

fn nonblocking(fd: i32) -> io::Result<()> {
    // SAFETY: only flags of a retained pipe reader are changed.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn drain(reader: &mut impl Read, buffer: &mut Vec<u8>, cap: usize) -> Result<(), String> {
    let mut chunk = [0; 8192];
    for _ in 0..8 {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if buffer.len() + n > cap {
                    return Err("environment tool output limit exceeded".into());
                }
                buffer.extend_from_slice(&chunk[..n]);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(format!("cannot read environment tool output: {error}")),
        }
    }
    Ok(())
}

fn run_tool(
    provider: Provider,
    program: &Path,
    args: &[&str],
    snapshot: &Snapshot,
    cancel: &AtomicBool,
    started: Instant,
) -> Result<Vec<u8>, String> {
    if cancel.load(Ordering::Acquire) || started.elapsed() >= TIMEOUT {
        return Err("environment refresh cancelled or timed out".into());
    }
    static RUNS: AtomicU64 = AtomicU64::new(0);
    let mut nonce = [0u8; 16];
    // SAFETY: fills a writable buffer before spawning any process.
    if unsafe { libc::getentropy(nonce.as_mut_ptr().cast(), nonce.len()) } != 0 {
        return Err(format!(
            "cannot identify environment process: {}",
            io::Error::last_os_error()
        ));
    }
    let marker = format!(
        "environment.{}.{}.{:032x}",
        std::process::id(),
        RUNS.fetch_add(1, Ordering::Relaxed),
        u128::from_ne_bytes(nonce)
    );
    let (mut stdout, out) = std::io::pipe().map_err(|e| e.to_string())?;
    let (mut stderr, err) = std::io::pipe().map_err(|e| e.to_string())?;
    nonblocking(stdout.as_raw_fd())
        .and_then(|_| nonblocking(stderr.as_raw_fd()))
        .map_err(|e| e.to_string())?;
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&snapshot.cwd)
        .env_clear()
        .envs(&snapshot.exported)
        .env(crate::procs::RUN_VAR, &marker)
        .stdin(Stdio::null())
        .stdout(out)
        .stderr(err);
    if provider == Provider::Mise {
        command
            .env("MISE_NO_HOOKS", "1")
            .env("MISE_OFFLINE", "1")
            .env("MISE_AUTO_INSTALL", "0")
            .env("MISE_NOT_FOUND_AUTO_INSTALL", "0")
            .env("MISE_EXEC_AUTO_INSTALL", "0")
            .env("MISE_EXPERIMENTAL", "0");
    }
    // SAFETY: only async-signal-safe process operations run between fork/exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            #[cfg(target_os = "linux")]
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .map_err(|e| format!("cannot start {}: {e}", provider.name()))?;
    drop(command);
    let mut child = OwnedChild::new(child, Some(marker));
    let mut output = Vec::new();
    let mut diagnostics = Vec::new();
    let result = (|| {
        loop {
            if cancel.load(Ordering::Acquire) || started.elapsed() >= TIMEOUT {
                return Err("environment refresh cancelled or timed out".into());
            }
            drain(&mut stdout, &mut output, MAX_ENV)?;
            drain(&mut stderr, &mut diagnostics, MAX_DIAGNOSTIC)?;
            if let Some(code) = child
                .exit_code()
                .map_err(|e| format!("environment process status: {e}"))?
            {
                // Continue draining all buffered bytes; a large result can span
                // multiple bounded reads even after its process has exited.
                loop {
                    let before = output.len() + diagnostics.len();
                    drain(&mut stdout, &mut output, MAX_ENV)?;
                    drain(&mut stderr, &mut diagnostics, MAX_DIAGNOSTIC)?;
                    if output.len() + diagnostics.len() == before {
                        break;
                    }
                }
                return if code == 0 {
                    Ok(())
                } else {
                    Err(format!(
                        "{} {} failed (exit {code}); review the tool's configuration/authorization",
                        provider.name(),
                        args[0]
                    ))
                };
            }
            std::thread::sleep(POLL);
        }
    })();
    let mut last_error = None;
    loop {
        match child.retire() {
            Ok(true) => break,
            Ok(false) => {}
            Err(error) => {
                let message = error.to_string();
                if last_error.as_ref() != Some(&message) {
                    eprintln!(
                        "nosh: environment process cleanup: {}",
                        crate::style::visible_text(&message)
                    );
                    last_error = Some(message);
                }
            }
        }
        std::thread::sleep(POLL);
    }
    result.map(|()| output)
}

#[cfg(test)]
mod tests;
