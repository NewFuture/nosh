//! [`EmbeddedShell`]: one brush `Shell` shared by the user and the agent.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use brush_core::openfiles::{self, OpenFile, OpenFiles};
use brush_core::{ExecutionControlFlow, ShellVariable, SourceInfo};

use crate::procs;

pub type BrushShell = brush_core::Shell;

#[derive(Debug, thiserror::Error)]
pub enum ShellError {
    #[error("shell error: {0}")]
    Brush(#[from] brush_core::Error),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

#[derive(Debug, Clone, Default)]
pub struct ShellOptions {
    pub interactive: bool,
    pub login: bool,
    /// Load `~/.bashrc` (interactive) / profiles (login).
    pub load_rc: bool,
    pub rc_file: Option<PathBuf>,
    /// `$0`.
    pub name: Option<String>,
    /// Positional parameters (`$1…`).
    pub args: Vec<String>,
    /// `-c` mode.
    pub command_string_mode: bool,
    pub working_dir: Option<PathBuf>,
    /// Turn SIGINT into [`Interrupts`] instead of dying, and let builtin-only
    /// loops be interrupted (implied by `interactive`; set for agent shells).
    pub catch_sigint: bool,
    /// `-e`, `-x`, `-u`.
    pub errexit: bool,
    pub xtrace: bool,
    pub nounset: bool,
}

/// Options for one agent command.
#[derive(Debug, Clone)]
pub struct AgentExecOpts {
    pub timeout: Duration,
    /// Bytes kept per stream (design: 10 MB).
    pub capture_limit: usize,
}

impl Default for AgentExecOpts {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            capture_limit: 10 * 1024 * 1024,
        }
    }
}

/// Receives agent command output as it arrives (for live display).
pub trait OutputSink {
    fn stdout(&mut self, chunk: &str);
    fn stderr(&mut self, chunk: &str);
}

/// Discards output.
pub struct NullSink;

impl OutputSink for NullSink {
    fn stdout(&mut self, _: &str) {}
    fn stderr(&mut self, _: &str) {}
}

#[derive(Debug, Clone, Default)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration: Duration,
    pub timed_out: bool,
    pub interrupted: bool,
    /// The command tried to read the terminal and was stopped (SIGTTIN).
    pub needed_terminal: bool,
    pub truncated: bool,
    pub diff: StateDiff,
}

/// A point-in-time view of session state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionState {
    pub cwd: PathBuf,
    pub vars: BTreeMap<String, String>,
    pub exported: BTreeSet<String>,
    pub functions: BTreeSet<String>,
    pub aliases: BTreeMap<String, String>,
    pub last_exit: i32,
}

impl SessionState {
    pub fn var(&self, name: &str) -> Option<&str> {
        self.vars.get(name).map(String::as_str)
    }

    /// Basename of the active Python virtualenv, if any.
    pub fn venv(&self) -> Option<String> {
        self.var("VIRTUAL_ENV").map(|v| {
            Path::new(v)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| v.to_string())
        })
    }
}

const VOLATILE: &[&str] = &[
    "_",
    "RANDOM",
    "SRANDOM",
    "SECONDS",
    "LINENO",
    "EPOCHREALTIME",
    "EPOCHSECONDS",
    "BASHPID",
    "BASH_COMMAND",
    "PIPESTATUS",
    "HISTCMD",
    "OLDPWD",
    "PWD",
    "COLUMNS",
    "LINES",
    "BASH_LINENO",
    "FUNCNAME",
    "BASH_SOURCE",
    "DIRSTACK",
    "BASH_ARGC",
    "BASH_ARGV",
    "BASH_SUBSHELL",
    "SHLVL",
    "BRUSH_PS_ALT",
    "COMP_WORDBREAKS",
];

/// What an agent command changed in the session.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct StateDiff {
    pub cwd: Option<(PathBuf, PathBuf)>,
    pub path_changed: bool,
    pub venv: Option<(Option<String>, Option<String>)>,
    pub vars_set: Vec<String>,
    pub vars_changed: Vec<String>,
    pub vars_unset: Vec<String>,
    pub funcs_added: Vec<String>,
    pub funcs_removed: Vec<String>,
    pub aliases_changed: Vec<String>,
}

impl StateDiff {
    pub fn between(a: &SessionState, b: &SessionState) -> Self {
        let mut d = StateDiff::default();
        if a.cwd != b.cwd {
            d.cwd = Some((a.cwd.clone(), b.cwd.clone()));
        }
        d.path_changed = a.var("PATH") != b.var("PATH");
        if a.venv() != b.venv() {
            d.venv = Some((a.venv(), b.venv()));
        }
        for (k, v) in &b.vars {
            if VOLATILE.contains(&k.as_str()) || k == "PATH" || k == "VIRTUAL_ENV" {
                continue;
            }
            match a.vars.get(k) {
                None => d.vars_set.push(k.clone()),
                Some(old) if old != v => d.vars_changed.push(k.clone()),
                _ => {}
            }
        }
        for k in a.vars.keys() {
            if !b.vars.contains_key(k) && !VOLATILE.contains(&k.as_str()) && k != "VIRTUAL_ENV" {
                d.vars_unset.push(k.clone());
            }
        }
        d.funcs_added = b.functions.difference(&a.functions).cloned().collect();
        d.funcs_removed = a.functions.difference(&b.functions).cloned().collect();
        for (k, v) in &b.aliases {
            if a.aliases.get(k) != Some(v) {
                d.aliases_changed.push(k.clone());
            }
        }
        for k in a.aliases.keys() {
            if !b.aliases.contains_key(k) {
                d.aliases_changed.push(k.clone());
            }
        }
        d
    }

    pub fn is_empty(&self) -> bool {
        *self == StateDiff::default()
    }

    /// One line, e.g. `cwd: /a → /b; PATH modified; vars: +FOO ~BAR -BAZ`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some((a, b)) = &self.cwd {
            parts.push(format!("cwd: {} → {}", a.display(), b.display()));
        }
        if self.path_changed {
            parts.push("PATH modified".to_string());
        }
        if let Some((a, b)) = &self.venv {
            parts.push(format!(
                "venv: {} → {}",
                a.as_deref().unwrap_or("none"),
                b.as_deref().unwrap_or("none")
            ));
        }
        let mut vars: Vec<String> = Vec::new();
        vars.extend(self.vars_set.iter().map(|v| format!("+{v}")));
        vars.extend(self.vars_changed.iter().map(|v| format!("~{v}")));
        vars.extend(self.vars_unset.iter().map(|v| format!("-{v}")));
        if !vars.is_empty() {
            parts.push(format!("vars: {}", vars.join(" ")));
        }
        let mut funcs: Vec<String> = self.funcs_added.iter().map(|f| format!("+{f}")).collect();
        funcs.extend(self.funcs_removed.iter().map(|f| format!("-{f}")));
        if !funcs.is_empty() {
            parts.push(format!("functions: {}", funcs.join(" ")));
        }
        if !self.aliases_changed.is_empty() {
            parts.push(format!("aliases: {}", self.aliases_changed.join(" ")));
        }
        parts.join("; ")
    }
}

/// How a command name resolves in the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolution {
    Alias(String),
    Function,
    Builtin,
    Keyword,
    File(PathBuf),
    NotFound,
}

/// A command the user ran (for the `[recent]` task header).
#[derive(Debug, Clone)]
pub struct UserCommand {
    pub line: String,
    pub exit: i32,
    pub duration: Duration,
}

/// Result of a user command line.
#[derive(Debug, Clone, Copy)]
pub struct UserRun {
    pub exit_code: i32,
    pub exit_shell: bool,
}

/// Environment variables set only for the duration of one agent command.
pub const ANTI_HANG_ENV: &[(&str, &str)] = &[
    ("PAGER", "cat"),
    ("GIT_PAGER", "cat"),
    ("MANPAGER", "cat"),
    ("SYSTEMD_PAGER", ""),
    ("GIT_TERMINAL_PROMPT", "0"),
    ("GIT_EDITOR", "true"),
    ("EDITOR", "true"),
    ("VISUAL", "true"),
    ("DEBIAN_FRONTEND", "noninteractive"),
    ("PIP_NO_INPUT", "1"),
    ("NO_COLOR", "1"),
    ("TERM", "dumb"),
    ("HOMEBREW_NO_AUTO_UPDATE", "1"),
    ("PYTHONUNBUFFERED", "1"),
];

/// SIGINT counter for interactive sessions (the shell ignores SIGINT itself).
#[derive(Default)]
pub struct Interrupts {
    count: AtomicU64,
    hooks: Mutex<Vec<Box<dyn Fn() + Send + Sync>>>,
}

impl Interrupts {
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::SeqCst)
    }

    /// Registers a callback run on every SIGINT (e.g. cancel generation).
    pub fn on_interrupt(&self, f: impl Fn() + Send + Sync + 'static) {
        self.hooks.lock().unwrap().push(Box::new(f));
    }

    pub fn fire(&self) {
        self.count.fetch_add(1, Ordering::SeqCst);
        for h in self.hooks.lock().unwrap().iter() {
            h();
        }
    }
}

pub struct EmbeddedShell {
    rt: Arc<tokio::runtime::Runtime>,
    shell: Arc<Mutex<BrushShell>>,
    interactive: bool,
    workspace: PathBuf,
    interrupts: Arc<Interrupts>,
    recent: Vec<UserCommand>,
    path_cache: Option<(String, Arc<Vec<String>>)>,
    path_scan: PathScan,
}

impl EmbeddedShell {
    pub fn new(opts: ShellOptions) -> Result<Self, ShellError> {
        let rt = Arc::new(
            tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .thread_name("nosh-shell")
                .enable_all()
                .build()?,
        );
        let name = opts.name.clone().unwrap_or_else(|| "nosh".to_string());
        let histfile = (opts.interactive && std::env::var_os("HISTFILE").is_none())
            .then(|| nosh_hub::paths::state_dir().join("shell_history"));
        let mut builtins = brush_builtins::default_builtins(brush_builtins::BuiltinSet::BashMode);
        if opts.interactive || opts.catch_sigint {
            crate::yielding::wrap(&mut builtins);
        }
        let shell = rt.block_on(async {
            let mut builder = brush_core::Shell::builder()
                .interactive(opts.interactive)
                .login(opts.login)
                .no_editing(true)
                .command_string_mode(opts.command_string_mode)
                .profile(brush_core::ProfileLoadBehavior::Skip)
                .rc(brush_core::RcLoadBehavior::Skip)
                .shell_name(name)
                .shell_args(opts.args.clone())
                .shell_version(env!("CARGO_PKG_VERSION").to_string())
                .shell_product_display_str(format!("nosh {}", env!("CARGO_PKG_VERSION")))
                .maybe_working_dir(opts.working_dir.clone())
                .exit_on_nonzero_command_exit(opts.errexit)
                .print_commands_and_arguments(opts.xtrace)
                .treat_unset_variables_as_error(opts.nounset)
                .builtins(builtins);
            if let Some(h) = histfile {
                if let Some(parent) = h.parent() {
                    let _ = nosh_hub::paths::ensure_private_dir(parent);
                }
                builder = builder.var("HISTFILE", ShellVariable::new(h.to_string_lossy().as_ref()));
            }
            builder.build().await
        })?;
        let workspace = shell.working_dir().to_path_buf();
        let mut me = Self {
            rt,
            shell: Arc::new(Mutex::new(shell)),
            interactive: opts.interactive,
            workspace,
            interrupts: Arc::new(Interrupts::default()),
            recent: Vec::new(),
            path_cache: None,
            path_scan: PathScan::default(),
        };
        me.scrub_internal_env();
        if opts.load_rc {
            let profile = if opts.login {
                brush_core::ProfileLoadBehavior::LoadDefault
            } else {
                brush_core::ProfileLoadBehavior::Skip
            };
            let rc = match opts.rc_file {
                Some(p) => brush_core::RcLoadBehavior::LoadCustom(p),
                None => brush_core::RcLoadBehavior::LoadDefault,
            };
            let rt = me.rt.clone();
            let mut sh = me.lock();
            if let Err(e) = rt.block_on(sh.load_config(&profile, &rc)) {
                let _ = sh.display_error(&mut std::io::stderr(), &e);
            }
        }
        if opts.interactive || opts.catch_sigint {
            me.install_signal_handlers(opts.interactive);
        }
        Ok(me)
    }

    fn lock(&self) -> MutexGuard<'_, BrushShell> {
        self.shell.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Shared handle for editor helpers (completion, validation).
    pub fn shared(&self) -> (Arc<tokio::runtime::Runtime>, Arc<Mutex<BrushShell>>) {
        (self.rt.clone(), self.shell.clone())
    }

    pub fn is_interactive(&self) -> bool {
        self.interactive
    }

    pub fn interrupts(&self) -> Arc<Interrupts> {
        self.interrupts.clone()
    }

    /// Session start directory; agent writes outside it are riskier.
    pub fn workspace(&self) -> &Path {
        &self.workspace
    }

    pub fn set_workspace(&mut self, p: PathBuf) {
        self.workspace = p;
    }

    pub fn recent_commands(&self) -> &[UserCommand] {
        &self.recent
    }

    /// SIGINT feeds [`Interrupts`]; interactive shells also shrug off
    /// SIGTSTP/SIGQUIT/SIGTERM like bash. Handlers (unlike SIG_IGN) are reset
    /// on exec, so child processes keep default dispositions.
    fn install_signal_handlers(&self, interactive: bool) {
        use tokio::signal::unix::{SignalKind, signal};
        let ints = self.interrupts.clone();
        let _guard = self.rt.enter();
        if let Ok(mut s) = signal(SignalKind::interrupt()) {
            self.rt.spawn(async move {
                while s.recv().await.is_some() {
                    ints.fire();
                }
            });
        }
        if interactive {
            for kind in [
                SignalKind::quit(),
                SignalKind::terminate(),
                SignalKind::from_raw(libc::SIGTSTP),
            ] {
                if let Ok(mut s) = signal(kind) {
                    self.rt
                        .spawn(async move { while s.recv().await.is_some() {} });
                }
            }
        }
    }

    pub fn parse(
        &self,
        line: &str,
    ) -> Result<brush_parser::ast::Program, brush_parser::ParseError> {
        self.lock().parse_string(line.to_owned())
    }

    pub fn set_last_exit_status(&mut self, code: i32) {
        self.lock().set_last_exit_status(code as u8);
    }

    /// Variables nosh set for its own inference threads must not leak into children.
    fn scrub_internal_env(&mut self) {
        let Some(originals) = NOSH_ORIGINAL.get() else {
            return;
        };
        let mut sh = self.lock();
        for (name, original) in originals {
            match original {
                Some(v) => {
                    let mut var = ShellVariable::new(v.as_str());
                    var.export();
                    let _ = sh.env_mut().set_global(name.as_str(), var);
                }
                None => {
                    let _ = sh.env_mut().unset(name);
                }
            }
        }
    }

    /// `nosh -c`: runs the command string with bash semantics and returns `$?`.
    pub fn run_dash_c(&mut self, command: &str) -> i32 {
        let rt = self.rt.clone();
        let mut sh = self.lock();
        match rt.block_on(sh.run_dash_c_command(command.to_string())) {
            Ok(_) => i32::from(sh.last_exit_status()),
            Err(e) => {
                let _ = sh.display_error(&mut std::io::stderr(), &e);
                1
            }
        }
    }

    /// `nosh script.sh args…`.
    pub fn run_script(&mut self, path: &Path, args: &[String]) -> i32 {
        if !path.exists() {
            eprintln!("nosh: {}: No such file or directory", path.display());
            return 127;
        }
        let rt = self.rt.clone();
        let mut sh = self.lock();
        match rt.block_on(sh.run_script(path, args.iter())) {
            Ok(_) => i32::from(sh.last_exit_status()),
            Err(e) => {
                let _ = sh.display_error(&mut std::io::stderr(), &e);
                1
            }
        }
    }

    pub fn start_interactive(&mut self) {
        let _ = self.lock().start_interactive_session();
    }

    /// Non-interactive commands from stdin (`cmd | nosh`). Reads one byte at a
    /// time so commands that read stdin themselves see the rest, like bash.
    pub fn run_stdin(&mut self) -> i32 {
        use std::os::fd::{AsFd, FromRawFd};
        // SAFETY: fd 0 stays open for the life of the process; ManuallyDrop
        // keeps it from being closed here.
        let mut stdin = std::mem::ManuallyDrop::new(unsafe { std::fs::File::from_raw_fd(0) });
        let mut buf: Vec<u8> = Vec::new();
        let mut byte = [0u8; 1];
        loop {
            let eof = !matches!(stdin.read(&mut byte), Ok(1));
            if !eof {
                buf.push(byte[0]);
                if byte[0] != b'\n' {
                    continue;
                }
            }
            let text = String::from_utf8_lossy(&buf).into_owned();
            if !eof && matches!(self.parse(&text), Err(ref e) if crate::trigger::is_incomplete(e)) {
                continue;
            }
            buf.clear();
            if !text.trim().is_empty() {
                // An unbuffered handle, so `read` and child processes consume
                // exactly what they read and the rest stays for us.
                let fd = std::io::stdin()
                    .as_fd()
                    .try_clone_to_owned()
                    .ok()
                    .map(|fd| OpenFile::File(std::fs::File::from(fd)));
                let run = self.run_line(&text, fd);
                if run.exit_shell {
                    return run.exit_code;
                }
            }
            if eof {
                return self.last_exit_status();
            }
        }
    }

    pub fn end_interactive(&mut self) {
        let rt = self.rt.clone();
        let mut sh = self.lock();
        let _ = sh.end_interactive_session();
        let _ = sh.save_history();
        let _ = rt.block_on(sh.on_exit());
    }

    pub fn add_history(&mut self, line: &str) {
        let _ = self.lock().add_to_history(line);
    }

    /// Runs a user command in the foreground (terminal handed to it).
    pub fn run_user_line(&mut self, line: &str) -> UserRun {
        self.run_line(line, None)
    }

    fn run_line(&mut self, line: &str, stdin: Option<OpenFile>) -> UserRun {
        let start = Instant::now();
        let rt = self.rt.clone();
        let ints = self.interrupts.clone();
        let interactive = self.interactive;
        let (code, exit_shell) = {
            let mut sh = self.lock();
            let _ = sh.check_for_completed_jobs();
            let mut params = sh.default_exec_params();
            if let Some(f) = stdin {
                params.set_fd(OpenFiles::STDIN_FD, f);
            }
            let source = SourceInfo::from("main");
            let res = if interactive {
                // SIGINT only reaches nosh while it runs builtins itself (a
                // foreground child gets its own); give up on the line then,
                // like bash does on Ctrl-C.
                let scopes = scope_depth(sh.env());
                let ints0 = ints.count();
                let r = rt.block_on(async {
                    let fut = sh.run_string(line.to_string(), &source, &params);
                    tokio::pin!(fut);
                    let mut tick = tokio::time::interval(Duration::from_millis(50));
                    loop {
                        tokio::select! {
                            r = &mut fut => break Some(r),
                            _ = tick.tick() => if ints.count() > ints0 {
                                break None;
                            },
                        }
                    }
                });
                if r.is_none() {
                    truncate_scopes(&mut sh, scopes);
                }
                r
            } else {
                Some(rt.block_on(sh.run_string(line.to_string(), &source, &params)))
            };
            drop(params);
            sh.increment_interactive_line_offset(line.lines().count().max(1));
            match res {
                Some(Ok(r)) => (
                    i32::from(u8::from(r.exit_code)),
                    matches!(r.next_control_flow, ExecutionControlFlow::ExitShell),
                ),
                Some(Err(e)) => {
                    let _ = sh.display_error(&mut std::io::stderr(), &e);
                    (1, false)
                }
                None => {
                    eprintln!();
                    sh.set_last_exit_status(130);
                    (130, false)
                }
            }
        };
        self.recent.push(UserCommand {
            line: line.trim().to_string(),
            exit: code,
            duration: start.elapsed(),
        });
        if self.recent.len() > 5 {
            self.recent.remove(0);
        }
        UserRun {
            exit_code: code,
            exit_shell,
        }
    }

    pub fn last_exit_status(&self) -> i32 {
        i32::from(self.lock().last_exit_status())
    }

    pub fn cwd(&self) -> PathBuf {
        self.lock().working_dir().to_path_buf()
    }

    pub fn home(&self) -> Option<PathBuf> {
        self.lock()
            .env_str("HOME")
            .map(|h| PathBuf::from(h.into_owned()))
    }

    pub fn var(&self, name: &str) -> Option<String> {
        self.lock().env_str(name).map(|v| v.into_owned())
    }

    pub fn aliases(&self) -> HashMap<String, String> {
        self.lock().aliases().clone()
    }

    /// Function name → body text.
    pub fn functions(&self) -> HashMap<String, String> {
        self.lock()
            .funcs()
            .iter()
            .map(|(k, v)| (k.clone(), v.definition().body.to_string()))
            .collect()
    }

    pub fn snapshot(&self) -> SessionState {
        let sh = self.lock();
        let mut vars = BTreeMap::new();
        let mut exported = BTreeSet::new();
        for (name, var) in sh.env().iter() {
            if VOLATILE.contains(&name.as_str()) {
                continue;
            }
            let v = var.value();
            let s = if v.is_array() {
                v.element_values(&sh).join(" ")
            } else {
                v.to_cow_str(&sh).into_owned()
            };
            if var.is_exported() {
                exported.insert(name.clone());
            }
            vars.insert(name.clone(), s);
        }
        SessionState {
            cwd: sh.working_dir().to_path_buf(),
            vars,
            exported,
            functions: sh.funcs().iter().map(|(k, _)| k.clone()).collect(),
            aliases: sh
                .aliases()
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            last_exit: i32::from(sh.last_exit_status()),
        }
    }

    pub fn is_keyword(&self, name: &str) -> bool {
        self.lock().is_keyword(name)
    }

    pub fn resolve(&self, name: &str) -> Resolution {
        let mut sh = self.lock();
        if let Some(a) = sh.aliases().get(name) {
            return Resolution::Alias(a.clone());
        }
        if sh.funcs().get(name).is_some() {
            return Resolution::Function;
        }
        if sh.is_keyword(name) {
            return Resolution::Keyword;
        }
        if sh.builtin_mut(name).is_some_and(|b| !b.disabled) {
            return Resolution::Builtin;
        }
        if name.contains('/') {
            let p = sh.absolute_path(Path::new(name));
            return if is_executable(&p) {
                Resolution::File(p)
            } else {
                Resolution::NotFound
            };
        }
        match sh.find_first_executable_in_path(name) {
            Some(p) => Resolution::File(p),
            None => Resolution::NotFound,
        }
    }

    /// Candidate command names for spelling correction (PATH, aliases,
    /// functions, builtins). PATH directories are listed without stat-ing each
    /// entry (slow on network/WSL mounts); callers verify a match with
    /// [`Self::resolve`].
    pub fn command_names(&mut self) -> Arc<Vec<String>> {
        let path = self.var("PATH").unwrap_or_default();
        let (aliases, funcs): (Vec<String>, Vec<String>) = {
            let sh = self.lock();
            (
                sh.aliases().keys().cloned().collect(),
                sh.funcs().iter().map(|(k, _)| k.clone()).collect(),
            )
        };
        let key = format!("{path}\0{}\0{}", aliases.join(" "), funcs.join(" "));
        if let Some((k, names)) = &self.path_cache
            && *k == key
        {
            return names.clone();
        }
        let scanned = path_names(&self.path_scan, &path);
        let mut set: BTreeSet<String> = scanned.iter().cloned().collect();
        set.extend(aliases);
        set.extend(funcs);
        set.extend(BUILTIN_NAMES.iter().map(|s| s.to_string()));
        let names = Arc::new(set.into_iter().collect::<Vec<_>>());
        self.path_cache = Some((key, names.clone()));
        names
    }

    /// Lists PATH in the background so the first correction is instant.
    pub fn warm_command_names(&self) {
        let path = self.var("PATH").unwrap_or_default();
        let cache = self.path_scan.clone();
        std::thread::spawn(move || {
            path_names(&cache, &path);
        });
    }

    /// The prompt: the user's `PS1` if their rc set one, else `~/dir ❯ `.
    pub fn prompt(&self) -> (String, bool) {
        let rt = self.rt.clone();
        let mut sh = self.lock();
        // brush's own default, set when nothing else did.
        let custom = sh.env_str("PS1").is_some_and(|p| p != r"\s-\v\$ ");
        if custom && let Ok(p) = rt.block_on(sh.compose_prompt()) {
            return (p, true);
        }
        let cwd = sh.working_dir().to_string_lossy().into_owned();
        (sh.tilde_shorten(cwd), false)
    }

    pub fn continuation_prompt(&self) -> String {
        let rt = self.rt.clone();
        let mut sh = self.lock();
        rt.block_on(sh.compose_continuation_prompt())
            .unwrap_or_else(|_| "> ".to_string())
    }

    /// Runs `PROMPT_COMMAND` and reaps finished jobs before a prompt.
    pub fn pre_prompt(&mut self) {
        let rt = self.rt.clone();
        let mut sh = self.lock();
        let _ = sh.check_for_completed_jobs();
        if let Some(cmd) = sh.env_str("PROMPT_COMMAND").map(|c| c.into_owned())
            && !cmd.trim().is_empty()
        {
            let prev = sh.last_exit_status();
            let params = sh.default_exec_params();
            let _ = rt.block_on(sh.run_string(cmd, &SourceInfo::from("PROMPT_COMMAND"), &params));
            sh.set_last_exit_status(prev);
        }
    }

    /// Runs an agent command in the shared session: stdin is `/dev/null`,
    /// output is captured through pipes (and streamed to `out`), it runs in its
    /// own background process group, anti-hang variables apply only to it,
    /// and it is interrupted on timeout or Ctrl-C.
    pub fn run_agent_command(
        &mut self,
        cmd: &str,
        opts: &AgentExecOpts,
        out: &mut dyn OutputSink,
    ) -> Result<CommandResult, ShellError> {
        let before = self.snapshot();
        let before_children = procs::child_pids();
        let rt = self.rt.clone();
        let ints = self.interrupts.clone();
        let start = Instant::now();

        let (out_r, out_w) = std::io::pipe()?;
        let (err_r, err_w) = std::io::pipe()?;
        // Bounded: a fast writer is held back by the pipe instead of growing
        // memory, and the loop below keeps getting to check the clock.
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, Vec<u8>)>(16);
        spawn_reader(out_r, false, tx.clone());
        spawn_reader(err_r, true, tx);
        // The command is in the background, so Ctrl-Z would stop nosh itself
        // (and brush would take the command for stopped).
        let _no_suspend = NoSuspend::new(self.interactive);

        let mut cap = Capture::new(opts.capture_limit);
        let mut result = CommandResult::default();
        let exit_code = {
            let mut sh = self.lock();
            let job_ids_before: HashSet<usize> = sh.jobs().jobs.iter().map(|j| j.id).collect();
            let scopes = scope_depth(sh.env());
            let saved = apply_anti_hang_env(&mut sh);
            let mut params = sh.default_exec_params();
            params.process_group_policy = brush_core::ProcessGroupPolicy::NewProcessGroup;
            params.set_fd(OpenFiles::STDIN_FD, openfiles::null()?);
            params.set_fd(OpenFiles::STDOUT_FD, OpenFile::PipeWriter(out_w));
            params.set_fd(OpenFiles::STDERR_FD, OpenFile::PipeWriter(err_w));
            let ints_start = ints.count();
            let deadline = start + opts.timeout;
            let source = SourceInfo::from("agent");
            let mut stop_signal = None;
            let res = rt.block_on(async {
                let fut = sh.run_string(cmd.to_string(), &source, &params);
                tokio::pin!(fut);
                let mut tick = tokio::time::interval(Duration::from_millis(40));
                loop {
                    tokio::select! {
                        r = &mut fut => break Some(r),
                        Some((is_err, bytes)) = rx.recv() => cap.push(is_err, &bytes, out),
                        _ = tick.tick() => {
                            if ints.count() > ints_start {
                                result.interrupted = true;
                                stop_signal = Some(libc::SIGINT);
                                break None;
                            }
                            if Instant::now() >= deadline {
                                result.timed_out = true;
                                stop_signal = Some(libc::SIGTERM);
                                break None;
                            }
                        }
                    }
                }
            });
            drop(params);
            if res.is_none() {
                // The whole command line is abandoned (like bash on Ctrl-C):
                // drop what brush left on its scope stack, stop everything the
                // command started, and escalate to SIGKILL.
                truncate_scopes(&mut sh, scopes);
                let targets = procs::new_targets(&before_children);
                if let Some(sig) = stop_signal {
                    procs::signal(&targets, sig);
                }
                if !targets.is_empty() {
                    std::thread::spawn(move || {
                        std::thread::sleep(Duration::from_secs(2));
                        procs::signal(&targets, libc::SIGKILL);
                    });
                }
            }
            restore_env(&mut sh, saved);
            // Stopped jobs (e.g. SIGTTIN from reading the terminal) are killed.
            let mut stopped = Vec::new();
            sh.jobs_mut().jobs.retain(|j| {
                let new = !job_ids_before.contains(&j.id);
                if new && matches!(j.state, brush_core::jobs::JobState::Stopped) {
                    if let Some(pg) = j.process_group_id() {
                        stopped.push(pg);
                    }
                    false
                } else {
                    true
                }
            });
            if !stopped.is_empty() {
                result.needed_terminal = true;
                procs::signal_groups(&stopped, libc::SIGKILL);
                procs::signal_groups(&stopped, libc::SIGCONT);
            }
            match res {
                Some(Ok(r)) => {
                    let code = i32::from(u8::from(r.exit_code));
                    if code == 148 {
                        result.needed_terminal = true;
                    }
                    code
                }
                Some(Err(e)) => {
                    cap.push(true, format!("{e}\n").as_bytes(), out);
                    1
                }
                None if result.timed_out => 124,
                None => 130,
            }
        };
        // Drain what is left; background children may keep the pipes open.
        rt.block_on(async {
            let until = tokio::time::Instant::now() + Duration::from_millis(300);
            while let Ok(Some((is_err, bytes))) = tokio::time::timeout_at(until, rx.recv()).await {
                cap.push(is_err, &bytes, out);
            }
        });
        drop(rx);
        cap.flush(out);
        result.exit_code = exit_code;
        result.duration = start.elapsed();
        result.truncated = cap.truncated;
        result.stdout = cap.out_text();
        result.stderr = cap.err_text();
        let after = self.snapshot();
        result.diff = StateDiff::between(&before, &after);
        Ok(result)
    }
}

/// Reads one output pipe of an agent command into the channel. Once nobody
/// listens (the command returned but a background job it started still
/// writes), it keeps reading and discarding so the job does not get SIGPIPE.
fn spawn_reader(
    mut r: std::io::PipeReader,
    is_err: bool,
    tx: tokio::sync::mpsc::Sender<(bool, Vec<u8>)>,
) {
    let _ = std::thread::Builder::new()
        .name("nosh-agent-io".into())
        .spawn(move || {
            let mut buf = vec![0u8; 16 * 1024];
            let mut tx = Some(tx);
            loop {
                match r.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Some(t) = &tx
                            && t.blocking_send((is_err, buf[..n].to_vec())).is_err()
                        {
                            tx = None;
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
        });
}

/// Disables the terminal's suspend character (Ctrl-Z) while alive.
struct NoSuspend(Option<libc::termios>);

impl NoSuspend {
    fn new(active: bool) -> Self {
        if !active {
            return Self(None);
        }
        // SAFETY: termios calls on fd 0 with a struct they fill in.
        unsafe {
            if libc::isatty(0) != 1 {
                return Self(None);
            }
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(0, &mut t) != 0 {
                return Self(None);
            }
            let saved = t;
            t.c_cc[libc::VSUSP] = 0; // _POSIX_VDISABLE
            if libc::tcsetattr(0, libc::TCSANOW, &t) != 0 {
                return Self(None);
            }
            Self(Some(saved))
        }
    }
}

impl Drop for NoSuspend {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            // SAFETY: restores the attributes read in `new`.
            unsafe {
                libc::tcsetattr(0, libc::TCSANOW, t);
            }
        }
    }
}

/// Number of variable scopes (global, function locals, per-command overrides).
fn scope_depth(env: &brush_core::env::ShellEnvironment) -> usize {
    use brush_core::env::EnvironmentScope;
    let mut e = env.clone();
    let mut n = 0;
    // `pop_scope` pops whatever is on top; it only reports a type mismatch.
    while !matches!(
        e.pop_scope(EnvironmentScope::Global),
        Err(ref err) if matches!(err.kind(), brush_core::ErrorKind::MissingScope)
    ) {
        n += 1;
    }
    n
}

/// Pops scopes an abandoned command left behind (a function's locals and
/// `VAR=x cmd` overrides would otherwise leak into the session).
fn truncate_scopes(sh: &mut BrushShell, depth: usize) {
    for _ in depth..scope_depth(sh.env()) {
        let _ = sh
            .env_mut()
            .pop_scope(brush_core::env::EnvironmentScope::Local);
    }
}

/// Shell builtins available for completion/spelling even without a PATH hit.
const BUILTIN_NAMES: &[&str] = &[
    "alias", "bg", "bind", "break", "builtin", "cd", "command", "compgen", "complete", "continue",
    "declare", "dirs", "disown", "echo", "enable", "eval", "exec", "exit", "export", "false", "fc",
    "fg", "getopts", "hash", "help", "history", "jobs", "kill", "let", "local", "logout", "popd",
    "printf", "pushd", "pwd", "read", "readonly", "return", "set", "shift", "shopt", "source",
    "test", "times", "trap", "true", "type", "typeset", "ulimit", "umask", "unalias", "unset",
    "wait",
];

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

type PathScan = Arc<Mutex<Option<(String, Arc<Vec<String>>)>>>;

/// Entry names of every PATH directory, cached per PATH value.
fn path_names(cache: &PathScan, path: &str) -> Arc<Vec<String>> {
    if let Some((p, names)) = &*cache.lock().unwrap_or_else(|e| e.into_inner())
        && p == path
    {
        return names.clone();
    }
    let mut set = BTreeSet::new();
    for dir in path.split(':').filter(|d| !d.is_empty()) {
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                if let Some(n) = e.file_name().to_str()
                    && !n.starts_with('.')
                {
                    set.insert(n.to_string());
                }
            }
        }
    }
    let names = Arc::new(set.into_iter().collect::<Vec<_>>());
    *cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((path.to_string(), names.clone()));
    names
}

static NOSH_ORIGINAL: std::sync::OnceLock<HashMap<String, Option<String>>> =
    std::sync::OnceLock::new();

/// Records process variables nosh overrode for its own inference threads
/// (`name`, value before the override); shells created afterwards restore the
/// original values so child processes are unaffected.
pub fn register_internal_env(entries: &[(&str, Option<String>)]) {
    let original: HashMap<String, Option<String>> = entries
        .iter()
        .map(|(n, o)| (n.to_string(), o.clone()))
        .collect();
    let _ = NOSH_ORIGINAL.set(original);
}

type SavedEnv = Vec<(String, Option<ShellVariable>, String)>;

fn apply_anti_hang_env(sh: &mut BrushShell) -> SavedEnv {
    let mut saved = Vec::new();
    for (k, v) in ANTI_HANG_ENV {
        let prev = sh.env_var(k).cloned();
        let mut var = ShellVariable::new(*v);
        var.export();
        if sh.env_mut().set_global(*k, var).is_ok() {
            saved.push((k.to_string(), prev, v.to_string()));
        }
    }
    saved
}

fn restore_env(sh: &mut BrushShell, saved: SavedEnv) {
    for (k, prev, injected) in saved {
        let current = sh.env_str(&k).map(|c| c.into_owned());
        if current.as_deref() != Some(injected.as_str()) {
            continue; // the command changed it on purpose
        }
        match prev {
            Some(var) => {
                let _ = sh.env_mut().set_global(k, var);
            }
            None => {
                let _ = sh.env_mut().unset(&k);
            }
        }
    }
}

/// Bounded capture of stdout/stderr with UTF-8 aware streaming to a sink.
struct Capture {
    limit: usize,
    out: Vec<u8>,
    err: Vec<u8>,
    pend_out: Vec<u8>,
    pend_err: Vec<u8>,
    truncated: bool,
}

impl Capture {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            out: Vec::new(),
            err: Vec::new(),
            pend_out: Vec::new(),
            pend_err: Vec::new(),
            truncated: false,
        }
    }

    fn push(&mut self, is_err: bool, bytes: &[u8], sink: &mut dyn OutputSink) {
        let (store, pend) = if is_err {
            (&mut self.err, &mut self.pend_err)
        } else {
            (&mut self.out, &mut self.pend_out)
        };
        // Past the limit nothing is kept or shown (the display is slower
        // than a pipe and would otherwise fall behind without bound).
        let room = self.limit.saturating_sub(store.len());
        if room < bytes.len() {
            self.truncated = true;
        }
        let bytes = &bytes[..room.min(bytes.len())];
        if bytes.is_empty() {
            return;
        }
        store.extend_from_slice(bytes);
        pend.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(pend) {
            Ok(_) => pend.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => pend.len(),
        };
        if valid > 0 {
            let chunk = String::from_utf8_lossy(&pend[..valid]).into_owned();
            pend.drain(..valid);
            if is_err {
                sink.stderr(&chunk);
            } else {
                sink.stdout(&chunk);
            }
        }
    }

    fn flush(&mut self, sink: &mut dyn OutputSink) {
        if !self.pend_out.is_empty() {
            sink.stdout(&String::from_utf8_lossy(&self.pend_out));
            self.pend_out.clear();
        }
        if !self.pend_err.is_empty() {
            sink.stderr(&String::from_utf8_lossy(&self.pend_err));
            self.pend_err.clear();
        }
    }

    fn out_text(&self) -> String {
        String::from_utf8_lossy(&self.out).into_owned()
    }

    fn err_text(&self) -> String {
        String::from_utf8_lossy(&self.err).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diff_describes_changes() {
        let mut a = SessionState {
            cwd: "/a".into(),
            ..SessionState::default()
        };
        a.vars.insert("PATH".into(), "/bin".into());
        a.vars.insert("OLD".into(), "1".into());
        let mut b = a.clone();
        b.cwd = "/b".into();
        b.vars.insert("PATH".into(), "/bin:/x".into());
        b.vars.remove("OLD");
        b.vars.insert("NEW".into(), "2".into());
        b.vars.insert("RANDOM".into(), "3".into());
        b.functions.insert("f".into());
        let d = StateDiff::between(&a, &b);
        assert_eq!(
            d.describe(),
            "cwd: /a → /b; PATH modified; vars: +NEW -OLD; functions: +f"
        );
        assert!(StateDiff::between(&a, &a).is_empty());
    }
}
