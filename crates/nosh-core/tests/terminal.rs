//! Real descriptors and controlling terminals, without loading a model.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use nosh_core::{AgentUi, JsonUi, TermUi};
use nosh_hub::{BarProgress, Progress};
use nosh_permissions::Risk;
use nosh_shell::{style, term};

#[cfg(target_os = "macos")]
#[global_allocator]
static INPUT_WORKER_ALLOCATOR: nosh_shell::input_assist::WorkerAllocator =
    nosh_shell::input_assist::WorkerAllocator;

const BEGIN: &str = "nosh-terminal-probe-begin\n";
const END: &str = "nosh-terminal-probe-end";
const TOOL_LABELS: [(Risk, &str); 4] = [
    (Risk::Safe, "SAFE \u{b7} auto"),
    (Risk::Mutating, "MUTATING \u{b7} approved"),
    (Risk::Dangerous, "DANGEROUS \u{b7} allowed (yolo)"),
    (Risk::Forbidden, "FORBIDDEN \u{b7} denied"),
];

struct Pty {
    master: File,
    slave: File,
}

fn size(columns: u16) -> libc::winsize {
    libc::winsize {
        ws_row: 24,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Pty {
    fn new(columns: u16) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut window = size(columns);
        // SAFETY: the output pointers and window size are valid; optional
        // name and termios arguments are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut window,
                )
            },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        for fd in [master, slave] {
            // SAFETY: openpty returned valid descriptors.
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        // SAFETY: each descriptor has exactly one owner after openpty.
        unsafe {
            Self {
                master: File::from_raw_fd(master),
                slave: File::from_raw_fd(slave),
            }
        }
    }
}

fn resize(fd: i32, columns: u16) {
    // SAFETY: callers supply an open PTY descriptor and a valid winsize.
    assert_eq!(
        unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size(columns)) },
        0
    );
}

fn read_output(mut reader: impl Read, observed: Arc<Mutex<Vec<u8>>>) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                let mut observed = observed.lock().unwrap();
                assert!(
                    observed.len() + n < 32 * 1024 * 1024,
                    "terminal capture limit"
                );
                observed.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Linux PTYs report EIO rather than EOF when the slave is closed.
                assert_eq!(e.raw_os_error(), Some(libc::EIO), "{e}");
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap().replace("\r\n", "\n")
}

fn framed(output: &str) -> String {
    output
        .split_once(BEGIN)
        .unwrap_or_else(|| panic!("{output:?}"))
        .1
        .split_once(END)
        .unwrap_or_else(|| panic!("{output:?}"))
        .0
        .to_string()
}

type KeyStep<'a> = (&'a str, &'a [u8]);

struct ProbeTimings {
    startup: Duration,
    input: Vec<Duration>,
}

struct Probe<'a> {
    mode: &'a str,
    terminal: Option<&'a str>,
    locale: Option<&'a str>,
    no_color: &'a str,
    stdout_tty: bool,
    stderr_tty: bool,
    stdin_pipe: bool,
    columns: u16,
    initial: &'a str,
    keys: Option<&'a [u8]>,
    steps: &'a [KeyStep<'a>],
    input_assist: bool,
}

impl Default for Probe<'_> {
    fn default() -> Self {
        Self {
            mode: "text",
            terminal: Some("xterm-256color"),
            locale: Some("C.UTF-8"),
            no_color: "",
            stdout_tty: false,
            stderr_tty: false,
            stdin_pipe: false,
            columns: 80,
            initial: "",
            keys: None,
            steps: &[],
            input_assist: true,
        }
    }
}

impl Probe<'_> {
    fn run(self) -> (String, String) {
        let (out, err, _) = self.run_with_timings();
        (out, err)
    }

    fn run_with_timings(self) -> (String, String, ProbeTimings) {
        let home = tempfile::tempdir().unwrap();
        let stdout = self.stdout_tty.then(|| Pty::new(80));
        let stderr = self.stderr_tty.then(|| Pty::new(self.columns));
        let input = stderr.as_ref().or(stdout.as_ref());
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "terminal_probe", "--nocapture"])
            .env("NOSH_TERMINAL_PROBE", self.mode)
            .env("NOSH_TERMINAL_INITIAL", self.initial)
            .env(
                "NOSH_TERMINAL_INPUT_ASSIST",
                if self.input_assist { "1" } else { "0" },
            )
            .env_remove("LC_ALL")
            .env_remove("LC_CTYPE")
            .env_remove("LANG")
            .env("NO_COLOR", self.no_color)
            .env("NOSH_LANG", "en")
            .env("NOSH_HOME", home.path())
            .env("HOME", home.path())
            .env_remove("CLICOLOR")
            .env_remove("NOSH_STATS")
            .stdin(if self.stdin_pipe {
                Stdio::piped()
            } else {
                input.map_or_else(Stdio::null, |t| t.slave.try_clone().unwrap().into())
            })
            .stdout(
                stdout
                    .as_ref()
                    .map_or_else(Stdio::piped, |t| t.slave.try_clone().unwrap().into()),
            )
            .stderr(
                stderr
                    .as_ref()
                    .map_or_else(Stdio::piped, |t| t.slave.try_clone().unwrap().into()),
            );
        match self.terminal {
            Some(term) => {
                command.env("TERM", term);
            }
            None => {
                command.env_remove("TERM");
            }
        }
        if let Some(locale) = self.locale {
            command.env("LC_ALL", locale);
        }
        if input.is_some() {
            let controlling_fd = if self.stderr_tty { 2 } else { 1 };
            // SAFETY: only async-signal-safe syscalls run between fork/exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() == -1
                        || libc::ioctl(controlling_fd, libc::TIOCSCTTY as _, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut input = input.map(|t| t.master.try_clone().unwrap());
        let launched = Instant::now();
        let mut child = command.spawn().unwrap();
        drop(command);
        if self.stdin_pipe {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"attachment\n")
                .unwrap();
        }
        let out: Box<dyn Read + Send> = match stdout {
            Some(t) => {
                drop(t.slave);
                Box::new(t.master)
            }
            None => Box::new(child.stdout.take().unwrap()),
        };
        let err: Box<dyn Read + Send> = match stderr {
            Some(t) => {
                drop(t.slave);
                Box::new(t.master)
            }
            None => Box::new(child.stderr.take().unwrap()),
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let out_observed = observed.clone();
        let err_observed = observed.clone();
        let out = thread::spawn(move || read_output(out, out_observed));
        let err = thread::spawn(move || read_output(err, err_observed));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut keys = self.keys;
        let mut steps = self.steps.iter();
        let mut next = steps.next();
        let mut observed_start = 0;
        let mut sent = Instant::now();
        let mut timings = Vec::new();
        let mut startup = None;
        let mut blocked_at = None;
        let marker = blocked_input_marker(child.id());
        let mut terminal_scan = 0;
        let mut cursor_replies = 0;
        let needs_cursor_reply = self.mode.starts_with("repl")
            && self
                .terminal
                .is_some_and(|t| !matches!(t, "" | "dumb" | "unknown"));
        let status = loop {
            if let Some(input) = input.as_mut() {
                let requests = {
                    let bytes = observed.lock().unwrap();
                    let requests = bytes[terminal_scan..]
                        .windows(4)
                        .filter(|s| *s == b"\x1b[6n")
                        .count();
                    terminal_scan = bytes.len().saturating_sub(3);
                    requests
                };
                for _ in 0..requests {
                    input.write_all(b"\x1b[1;1R").unwrap();
                    cursor_replies += 1;
                }
            }
            if (keys.is_some() || next.is_some()) && (!needs_cursor_reply || cursor_replies > 0) {
                let input = input.as_mut().unwrap();
                let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
                // SAFETY: tcgetattr writes to valid storage, read only on success.
                if unsafe { libc::tcgetattr(input.as_raw_fd(), attrs.as_mut_ptr()) } == 0
                    && unsafe { attrs.assume_init() }.c_lflag & libc::ICANON == 0
                {
                    startup.get_or_insert_with(|| launched.elapsed());
                    if let Some(bytes) = keys.take() {
                        observed_start = observed.lock().unwrap().len();
                        sent = Instant::now();
                        input.write_all(bytes).unwrap();
                    } else if let Some((needle, bytes)) = next {
                        let ready = if *needle == "@worker-blocked" {
                            marker.exists()
                        } else {
                            String::from_utf8_lossy(&observed.lock().unwrap()[observed_start..])
                                .contains(needle)
                        };
                        if ready {
                            if *needle == "@worker-blocked" {
                                blocked_at = Some(Instant::now());
                            } else {
                                timings.push(sent.elapsed());
                            }
                            observed_start = observed.lock().unwrap().len();
                            sent = Instant::now();
                            input.write_all(bytes).unwrap();
                            next = steps.next();
                        }
                    }
                }
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                if marker.exists() {
                    std::fs::remove_file(&marker).unwrap();
                }
                panic!(
                    "terminal probe timed out: {:?} {:?}",
                    out.join(),
                    err.join()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        let exited = Instant::now();
        let out = out.join().unwrap();
        let err = err.join().unwrap();
        if let Some(blocked_at) = blocked_at {
            std::fs::remove_file(marker).unwrap();
            assert!(
                exited.duration_since(blocked_at) < Duration::from_millis(500),
                "editing/cancel/exit waited for a diagnostic timeout: {:?}",
                exited.duration_since(blocked_at)
            );
            println!(
                "input_assist_blocked_edit_cancel_exit_us={}",
                exited.duration_since(blocked_at).as_micros()
            );
        }
        assert!(status.success(), "{status}: {out:?}\n{err:?}");
        assert!(next.is_none(), "probe exited before staged input completed");
        (
            framed(&out),
            framed(&err),
            ProbeTimings {
                startup: startup.unwrap_or_default(),
                input: timings,
            },
        )
    }
}

#[test]
fn terminal_probe() {
    let Ok(mode) = std::env::var("NOSH_TERMINAL_PROBE") else {
        return;
    };
    print!("{BEGIN}");
    eprint!("{BEGIN}");
    match mode.as_str() {
        "text" => {
            let mut ui = TermUi::new(true);
            ui.show_think = true;
            ui.think("thinking");
            ui.text("answer\r");
            ui.text("\n\u{1f469}\u{200d}\u{1f4bb}");
            ui.pause();
        }
        "lines" | "resize" => {
            let mut ui = TermUi::new(false);
            ui.output(&("\u{4e2d}".repeat(50) + "\n"), false);
            if mode == "resize" {
                resize(libc::STDERR_FILENO, 20);
                ui.output(&("\u{4e2d}".repeat(50) + "\n"), false);
            } else {
                ui.output(&("x\t".repeat(30) + "\n"), true);
            }
        }
        "status" => {
            let mut ui = TermUi::new(false);
            ui.prefill(1, 1000);
            ui.pause();
        }
        "approval-state" => {
            let mut ui = TermUi::new(false);
            for mode in [
                nosh_permissions::ApprovalMode::Confirm,
                nosh_permissions::ApprovalMode::Auto,
                nosh_permissions::ApprovalMode::Yolo,
            ] {
                ui.state(mode, nosh_core::ui::Activity::Running);
                ui.state(mode, nosh_core::ui::Activity::NeedsUser);
            }
        }
        "approval-card" => {
            let mut approval = nosh_core::TerminalApproval::default();
            let answer = nosh_core::ApprovalChannel::request(
                &mut approval,
                &nosh_core::ApprovalRequest {
                    tool: "run_command".into(),
                    command: "fixture-prohibited-operation".into(),
                    risk: Risk::Forbidden,
                    reasons: vec!["built-in prohibition".into()],
                    strong: true,
                    can_grant: false,
                    can_edit: true,
                    mode: nosh_permissions::ApprovalMode::Confirm,
                },
            );
            println!("{answer:?}");
        }
        "proposal" => {
            let mut ui = TermUi::new(false);
            ui.proposed("printf hello", Some("Suggested explanation.\nMore detail."));
            ui.text("Final answer.\n");
            ui.pause();
        }
        "tool-labels" => {
            let mut ui = TermUi::new(false);
            for (risk, label) in TOOL_LABELS {
                ui.tool_start("run_command", "echo \u{4e2d}\u{6587}", Some(risk), label);
            }
        }
        "input" | "input-pipe" => {
            let initial = std::env::var("NOSH_TERMINAL_INITIAL").unwrap();
            let answer = term::read_text("input> ", &initial);
            if mode == "input-pipe" {
                let mut attachment = String::new();
                std::io::stdin().read_to_string(&mut attachment).unwrap();
                println!(
                    "{}",
                    serde_json::json!({"answer": answer, "attachment": attachment})
                );
            } else {
                println!("{}", serde_json::to_string(&answer).unwrap());
            }
            // The raw-mode guard must also restore the terminal after cancellation.
            assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
        }
        "progress" => {
            let progress = BarProgress::new();
            progress.start("model.gguf", 100, 0);
            progress.note("source changed");
            progress.finish(false);
        }
        "available" => println!("{}", term::available()),
        "json" => {
            let mut ui = JsonUi;
            ui.state(
                nosh_permissions::ApprovalMode::Auto,
                nosh_core::ui::Activity::Running,
            );
            ui.text("\u{1f469}\u{200d}\u{1f4bb}");
            ui.output("\x1b[31mraw\r\n", true);
            for (risk, label) in TOOL_LABELS {
                ui.tool_start("run_command", "echo \u{4e2d}\u{6587}", Some(risk), label);
            }
        }
        "repl" | "repl-blocked" => {
            let mut shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
                interactive: true,
                ..nosh_shell::ShellOptions::default()
            })
            .unwrap();
            shell.run_user_line("PATH=/usr/bin:/bin; PS1='probe> '");
            let worker = nosh_shell::input_assist::WorkerCommand {
                program: std::env::current_exe().unwrap(),
                args: vec![
                    "--exact".into(),
                    if mode == "repl-blocked" {
                        "blocked_input_worker_probe".into()
                    } else {
                        "input_worker_probe".into()
                    },
                    "--nocapture".into(),
                ],
            };
            let config = nosh_shell::ReplConfig {
                trigger: nosh_shell::TriggerConfig {
                    ai_enabled: false,
                    ..Default::default()
                },
                input_assist: nosh_shell::input_assist::Config {
                    enabled: std::env::var("NOSH_TERMINAL_INPUT_ASSIST").as_deref() != Ok("0"),
                    worker: Some(worker),
                },
                ..Default::default()
            };
            assert_eq!(
                nosh_shell::repl::run(&mut shell, &mut nosh_shell::repl::NoAi, config),
                if mode == "repl-blocked" { 130 } else { 0 }
            );
        }
        _ => panic!("unknown probe"),
    }
    println!("{END}");
    eprintln!("{END}");
}

#[test]
fn approval_modes_and_activity_are_separate_and_do_not_decorate_pipes() {
    for terminal in ["xterm-256color", "dumb"] {
        let (_, err) = Probe {
            mode: "approval-state",
            terminal: Some(terminal),
            stderr_tty: true,
            ..Default::default()
        }
        .run();
        for mode in ["Confirm", "Auto", "YOLO"] {
            assert!(err.contains(&format!("Approval: {mode}")), "{err}");
        }
        assert!(err.contains("| Running"), "{err}");
        assert!(err.contains("| Needs your attention"), "{err}");
        if terminal == "dumb" {
            assert!(!err.contains('\x1b'));
        }
    }
    let (out, err) = Probe {
        mode: "approval-state",
        ..Default::default()
    }
    .run();
    assert!(out.trim().is_empty());
    assert!(err.trim().is_empty());
}

#[test]
fn built_in_prohibition_requires_yes_and_never_offers_a_session_grant() {
    let (out, err) = Probe {
        mode: "approval-card",
        stderr_tty: true,
        keys: Some(b"yes\r"),
        ..Default::default()
    }
    .run();
    assert_eq!(out.trim(), "Approve");
    assert!(err.contains("Approval: Confirm"));
    assert!(err.contains("Awaiting approval"));
    assert!(err.contains("type yes"));
    assert!(!err.contains("[a]"));
}

#[test]
fn input_worker_probe() {
    if let Some(code) = nosh_shell::input_assist::run_worker_from_env() {
        std::process::exit(code);
    }
}

fn blocked_input_marker(parent: u32) -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp").join(format!("nosh-pty-input-blocked-{parent}"))
}

#[test]
fn blocked_input_worker_probe() {
    if std::env::var("NOSH_INPUT_WORKER").as_deref() == Ok("lookup") {
        // SAFETY: getppid takes no pointers and reports this worker's parent.
        let parent = unsafe { libc::getppid() };
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(blocked_input_marker(parent as u32))
            .unwrap();
        while unsafe { libc::getppid() } == parent {
            thread::park_timeout(Duration::from_secs(1));
        }
        std::process::exit(0);
    }
    input_worker_probe();
}

#[test]
fn input_assist_updates_without_another_key_and_keeps_unicode_input_unchanged() {
    for no_color in ["", "1"] {
        let initial = "printf '%s\\n' '中文e\u{301}👩\u{200d}💻";
        let steps: &[KeyStep<'_>] = &[("Incomplete:", b"'\rexit 0\r")];
        let (out, err) = Probe {
            mode: "repl",
            stdout_tty: true,
            stderr_tty: true,
            keys: Some(initial.as_bytes()),
            steps,
            no_color,
            ..Default::default()
        }
        .run();
        assert_eq!(style::strip_ansi(&out).trim(), "中文e\u{301}👩\u{200d}💻");
        assert!(err.contains("Incomplete:"), "{err:?}");
        if no_color == "1" {
            for color in ["\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[35m"] {
                assert!(!err.contains(color), "{err:?}");
            }
        }
    }
}

#[test]
fn input_assist_blocked_worker_does_not_delay_edit_cancel_or_exit() {
    let steps: &[KeyStep<'_>] = &[
        ("@worker-blocked", b"printf '%s\\n' EDITx"),
        ("EDITx", b"\x7f\r"),
        ("EDIT\r\n", b"\x03"),
        ("probe> ", b"\x04"),
    ];
    let (out, _) = Probe {
        mode: "repl-blocked",
        stdout_tty: true,
        stderr_tty: true,
        no_color: "1",
        steps,
        ..Default::default()
    }
    .run();
    assert_eq!(style::strip_ansi(&out).trim(), "EDIT");
}

#[test]
fn input_assist_can_be_disabled_without_changing_submission() {
    let keys = b"printf '%s\\n' unchanged\rexit 0\r";
    for enabled in [false, true] {
        let (out, _) = Probe {
            mode: "repl",
            stdout_tty: true,
            stderr_tty: true,
            input_assist: enabled,
            keys: Some(keys),
            ..Default::default()
        }
        .run();
        assert_eq!(style::strip_ansi(&out).trim(), "unchanged");
    }
}

#[test]
#[ignore = "fixed-device input latency comparison; no model"]
fn input_assist_latency_comparison() {
    for single_key in [true, false] {
        let mut off = Vec::new();
        let mut on = Vec::new();
        let mut startup_off = Vec::new();
        let mut startup_on = Vec::new();
        for round in 0..5 {
            for enabled in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let text: Vec<String> = (0..40)
                    .map(|i| {
                        if single_key {
                            format!("echo {}", "x".repeat(i + 1))
                        } else {
                            format!("echo latency_{i:02}")
                        }
                    })
                    .collect();
                let keys: Vec<Vec<u8>> = text
                    .iter()
                    .skip(1)
                    .map(|line| {
                        if single_key {
                            vec![b'x']
                        } else {
                            format!("\x15{line}").into_bytes()
                        }
                    })
                    .chain(std::iter::once(b"\x15exit 0\r".to_vec()))
                    .collect();
                let steps: Vec<KeyStep<'_>> = text
                    .iter()
                    .zip(&keys)
                    .map(|(text, keys)| (text.as_str(), keys.as_slice()))
                    .collect();
                let (_, _, timings) = Probe {
                    mode: "repl",
                    stdout_tty: true,
                    stderr_tty: true,
                    no_color: "1",
                    input_assist: enabled,
                    keys: Some(text[0].as_bytes()),
                    steps: &steps,
                    ..Default::default()
                }
                .run_with_timings();
                if enabled { &mut on } else { &mut off }
                    .extend(timings.input.into_iter().skip(usize::from(single_key)));
                if enabled {
                    &mut startup_on
                } else {
                    &mut startup_off
                }
                .push(timings.startup);
            }
        }
        for (enabled, mut values, mut startup) in
            [(false, off, startup_off), (true, on, startup_on)]
        {
            values.sort_unstable();
            startup.sort_unstable();
            println!(
                "{}",
                serde_json::json!({
                    "input_assist": enabled,
                    "input_pattern": if single_key { "single-key" } else { "replacement-burst" },
                    "samples": values.len(),
                    "observer_poll_ms": 5,
                    "p50_us": values[values.len() / 2].as_micros(),
                    "p95_us": values[values.len() * 95 / 100].as_micros(),
                    "p99_us": values[values.len() * 99 / 100].as_micros(),
                    "max_us": values.last().unwrap().as_micros(),
                    "startup_p50_us": startup[startup.len() / 2].as_micros(),
                    "startup_max_us": startup.last().unwrap().as_micros(),
                })
            );
        }
    }
}
#[test]
fn redirected_answers_have_no_thinking_newline_or_decoration() {
    for stderr_tty in [false, true] {
        let (out, err) = Probe {
            stderr_tty,
            ..Probe::default()
        }
        .run();
        assert_eq!(out, "answer\n\u{1f469}\u{200d}\u{1f4bb}\n");
        assert!(style::strip_ansi(&err).contains("thinking\n"));
        assert_eq!(err.contains("\x1b["), stderr_tty);
    }
    let (out, err) = Probe {
        stdout_tty: true,
        ..Probe::default()
    }
    .run();
    assert!(
        out.contains("\x1b[36m"),
        "stdout colors do not depend on stderr"
    );
    assert!(!err.contains('\x1b'));
}

#[test]
fn terminal_and_locale_matrix_has_safe_fallbacks() {
    for terminal in [None, Some("dumb"), Some("unknown"), Some("")] {
        let (out, err) = Probe {
            terminal,
            stdout_tty: true,
            stderr_tty: true,
            ..Probe::default()
        }
        .run();
        assert!(!out.contains('\x1b') && !err.contains('\x1b'));
        assert!(out.starts_with("| answer\n"), "{out:?}");
        let (_, err) = Probe {
            mode: "status",
            terminal,
            stderr_tty: true,
            ..Probe::default()
        }
        .run();
        assert!(err.is_empty());
    }
    for terminal in ["xterm-256color", "screen", "tmux-256color", "rxvt-unicode"] {
        let (out, err) = Probe {
            terminal: Some(terminal),
            no_color: "1",
            stdout_tty: true,
            stderr_tty: true,
            ..Probe::default()
        }
        .run();
        assert!(!out.contains('\x1b') && !err.contains('\x1b'));
        assert!(out.starts_with("\u{2503} answer\n"));
    }
    for locale in [
        None,
        Some(""),
        Some("C"),
        Some("POSIX"),
        Some("zh_CN.GB18030"),
    ] {
        let (out, _) = Probe {
            locale,
            stdout_tty: true,
            no_color: "1",
            ..Probe::default()
        }
        .run();
        assert!(out.starts_with("| answer\n"));
        assert!(
            out.contains("\u{1f469}\u{200d}\u{1f4bb}"),
            "data is never transliterated"
        );
    }
    let (_, err) = Probe {
        mode: "status",
        stderr_tty: true,
        no_color: "1",
        ..Probe::default()
    }
    .run();
    assert!(
        err.contains("\r\x1b[K"),
        "NO_COLOR alone does not disable cursor control"
    );
    assert!(!err.contains("\x1b[2m"));
}

#[test]
fn tool_labels_follow_the_destination_without_rewriting_command_text() {
    for (terminal, locale, stderr_tty, unicode) in [
        ("xterm-256color", Some("C.UTF-8"), true, true),
        ("dumb", Some("C.UTF-8"), true, false),
        ("xterm-256color", Some("C"), true, false),
        ("xterm-256color", None, true, false),
        ("xterm-256color", Some("C.UTF-8"), false, false),
    ] {
        for no_color in ["", "1"] {
            let (out, err) = Probe {
                mode: "tool-labels",
                terminal: Some(terminal),
                locale,
                no_color,
                stderr_tty,
                ..Probe::default()
            }
            .run();
            assert!(out.is_empty());
            let text = style::strip_ansi(&err);
            let lines: Vec<_> = text.lines().collect();
            assert_eq!(lines.len(), TOOL_LABELS.len() * 2);
            let (bar, marker, separator) = if unicode {
                ("\u{2503}", "\u{2699}", " \u{b7} ")
            } else {
                ("|", "*", " | ")
            };
            for (i, (_, label)) in TOOL_LABELS.iter().enumerate() {
                assert_eq!(
                    lines[2 * i],
                    format!(
                        "{bar} {marker} run_command  {}",
                        label.replace(" \u{b7} ", separator)
                    ),
                    "{terminal} {locale:?} tty={stderr_tty} NO_COLOR={no_color:?}"
                );
                if !unicode {
                    assert!(lines[2 * i].is_ascii());
                }
                assert_eq!(lines[2 * i + 1], format!("{bar}   $ echo \u{4e2d}\u{6587}"));
            }
        }
    }
}

#[test]
fn proposal_explanations_keep_the_detail_prefix_in_both_renderers() {
    for (terminal, bar, arrow) in [
        ("xterm-256color", "\u{2503}", "\u{21b3}"),
        ("dumb", "|", "->"),
    ] {
        let (out, err) = Probe {
            mode: "proposal",
            terminal: Some(terminal),
            stderr_tty: true,
            no_color: "1",
            ..Probe::default()
        }
        .run();
        assert!(out.is_empty());
        assert_eq!(
            err,
            format!(
                "{bar} {arrow} printf hello\n{bar}   Suggested explanation.\n{bar}   More detail.\n{bar} Final answer.\n"
            )
        );
    }
}

#[test]
fn clipping_uses_stderr_size_and_observes_resizes() {
    for (columns, chinese) in [(80, 37), (20, 7), (8, 1)] {
        let (_, err) = Probe {
            mode: "lines",
            stdout_tty: true,
            stderr_tty: true,
            columns,
            no_color: "1",
            ..Probe::default()
        }
        .run();
        let lines: Vec<_> = err.lines().collect();
        assert_eq!(lines[0].matches('\u{4e2d}').count(), chinese);
        assert!(
            lines.iter().all(|l| style::width(l) < usize::from(columns)),
            "{err:?}"
        );
        assert!(!err.contains('\t'));
    }
    let (_, err) = Probe {
        mode: "resize",
        stderr_tty: true,
        no_color: "1",
        ..Probe::default()
    }
    .run();
    let lines: Vec<_> = err.lines().collect();
    assert_eq!(lines[0].matches('\u{4e2d}').count(), 37);
    assert_eq!(lines[1].matches('\u{4e2d}').count(), 7);
}

#[test]
fn editing_unicode_keeps_echo_and_buffer_in_sync() {
    for initial in [
        "\u{1f600}",
        "\u{20bb7}",
        "e\u{301}",
        "\u{1f469}\u{200d}\u{1f4bb}",
        "\u{1f44d}\u{1f3fd}",
        "\u{1f1e8}\u{1f1f3}",
        "\u{2764}\u{fe0f}",
    ] {
        for terminal in ["xterm-256color", "dumb"] {
            let (out, err) = Probe {
                mode: "input",
                initial,
                keys: Some(b"\x7f\r"),
                stderr_tty: true,
                terminal: Some(terminal),
                ..Probe::default()
            }
            .run();
            assert_eq!(
                serde_json::from_str::<Option<String>>(out.trim()).unwrap(),
                Some(String::new())
            );
            assert_eq!(err.contains('\x1b'), terminal != "dumb");
        }
    }
    let (out, _) = Probe {
        mode: "input",
        initial: "old",
        stderr_tty: true,
        columns: 12,
        keys: Some(
            "\x15\x1b[200~\u{4e2d}\u{6587}\u{1f469}\u{200d}\u{1f4bb}\x1b[201~\x7f\r".as_bytes(),
        ),
        ..Probe::default()
    }
    .run();
    assert_eq!(
        serde_json::from_str::<Option<String>>(out.trim())
            .unwrap()
            .as_deref(),
        Some("\u{4e2d}\u{6587}")
    );
    for keys in [b"\x03", b"\x04"] {
        let (out, _) = Probe {
            mode: "input",
            stderr_tty: true,
            keys: Some(keys),
            ..Probe::default()
        }
        .run();
        assert_eq!(out, "null\n");
    }
}

#[test]
fn hidden_prompts_are_unavailable_and_plain_download_logs_keep_notes() {
    let (out, _) = Probe {
        mode: "available",
        stdout_tty: true,
        ..Probe::default()
    }
    .run();
    assert_eq!(out, "false\n");
    for stderr_tty in [false, true] {
        let (_, err) = Probe {
            mode: "progress",
            terminal: Some("dumb"),
            stderr_tty,
            ..Probe::default()
        }
        .run();
        assert!(err.contains("downloading model.gguf") && err.contains("source changed"));
        assert!(!err.contains('\x1b'));
    }
}

#[test]
fn dumb_repl_executes_multiline_commands_without_escape_sequences() {
    let (out, err) = Probe {
        mode: "repl",
        terminal: Some("dumb"),
        stderr_tty: true,
        stdout_tty: true,
        keys: Some(b"for x in one two; do\rprintf '%s\\n' \"$x\"\rdone\rexit\r"),
        ..Probe::default()
    }
    .run();
    assert_eq!(out, "one\ntwo\n");
    assert!(!err.contains('\x1b'), "{err:?}");
}

#[test]
fn prompts_use_the_controlling_terminal_without_consuming_piped_stdin() {
    let (out, err) = Probe {
        mode: "input-pipe",
        stdin_pipe: true,
        stderr_tty: true,
        keys: Some(b"yes\r"),
        ..Probe::default()
    }
    .run();
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["answer"], "yes", "{err:?}");
    assert_eq!(result["attachment"], "attachment\n", "{err:?}");
}

#[test]
fn json_output_keeps_original_text_on_both_pipes_and_terminals() {
    for stdout_tty in [false, true] {
        for (terminal, locale) in [
            ("xterm-256color", Some("C.UTF-8")),
            ("dumb", Some("C.UTF-8")),
            ("xterm-256color", Some("C")),
        ] {
            let (out, err) = Probe {
                mode: "json",
                terminal: Some(terminal),
                locale,
                stdout_tty,
                ..Probe::default()
            }
            .run();
            let events: Vec<serde_json::Value> = out
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(events.len(), 2 + TOOL_LABELS.len());
            assert_eq!(events[0]["text"], "\u{1f469}\u{200d}\u{1f4bb}");
            assert_eq!(events[1]["text"], "\x1b[31mraw\r\n");
            assert_eq!(events[1]["stream"], "stderr");
            for (event, (risk, label)) in events[2..].iter().zip(TOOL_LABELS) {
                assert_eq!(event["risk"], risk.label());
                assert_eq!(event["decision"], label);
                assert_eq!(event["detail"], "echo \u{4e2d}\u{6587}");
            }
            assert!(!out.contains('\x1b') && err.is_empty());
        }
    }
}
