//! Real descriptors and controlling terminals, without loading a model.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use nosh_core::{AgentUi, JsonUi, TermUi};
use nosh_hub::{BarProgress, Progress};
use nosh_shell::{style, term};

const BEGIN: &str = "nosh-terminal-probe-begin\n";
const END: &str = "nosh-terminal-probe-end";

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

fn read_output(mut reader: impl Read) -> String {
    let mut bytes = Vec::new();
    if let Err(e) = reader.read_to_end(&mut bytes) {
        // Linux PTYs report EIO rather than EOF when the slave is closed.
        assert_eq!(e.raw_os_error(), Some(libc::EIO), "{e}");
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

struct Probe<'a> {
    mode: &'a str,
    terminal: Option<&'a str>,
    locale: &'a str,
    no_color: &'a str,
    stdout_tty: bool,
    stderr_tty: bool,
    stdin_pipe: bool,
    columns: u16,
    initial: &'a str,
    keys: Option<&'a [u8]>,
}

impl Default for Probe<'_> {
    fn default() -> Self {
        Self {
            mode: "text",
            terminal: Some("xterm-256color"),
            locale: "C.UTF-8",
            no_color: "",
            stdout_tty: false,
            stderr_tty: false,
            stdin_pipe: false,
            columns: 80,
            initial: "",
            keys: None,
        }
    }
}

impl Probe<'_> {
    fn run(self) -> (String, String) {
        let home = tempfile::tempdir().unwrap();
        let stdout = self.stdout_tty.then(|| Pty::new(80));
        let stderr = self.stderr_tty.then(|| Pty::new(self.columns));
        let input = stderr.as_ref().or(stdout.as_ref());
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "terminal_probe", "--nocapture"])
            .env("NOSH_TERMINAL_PROBE", self.mode)
            .env("NOSH_TERMINAL_INITIAL", self.initial)
            .env("LC_ALL", self.locale)
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
        if input.is_some() {
            let controlling_fd = if self.stderr_tty { 2 } else { 1 };
            // SAFETY: only async-signal-safe syscalls run between fork/exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() == -1 || libc::ioctl(controlling_fd, libc::TIOCSCTTY, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut input = input.map(|t| t.master.try_clone().unwrap());
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
        let out = thread::spawn(move || read_output(out));
        let err = thread::spawn(move || read_output(err));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut keys = self.keys;
        let status = loop {
            if let Some(bytes) = keys {
                let input = input.as_mut().unwrap();
                let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
                // SAFETY: tcgetattr writes to valid storage, read only on success.
                if unsafe { libc::tcgetattr(input.as_raw_fd(), attrs.as_mut_ptr()) } == 0
                    && unsafe { attrs.assume_init() }.c_lflag & libc::ICANON == 0
                {
                    input.write_all(bytes).unwrap();
                    keys = None;
                }
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!(
                    "terminal probe timed out: {:?} {:?}",
                    out.join(),
                    err.join()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        let out = out.join().unwrap();
        let err = err.join().unwrap();
        assert!(status.success(), "{status}: {out:?}\n{err:?}");
        (framed(&out), framed(&err))
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
            ui.text("\u{1f469}\u{200d}\u{1f4bb}");
            ui.output("\x1b[31mraw\r\n", true);
        }
        "repl" => {
            let mut shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
                interactive: true,
                ..nosh_shell::ShellOptions::default()
            })
            .unwrap();
            assert_eq!(
                nosh_shell::repl::run(
                    &mut shell,
                    &mut nosh_shell::repl::NoAi,
                    nosh_shell::ReplConfig::default()
                ),
                0
            );
        }
        _ => panic!("unknown probe"),
    }
    println!("{END}");
    eprintln!("{END}");
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
    for locale in ["C", "POSIX", "zh_CN.GB18030"] {
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
    let (out, _) = Probe {
        mode: "input-pipe",
        stdin_pipe: true,
        stderr_tty: true,
        keys: Some(b"yes\r"),
        ..Probe::default()
    }
    .run();
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["answer"], "yes");
    assert_eq!(result["attachment"], "attachment\n");
}

#[test]
fn json_output_keeps_original_text_on_both_pipes_and_terminals() {
    for stdout_tty in [false, true] {
        let (out, err) = Probe {
            mode: "json",
            stdout_tty,
            ..Probe::default()
        }
        .run();
        let events: Vec<serde_json::Value> = out
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert_eq!(events[0]["text"], "\u{1f469}\u{200d}\u{1f4bb}");
        assert_eq!(events[1]["text"], "\x1b[31mraw\r\n");
        assert_eq!(events[1]["stream"], "stderr");
        assert!(!out.contains('\x1b') && err.is_empty());
    }
}
