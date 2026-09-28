use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::{AccessKind, Context, Operation, PathClass, Risk, RiskReport, classify_path};

const MAX_TARGETS: usize = 32;
const SMALL_BYTES: u64 = 1024 * 1024;
const PROBE_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoAdmission {
    pub reasons: Vec<String>,
    pub development: bool,
}

pub(crate) struct AutoRejection {
    pub reason: String,
    pub risk: Risk,
}

impl From<String> for AutoRejection {
    fn from(reason: String) -> Self {
        Self {
            reason,
            risk: Risk::Mutating,
        }
    }
}

impl From<&str> for AutoRejection {
    fn from(reason: &str) -> Self {
        reason.to_string().into()
    }
}

fn base(op: &Operation) -> &str {
    op.argv
        .first()
        .map(|s| s.rsplit('/').next().unwrap_or(s))
        .unwrap_or("")
}

fn recognized_program(op: &Operation) -> bool {
    !op.local_program
        && !op.transparent
        && op.argv.first().is_some_and(|name| {
            !name.contains('/')
                || crate::analyze::in_system_bin_dir(name)
                || name == "./gradlew" && op.payload
        })
}

fn ordinary_move_or_copy(op: &Operation) -> bool {
    recognized_program(op)
        && matches!(base(op), "mv" | "cp")
        && op.risk < Risk::Dangerous
        && !op.opaque
        && !op.payload
        && op.known.iter().all(|known| *known)
        && op
            .paths
            .iter()
            .all(|path| !path.extra && path.resolved.is_some())
}

fn build_goal(goal: &str) -> bool {
    matches!(
        goal,
        "all" | "build" | "test" | "check" | "compile" | "package" | "verify"
    )
}

fn build_goals(args: &[String], valued_options: &[&str]) -> bool {
    let mut args = args.iter();
    let mut has_goal = false;
    while let Some(arg) = args.next() {
        if valued_options.contains(&arg.as_str()) {
            if args.next().is_none() {
                return false;
            }
        } else if !arg.starts_with('-') {
            if !build_goal(arg) {
                return false;
            }
            has_goal = true;
        }
    }
    has_goal
}

/// Deliberately accepted project-code risk, separate from recovery evidence.
fn development(op: &Operation) -> bool {
    let args = &op.argv;
    let known = |i: usize| op.known.get(i) == Some(&true);
    if !known(0) || !recognized_program(op) || op.known.iter().any(|known| !known) {
        return false;
    }
    let sub = args.get(1).map(String::as_str);
    match base(op) {
        "cargo" => {
            known(1)
                && !args.iter().any(|arg| arg == "--fix")
                && match sub {
                    Some("fmt") => {
                        op.known.iter().all(|known| *known)
                            && args.iter().any(|arg| arg == "--check")
                    }
                    Some("build" | "check" | "test" | "bench" | "clippy") => true,
                    _ => false,
                }
        }
        "go" => known(1) && matches!(sub, Some("build" | "test" | "vet")),
        "npm" | "pnpm" | "yarn" => {
            known(1)
                && match sub {
                    Some("test" | "build" | "check" | "lint" | "typecheck") => true,
                    Some("run") => {
                        known(2)
                            && args.get(2).is_some_and(|s| {
                                matches!(
                                    s.as_str(),
                                    "build" | "test" | "check" | "lint" | "typecheck"
                                )
                            })
                    }
                    _ => false,
                }
        }
        "pytest" | "pytest-3" | "ctest" => true,
        "tsc" => !args.iter().any(|arg| arg.eq_ignore_ascii_case("--clean")),
        "eslint" => !args
            .iter()
            .any(|arg| matches!(arg.as_str(), "--fix" | "--init")),
        "python" | "python3" => {
            known(1) && known(2) && sub == Some("-m") && args.get(2).is_some_and(|s| s == "pytest")
        }
        "cmake" => {
            if sub != Some("--build") || args.len() < 3 || op.known.iter().any(|known| !known) {
                return false;
            }
            let mut rest = args[3..].iter().peekable();
            while let Some(arg) = rest.next() {
                match arg.as_str() {
                    "--target" | "-t" => {
                        let mut targets = 0;
                        while rest.peek().is_some_and(|a| !a.starts_with('-')) {
                            if !build_goal(rest.next().unwrap()) {
                                return false;
                            }
                            targets += 1;
                        }
                        if targets == 0 {
                            return false;
                        }
                    }
                    "--config" => {
                        if rest.next().is_none() {
                            return false;
                        }
                    }
                    "--parallel" | "-j" => {
                        if rest.peek().is_some_and(|a| a.parse::<u32>().is_ok()) {
                            rest.next();
                        }
                    }
                    "--verbose" | "-v" => {}
                    _ => return false,
                }
            }
            true
        }
        "dotnet" => known(1) && matches!(sub, Some("build" | "test")),
        "make" | "gmake" | "ninja" => {
            if op.known.iter().any(|known| !known) {
                return false;
            }
            let mut i = 1;
            while i < args.len() {
                match args[i].as_str() {
                    "-j" | "--jobs" => {
                        if args.get(i + 1).is_some_and(|n| n.parse::<u32>().is_ok()) {
                            i += 1;
                        }
                    }
                    "-C" | "--directory" | "-f" | "--file" => {
                        i += 1;
                        if i >= args.len() {
                            return false;
                        }
                    }
                    "all" | "build" | "test" | "check" => {}
                    arg if arg
                        .strip_prefix("-j")
                        .is_some_and(|n| n.parse::<u32>().is_ok()) => {}
                    _ => return false,
                }
                i += 1;
            }
            true
        }
        "meson" => {
            known(1) && sub.is_some_and(build_goal) && !args.iter().any(|arg| arg == "--clean")
        }
        "bazel" => known(1) && sub.is_some_and(build_goal),
        "gradle" | "gradlew" => {
            op.known.iter().all(|known| *known)
                && build_goals(&args[1..], &["-p", "--project-dir", "-f", "--file"])
        }
        "mvn" => {
            op.known.iter().all(|known| *known)
                && build_goals(
                    &args[1..],
                    &["-f", "--file", "-pl", "--projects", "-s", "--settings"],
                )
        }
        "sbt" => op.known.iter().all(|known| *known) && build_goals(&args[1..], &[]),
        _ => false,
    }
}

fn path_ok(path: &Path, ctx: &Context) -> bool {
    matches!(
        classify_path(path, ctx),
        PathClass::Workspace | PathClass::Temp | PathClass::Null
    )
}

fn metadata(path: &Path) -> Result<Option<fs::Metadata>, String> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("cannot inspect {}: {e}", path.display())),
    }
}

fn concrete_paths(op: &Operation, ctx: &Context) -> Result<Vec<PathBuf>, String> {
    if op.paths.len() > MAX_TARGETS {
        return Err(format!("more than {MAX_TARGETS} file targets"));
    }
    op.paths
        .iter()
        .map(|access| {
            let path = access
                .resolved
                .as_ref()
                .ok_or("file target is determined at runtime")?;
            if !path_ok(path, ctx) {
                return Err(format!(
                    "target is outside the low-impact scope: {}",
                    path.display()
                ));
            }
            if !access
                .lexical
                .as_deref()
                .is_some_and(|lexical| crate::paths::same_workspace_target(lexical, path, ctx))
            {
                return Err(format!("target goes through a symlink: {}", path.display()));
            }
            Ok(path.clone())
        })
        .collect()
}

fn ordinary_variables(op: &Operation, ctx: &Context) -> bool {
    op.variables.iter().all(|(name, value)| {
        if matches!(name.as_str(), "PWD" | "OLDPWD") {
            return matches!(base(op), "cd" | "pushd")
                && value.as_deref().is_some_and(|p| Path::new(p).is_dir())
                && ctx.variables.get("CDPATH").is_none_or(String::is_empty);
        }
        crate::rules::var_assignment_risk(name).is_none()
            && !ctx.unknown_variables.contains(name)
            && value.as_ref().is_none_or(|v| v.len() as u64 <= SMALL_BYTES)
            && ctx
                .variables
                .get(name)
                .is_none_or(|v| v.len() as u64 <= SMALL_BYTES)
            && (value.is_some() || base(op) == "unset" || ctx.variables.contains_key(name))
            && !name.is_empty()
    })
}

fn diagnostic(op: &Operation, ctx: &Context) -> bool {
    if !recognized_program(op)
        || op.known.iter().any(|known| !known)
        || ctx.timeout > Duration::from_secs(60)
    {
        return false;
    }
    match base(op) {
        "ping" | "ping6" => {
            let mut targets = 0;
            let mut i = 1;
            while i < op.argv.len() {
                let arg = op.argv[i].as_str();
                match arg {
                    "-n" | "-q" | "-4" | "-6" => {}
                    "-c" | "-W" | "-w" | "-t" => {
                        i += 1;
                        if !op
                            .argv
                            .get(i)
                            .and_then(|s| s.parse::<u32>().ok())
                            .is_some_and(|n| (1..=60).contains(&n))
                        {
                            return false;
                        }
                    }
                    s if s.starts_with('-') => return false,
                    _ => targets += 1,
                }
                i += 1;
            }
            targets == 1
        }
        "dig" | "host" | "nslookup" => {
            op.argv.len() <= 8
                && !op.argv.iter().any(|a| {
                    a.eq_ignore_ascii_case("AXFR")
                        || a.eq_ignore_ascii_case("IXFR")
                        || a.starts_with("-f")
                        || a == "-l"
                })
        }
        _ => false,
    }
}

struct GitProbe<'a> {
    cwd: &'a Path,
    ctx: &'a Context,
    program: &'a str,
}

impl GitProbe<'_> {
    fn run(&self, args: &[&str]) -> Result<Option<String>, String> {
        // Use the resolved Git for evidence, never a project-local lookalike.
        let git = if self.program.contains('/') {
            PathBuf::from(self.program)
        } else {
            self.ctx
                .variables
                .get("PATH")
                .and_then(|path| {
                    std::env::split_paths(path)
                        .map(|dir| dir.join("git"))
                        .find(|path| path.is_file())
                })
                .unwrap_or_else(|| PathBuf::from("/usr/bin/git"))
        };
        if !git.is_file() {
            return Err("resolved Git is unavailable for recovery evidence".into());
        }
        if fs::canonicalize(&git)
            .map_err(|e| format!("cannot resolve Git: {e}"))?
            .starts_with(&self.ctx.workspace)
        {
            return Err("the session resolves Git to a workspace-local program".into());
        }
        if self.ctx.variables.keys().any(|k| k.starts_with("GIT_")) {
            return Err("custom GIT_* environment needs explicit authorization".into());
        }
        let mut cmd = Command::new(&git);
        cmd.arg("--no-optional-locks")
            .current_dir(self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .env_clear()
            .envs(
                self.ctx
                    .exported
                    .iter()
                    .filter_map(|name| self.ctx.variables.get(name).map(|value| (name, value))),
            )
            .envs(
                self.ctx
                    .execution_variables
                    .iter()
                    .filter_map(|(name, value)| value.as_ref().map(|value| (name, value))),
            )
            .env("LC_ALL", "C");
        if let Some(home) = &self.ctx.home {
            cmd.env("HOME", home);
        }
        if let Some(xdg) = self.ctx.variables.get("XDG_CONFIG_HOME") {
            cmd.env("XDG_CONFIG_HOME", xdg);
        }
        if args.first() != Some(&"config") {
            cmd.args(["-c", "core.fsmonitor=false"]);
        }
        let mut child = cmd
            .args(args)
            .spawn()
            .map_err(|e| format!("cannot inspect Git state: {e}"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or("Git evidence has no output pipe")?;
        let reader = std::thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout
                .take(PROBE_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes)
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10))
                }
                other => {
                    let _ = child.kill();
                    let _ = child.wait();
                    let _ = reader.join();
                    return Err(match other {
                        Err(e) => format!("cannot wait for Git evidence: {e}"),
                        _ => format!(
                            "Git {} evidence exceeded its time budget ({})",
                            args.first().copied().unwrap_or("state"),
                            git.display()
                        ),
                    });
                }
            }
        };
        let bytes = reader
            .join()
            .map_err(|_| "Git evidence reader failed")?
            .map_err(|e| format!("cannot read Git evidence: {e}"))?;
        if bytes.len() as u64 > PROBE_BYTES {
            return Err("Git evidence exceeded its size budget".into());
        }
        if !status.success() {
            if status.code() == Some(1)
                && matches!(args.first(), Some(&"config" | &"diff" | &"symbolic-ref"))
            {
                return Ok(None);
            }
            return Err(format!(
                "Git {} evidence failed ({status})",
                args.first().copied().unwrap_or("state")
            ));
        }
        String::from_utf8(bytes)
            .map(|s| Some(s.trim_end_matches('\n').to_string()))
            .map_err(|e| format!("invalid Git evidence: {e}"))
    }

    fn no_executable_config(&self) -> Result<(), String> {
        if self
            .run(&[
                "config",
                "--get-regexp",
                r"^(core\.(hookspath|fsmonitor|sshcommand|askpass)|credential(\..*)?\.helper|commit\.gpgsign|gpg\..*|filter\..*\.(clean|smudge|process))$",
            ])?
            .is_some()
        {
            return Err("Git hooks, filters or executable configuration need authorization".into());
        }
        let hooks = self
            .run(&["rev-parse", "--git-path", "hooks"])?
            .ok_or("not a Git repository")?;
        let hooks = self.cwd.join(hooks);
        match fs::read_dir(&hooks) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry.map_err(|e| format!("cannot inspect Git hooks: {e}"))?;
                    if !entry.file_name().to_string_lossy().ends_with(".sample") {
                        return Err("repository has active Git hooks".into());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("cannot inspect Git hooks: {e}")),
        }
        Ok(())
    }

    fn index_snapshot(&self, paths: &[&str]) -> Result<String, String> {
        let mut args = vec!["ls-files", "--stage", "-z", "--"];
        args.extend_from_slice(paths);
        let index = self.run(&args)?.ok_or("cannot inspect the index")?;
        let entries: Vec<_> = index
            .split('\0')
            .filter(|entry| !entry.is_empty())
            .collect();
        if entries.len() > MAX_TARGETS {
            return Err(format!("index recovery exceeds {MAX_TARGETS} entries"));
        }
        for entry in &entries {
            let (metadata, _) = entry.split_once('\t').ok_or("invalid index entry")?;
            let fields: Vec<_> = metadata.split_whitespace().collect();
            if fields.len() != 3 || fields[2] != "0" || !matches!(fields[0], "100644" | "100755") {
                return Err("index recovery requires ordinary, unconflicted files".into());
            }
            self.run(&["cat-file", "-e", fields[1]])?
                .ok_or("an original index object is missing")?;
        }
        Ok(format!(
            "original index entries: {}",
            if entries.is_empty() {
                "(none)".into()
            } else {
                entries.join("; ")
            }
        ))
    }

    fn small_switch(&self, target: &str) -> Result<(), AutoRejection> {
        let target = format!("{target}^{{commit}}");
        self.run(&["rev-parse", "--verify", &target])?
            .ok_or("unknown target commit")?;
        let changed = self
            .run(&[
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--name-only",
                "-z",
                "HEAD",
                &target,
                "--",
            ])?
            .ok_or("cannot inspect branch changes")?;
        let paths: Vec<_> = changed
            .split('\0')
            .filter(|path| !path.is_empty())
            .collect();
        if paths.len() > MAX_TARGETS {
            return Err(format!("branch switch changes more than {MAX_TARGETS} files").into());
        }
        if paths.is_empty() {
            return Ok(());
        }
        let root = self
            .run(&["rev-parse", "--show-toplevel"])?
            .ok_or("cannot locate the Git worktree")?;
        for path in &paths {
            let path = Path::new(&root).join(path);
            if !path_ok(&path, self.ctx) {
                return Err(AutoRejection {
                    reason: format!(
                        "branch switch changes a protected or external path: {}",
                        path.display()
                    ),
                    risk: Risk::Dangerous,
                });
            }
        }
        let mut bytes = 0u64;
        for tree in ["HEAD", &target] {
            let mut args = vec!["ls-tree", "-r", "-l", "-z", tree, "--"];
            args.extend_from_slice(&paths);
            let entries = self.run(&args)?.ok_or("cannot inspect tree contents")?;
            for entry in entries.split('\0').filter(|entry| !entry.is_empty()) {
                let (metadata, _) = entry.split_once('\t').ok_or("invalid tree entry")?;
                let fields: Vec<_> = metadata.split_whitespace().collect();
                if fields.len() != 4
                    || !matches!(fields[0], "100644" | "100755")
                    || fields[1] != "blob"
                {
                    return Err("branch switch includes links or submodules".into());
                }
                bytes = bytes.saturating_add(
                    fields[3]
                        .parse::<u64>()
                        .map_err(|_| "unknown tree object size")?,
                );
                if bytes > SMALL_BYTES {
                    return Err("branch switch exceeds the bounded content budget".into());
                }
            }
        }
        Ok(())
    }
}

fn git_admission(op: &Operation, ctx: &Context) -> Result<String, AutoRejection> {
    if !recognized_program(op) {
        return Err("path-qualified program is not a recognized Git executable".into());
    }
    let executable = op.executable.as_ref().map(|path| path.to_string_lossy());
    let probe = GitProbe {
        cwd: &op.cwd,
        ctx,
        program: executable.as_deref().unwrap_or(&op.argv[0]),
    };
    probe.no_executable_config()?;
    let sub = op.argv.get(1).map(String::as_str).unwrap_or("");
    if op.known.iter().any(|k| !k) {
        return Err("Git arguments are determined at runtime".into());
    }
    let head = probe.run(&["rev-parse", "--verify", "HEAD"])?;
    match sub {
        "add" => {
            let head = head.ok_or("an initial commit is needed for index recovery")?;
            if probe
                .run(&[
                    "diff",
                    "--cached",
                    "--quiet",
                    "--no-ext-diff",
                    "--no-textconv",
                ])?
                .is_none()
            {
                let mut paths = Vec::new();
                let mut options = true;
                for arg in op.argv.iter().skip(2) {
                    if options && arg == "--" {
                        options = false;
                        continue;
                    }
                    if options && matches!(arg.as_str(), "-A" | "-u" | "--all" | "--update") {
                        continue;
                    }
                    if options && arg.starts_with('-') {
                        return Err("unsupported staging options".into());
                    }
                    paths.push(arg.as_str());
                }
                return probe.index_snapshot(&paths).map_err(Into::into);
            }
            Ok(format!("the original index is recoverable from {head}"))
        }
        "restore" if op.argv.iter().any(|a| a == "--staged" || a == "-S") => {
            if op
                .argv
                .iter()
                .any(|a| matches!(a.as_str(), "--worktree" | "-W"))
            {
                return Err("also changes the working tree".into());
            }
            let mut paths = Vec::new();
            for arg in op.argv.iter().skip(2) {
                match arg.as_str() {
                    "--staged" | "-S" | "--" => {}
                    arg if arg.starts_with('-') => {
                        return Err("unsupported index-restore option".into());
                    }
                    arg => paths.push(arg),
                }
            }
            if paths.is_empty() {
                return Err("index restore needs explicit paths".into());
            }
            probe.index_snapshot(&paths).map_err(Into::into)
        }
        "branch" if op.argv.len() == 3 && !op.argv[2].starts_with('-') => {
            Ok("creates a local branch without changing files or existing refs".into())
        }
        "switch" | "checkout" => {
            let head = head.ok_or("missing original HEAD")?;
            if op.argv.len() == 4
                && matches!(op.argv[2].as_str(), "-b" | "-c")
                && !op.argv[3].starts_with('-')
            {
                return Ok(format!(
                    "new local branch; original HEAD is {head}, working files remain unchanged"
                ));
            }
            if op.argv.len() != 3 {
                return Err("branch switch needs one explicit target".into());
            }
            if op
                .argv
                .iter()
                .skip(2)
                .any(|a| a.starts_with('-') && !matches!(a.as_str(), "-b" | "-c"))
            {
                return Err("unsupported branch-switch options".into());
            }
            if probe
                .run(&["diff", "--quiet", "--no-ext-diff", "--no-textconv"])?
                .is_none()
                || probe
                    .run(&[
                        "diff",
                        "--cached",
                        "--quiet",
                        "--no-ext-diff",
                        "--no-textconv",
                    ])?
                    .is_none()
            {
                return Err("branch switch has uncommitted changes".into());
            }
            let files = probe
                .run(&["ls-files", "--others", "--exclude-standard"])?
                .ok_or("cannot inspect untracked files")?;
            if !files.is_empty() {
                return Err("branch switch has untracked files".into());
            }
            probe.small_switch(&op.argv[2])?;
            Ok(format!("clean worktree can return to commit {head}"))
        }
        "commit" if op.argv.len() == 4 && matches!(op.argv[2].as_str(), "-m" | "--message") => {
            Ok(format!(
                "new local commit; original HEAD is {}",
                head.ok_or("missing original HEAD")?
            ))
        }
        "fetch" | "ls-remote" => {
            if ctx.exported.contains("SSH_ASKPASS")
                && (ctx.unknown_variables.contains("SSH_ASKPASS")
                    || ctx
                        .variables
                        .get("SSH_ASKPASS")
                        .is_some_and(|value| !value.is_empty()))
            {
                return Err(
                    "remote authentication can execute SSH_ASKPASS; authorization required".into(),
                );
            }
            if op
                .argv
                .iter()
                .skip(2)
                .any(|a| a.starts_with('-') || a.starts_with('+') || a.contains(':'))
            {
                return Err("custom remote/refspec options need authorization".into());
            }
            if op.argv.len() > 3 {
                return Err("additional remote/refspec arguments need authorization".into());
            }
            let remote = op.argv.get(2).map(String::as_str).unwrap_or("origin");
            if sub == "fetch" {
                let refspec = probe
                    .run(&["config", "--get-all", &format!("remote.{remote}.fetch")])?
                    .ok_or("fetch has no configured destination scope")?;
                if refspec.trim_start_matches('+')
                    != format!("refs/heads/*:refs/remotes/{remote}/*")
                {
                    return Err(
                        "configured fetch refspec is outside the supported tracking-ref scope"
                            .into(),
                    );
                }
                for key in [
                    "fetch.prune".to_string(),
                    "fetch.pruneTags".into(),
                    format!("remote.{remote}.prune"),
                    format!("remote.{remote}.pruneTags"),
                    format!("remote.{remote}.mirror"),
                ] {
                    if probe.run(&["config", "--bool", "--get", &key])?.as_deref() == Some("true") {
                        return Err(format!(
                            "{key} enables destructive ref updates; authorization required"
                        )
                        .into());
                    }
                }
                if probe
                    .run(&["config", "--get", &format!("remote.{remote}.tagOpt")])?
                    .is_some_and(|option| !matches!(option.as_str(), "--no-tags" | "--tags"))
                {
                    return Err("unrecognized configured tag behavior needs authorization".into());
                }
            }
            let url = probe
                .run(&["remote", "get-url", remote])?
                .ok_or("remote URL is not known")?;
            if !(url.starts_with("https://")
                || url.starts_with("ssh://")
                || url.starts_with("git@"))
            {
                return Err("remote transport is not a known read-only Git transport".into());
            }
            let refs = probe
                .run(&[
                    "for-each-ref",
                    "--format=%(refname) %(objectname)",
                    "refs/remotes",
                    "refs/tags",
                ])?
                .ok_or("cannot capture remote-tracking refs")?;
            Ok(format!(
                "remote query; original tracking refs and tags: {}",
                if refs.is_empty() { "(none)" } else { &refs }
            ))
        }
        _ => Err("Git operation needs an explicit grant or stronger recovery evidence".into()),
    }
}

fn file_admission(op: &Operation, ctx: &Context) -> Result<String, String> {
    // Moves/copies have their own convenience category. Other content changes
    // still need authorization rather than a claimed recovery guarantee.
    if !matches!(base(op), "mkdir" | "touch") || op.paths.iter().any(|path| path.extra) {
        return Err(
            "this file content change is outside the automatic operation categories".into(),
        );
    }
    if op.known.iter().any(|known| !known) {
        return Err("file arguments or output contents are determined at runtime".into());
    }
    let paths = concrete_paths(op, ctx)?;
    let writes: Vec<_> = op
        .paths
        .iter()
        .zip(&paths)
        .filter(|(p, _)| matches!(p.kind, AccessKind::Write | AccessKind::Delete))
        .collect();
    if writes.is_empty() {
        return Err("no bounded file effect was identified".into());
    }
    let mut reasons = Vec::new();
    for (access, path) in writes {
        match metadata(path)? {
            None if access.kind != AccessKind::Delete => {
                if !metadata(path.parent().ok_or("target has no parent")?)?
                    .is_some_and(|m| m.is_dir())
                {
                    return Err("target parent does not already exist".into());
                }
                reasons.push(format!("new bounded target {}", path.display()));
            }
            None => return Err("delete target is missing".into()),
            Some(meta)
                if base(op) == "mkdir" && meta.is_dir() && op.argv.iter().any(|a| a == "-p") =>
            {
                reasons.push("directory already exists; no contents are replaced".into());
            }
            Some(meta) if base(op) == "touch" && meta.is_file() => {
                let modified = meta
                    .modified()
                    .map_err(|e| format!("cannot inspect previous modification time: {e}"))?;
                let accessed = meta
                    .accessed()
                    .map_err(|e| format!("cannot inspect previous access time: {e}"))?;
                reasons.push(format!(
                    "previous timestamps for {}: modified {modified:?}, accessed {accessed:?}",
                    path.display()
                ));
            }
            Some(_) => {
                return Err("file target is not a supported non-content-changing operation".into());
            }
        }
    }
    Ok(reasons.join("; "))
}

pub(crate) fn automatic(report: &RiskReport) -> Result<AutoAdmission, AutoRejection> {
    let mut build_scope = Vec::with_capacity(report.operations.len());
    for (i, op) in report.operations.iter().enumerate() {
        let own = development(op)
            && op.paths.iter().all(|p| !p.extra)
            && ordinary_variables(op, &report.context);
        let inherited = op
            .parent
            .filter(|parent| *parent < i)
            .is_some_and(|parent| build_scope[parent]);
        build_scope.push(own || inherited);
    }
    if report.incomplete && build_scope.iter().any(|covered| !covered) {
        return Err("the shell operation is not completely bounded".into());
    }
    if report.reads_protected {
        return Err("reads protected data".into());
    }
    let mut result = AutoAdmission {
        reasons: Vec::new(),
        development: false,
    };
    let mut previous_paths: Vec<(&Path, bool)> = Vec::new();
    let mut prior_effects_unknown = false;
    for (op, covered) in report.operations.iter().zip(build_scope) {
        if covered {
            if !result.development {
                result.reasons.push(
                    "common build/test/check or config initialization: project-code effects are not guaranteed recoverable"
                        .into(),
                );
            }
            result.development = true;
            prior_effects_unknown = true;
            continue;
        }
        if op.risk == Risk::Safe && op.paths.iter().all(|p| !p.extra) && op.variables.is_empty() {
            continue;
        }
        if op.risk >= Risk::Dangerous
            && (op.opaque
                || op.known.iter().any(|known| !known)
                || op.paths.iter().any(|path| path.resolved.is_none()))
        {
            return Err("contains an explicitly high-risk operation".into());
        }
        if !op.variables.is_empty() && !ordinary_variables(op, &report.context) {
            return Err("session changes are not limited to known, ordinary values".into());
        }
        if ordinary_move_or_copy(op) {
            result.reasons.push(format!(
                "ordinary {}: native file semantics, not an atomic recovery guarantee",
                base(op)
            ));
            continue;
        }
        if prior_effects_unknown {
            return Err("an earlier build or opaque operation can change later recovery evidence; split the calls or approve the program".into());
        }
        // Evidence is read before the whole shell program runs. Do not reuse
        // it after an earlier operation could have changed the same target.
        if previous_paths.len() + op.paths.len() > MAX_TARGETS {
            return Err(format!("combined file effects exceed {MAX_TARGETS} targets").into());
        }
        for access in &op.paths {
            if let Some(path) = &access.resolved {
                let writes = matches!(access.kind, AccessKind::Write | AccessKind::Delete);
                if previous_paths.iter().any(|(previous, changed)| {
                    (writes || *changed)
                        && (path.starts_with(previous) || previous.starts_with(path))
                }) {
                    return Err("overlapping file effects need authorization; initial recovery evidence may be stale".into());
                }
            }
        }
        previous_paths.extend(op.paths.iter().filter_map(|access| {
            access.resolved.as_deref().map(|path| {
                (
                    path,
                    matches!(access.kind, AccessKind::Write | AccessKind::Delete),
                )
            })
        }));
        if op.paths.iter().any(|path| path.extra) {
            result.reasons.push(file_admission(op, &report.context)?);
        } else if diagnostic(op, &report.context) {
            result
                .reasons
                .push("low-impact network diagnostic bounded by the execution deadline".into());
        } else if base(op) == "git" {
            result.reasons.push(git_admission(op, &report.context)?);
        } else if !op.paths.is_empty() {
            result.reasons.push(file_admission(op, &report.context)?);
        } else if !op.variables.is_empty()
            && !op.opaque
            && (op.argv.is_empty()
                || matches!(
                    base(op),
                    "cd" | "export" | "unset" | "declare" | "typeset" | "local"
                ))
            && op
                .argv
                .iter()
                .skip(1)
                .all(|a| !a.starts_with('-') || a == "--")
        {
            result.reasons.push(format!(
                "known previous session values retained for: {}",
                op.variables
                    .iter()
                    .map(|(name, _)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        } else if op.transparent && !op.opaque {
            continue;
        } else {
            return Err(format!(
                "effects of '{}' are not known to be low-impact",
                op.argv.join(" ")
            )
            .into());
        }
        prior_effects_unknown |= op.opaque;
    }
    if result.reasons.is_empty() {
        return Err("no automatic-admission evidence".into());
    }
    Ok(result)
}
