//! Real session PTYs and a persistent brush host; no model or terminal emulator.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nosh_shell::pty::{Control, SessionPty, Snapshot};
use nosh_shell::repl::{GuardChoice, Pipeline, ReplUi};
use nosh_shell::{
    AgentExecOpts, AiHandler, AiOutcome, AiRequest, Badge, CaptureUserOutput, EmbeddedShell,
    NullSink, OutputState, OutputUnavailable, ReplConfig, ShellOptions,
};

const ROLE: &str = "NOSH_PTY_TEST_ROLE";
const CASE: &str = "NOSH_PTY_TEST_CASE";

fn execute(shell: &mut EmbeddedShell, control: &Control, id: u64, line: &str) -> (i32, Snapshot) {
    control.begin(id).unwrap();
    let result = shell.run_user_line(line);
    let output = control.finish(id).unwrap();
    (result.exit_code, output)
}

#[derive(Default)]
struct RecordingAi(Vec<AiRequest>);

impl AiHandler for RecordingAi {
    fn handle(&mut self, _: &mut EmbeddedShell, request: AiRequest) -> AiOutcome {
        self.0.push(request);
        AiOutcome::default()
    }
    fn builtin(&mut self, _: &mut EmbeddedShell, _: &[String]) -> AiOutcome {
        AiOutcome::default()
    }
    fn suggest(&mut self, _: &mut EmbeddedShell, _: &str) -> Option<String> {
        None
    }
    fn badge(&self) -> Badge {
        Badge::default()
    }
}

struct QuietUi;
impl ReplUi for QuietUi {
    fn guard(&mut self, _: &str) -> GuardChoice {
        GuardChoice::Run
    }
    fn notice(&mut self, _: &str) {}
}

#[test]
fn pty_probe() {
    let Ok(role) = std::env::var(ROLE) else {
        return;
    };
    if role == "relay" {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args(["--exact", "pty_probe", "--nocapture", "--test-threads=1"]);
        command.env(ROLE, "host");
        let relay = SessionPty::spawn(&mut command).unwrap();
        drop(command);
        std::process::exit(relay.run().unwrap());
    }
    assert_eq!(role, "host");
    // SAFETY: the isolated probe has no concurrent environment readers.
    let control = unsafe { nosh_shell::pty::inherited_control() }
        .unwrap()
        .unwrap();
    let mut shell = EmbeddedShell::new(ShellOptions {
        interactive: true,
        ..ShellOptions::default()
    })
    .unwrap();
    let _terminal = brush_core::terminal::TerminalControl::acquire().unwrap();
    shell.start_interactive();
    match std::env::var(CASE).unwrap().as_str() {
        "boundaries" => {
            for id in 1..=500 {
                print!("prompt-{id}> ");
                let expected = format!("command-{id}");
                let (exit, out) =
                    execute(&mut shell, &control, id, &format!("printf '{expected}'"));
                assert_eq!(exit, 0);
                assert_eq!(out.text, expected, "command {id}");
            }
            let (exit, empty) = execute(
                &mut shell,
                &control,
                501,
                "local_value=before; f() { printf 'function:%s' \"$local_value\"; }; alias show='f'; mkdir child; cd child; export VIRTUAL_ENV=\"$PWD/venv\"",
            );
            assert_eq!(exit, 0);
            assert_eq!(empty.observed, 0);
            assert!(empty.text.is_empty());
            let result = shell
                .run_agent_command(
                    "local_value=after; printf '%s' \"$VIRTUAL_ENV\"",
                    &AgentExecOpts::default(),
                    &mut NullSink,
                )
                .unwrap();
            assert!(result.stdout.ends_with("child/venv"));
            let (exit, output) = execute(&mut shell, &control, 502, "show");
            assert_eq!(exit, 0);
            assert_eq!(output.text, "function:after");
            let (exit, output) = execute(&mut shell, &control, 503, "printf hidden > redirected");
            assert_eq!(exit, 0);
            assert!(output.text.is_empty());
            assert_eq!(output.observed, 0);
            assert_eq!(
                std::fs::read(shell.cwd().join("redirected")).unwrap(),
                b"hidden"
            );
            let (_, output) = execute(
                &mut shell,
                &control,
                504,
                "printf '\\033[?1049h\\033[31mraw\\377\\r\\n\\033[?1049l'",
            );
            assert!(output.full_screen);
            assert!(output.text.is_empty());
        }
        "input-signals" => {
            let (exit, out) = execute(
                &mut shell,
                &control,
                1,
                "printf 'READY_INPUT\\n'; read answer; printf 'answer:%s\\n' \"$answer\"",
            );
            assert_eq!(exit, 0);
            assert!(out.text.contains("answer:hello"));
            let (exit, _) = execute(
                &mut shell,
                &control,
                2,
                "python3 -c 'import os,time; assert os.tcgetpgrp(0) == os.getpgrp(); print(\"READY_INT\", flush=True); time.sleep(30)'",
            );
            assert_eq!(exit, 130);
            let (exit, _) = execute(
                &mut shell,
                &control,
                3,
                "python3 -c 'import os,time; assert os.tcgetpgrp(0) == os.getpgrp(); print(\"READY_STOP\", flush=True); time.sleep(0.1); print(\"RESUMED\", flush=True)'",
            );
            assert_ne!(exit, 0, "the foreground command must stop");
            let (_, out) = execute(&mut shell, &control, 4, "fg");
            assert!(out.text.contains("RESUMED"));
            let (exit, out) = execute(
                &mut shell,
                &control,
                5,
                "printf 'READY_RESIZE\\n'; read answer; stty size",
            );
            assert_eq!(exit, 0);
            assert!(out.text.contains("35 111"));
        }
        "large" => {
            let (exit, out) = execute(
                &mut shell,
                &control,
                1,
                "python3 -c 'import os; chunk=b\"X\"*16384\nfor _ in range(6400): os.write(1,chunk)\nos.write(1,b\"LAST_OUTPUT\")'",
            );
            assert_eq!(exit, 0);
            assert_eq!(out.observed, 100 * 1024 * 1024 + 11);
            assert_eq!(out.text.len(), 4096);
            assert!(out.text.ends_with("LAST_OUTPUT"));
            assert!(out.truncated);
        }
        "evidence" => {
            shell.configure_output_capture(CaptureUserOutput::Last, Some(control.clone()));
            std::fs::write("calls", "0").unwrap();
            std::fs::write(
                "once.sh",
                "n=$(cat calls); n=$((n + 1)); printf '%s' \"$n\" > calls\n\
                 if [ \"$n\" = 1 ]; then printf 'ONCE_ERROR: missing REGION\\n' >&2; fi\nexit 17\n",
            )
            .unwrap();
            let mut pipeline = Pipeline::new(ReplConfig::default());
            let mut ai = RecordingAi::default();
            pipeline.process(&mut shell, &mut ai, &mut QuietUi, "sh once.sh");
            pipeline.process(&mut shell, &mut ai, &mut QuietUi, "ai fix");
            let request = &ai.0[0];
            let output = request.user_output.as_ref().unwrap();
            assert_eq!(output.state, OutputState::Captured);
            assert_eq!(output.command_id, request.failed.as_ref().unwrap().id);
            assert_eq!(output.text, "ONCE_ERROR: missing REGION\n");
            assert_eq!(std::fs::read_to_string("calls").unwrap(), "1");
            let first_id = output.command_id;

            pipeline.process(&mut shell, &mut ai, &mut QuietUi, "sh -c 'exit 17'");
            pipeline.process(&mut shell, &mut ai, &mut QuietUi, "#");
            let empty = ai.0[1].user_output.as_ref().unwrap();
            assert_eq!(empty.state, OutputState::Captured);
            assert_eq!(empty.observed_bytes, Some(0));
            assert!(empty.text.is_empty());
            assert_eq!(empty.command_id, first_id + 1);
            pipeline.process(&mut shell, &mut ai, &mut QuietUi, "echo )");
            assert!(
                ai.0.last().unwrap().user_output.is_none(),
                "parse errors did not execute a new command"
            );
            for command in ["printf '' | grep -q absent", "sh -c 'exit 130'"] {
                pipeline.process(&mut shell, &mut ai, &mut QuietUi, command);
                assert!(pipeline.last_failure().is_none());
                let before = ai.0.len();
                pipeline.fix(&mut shell, &mut ai, &mut QuietUi);
                assert_eq!(before, ai.0.len(), "stale failure after {command}");
            }
            shell.run_user_line("sleep 0.1 & printf foreground");
            let mixed = shell.last_user_output().unwrap();
            assert!(mixed.mixed);
            assert!(mixed.text.is_empty());
            assert!(!mixed.has_body());
            shell.run_user_line("wait");
            shell.run_user_line("printf '\\033[?1049hscreen\\033[?1049l'");
            assert_eq!(
                shell.last_user_output().unwrap().state,
                OutputState::Unavailable(OutputUnavailable::FullScreen)
            );
            let cwd = shell.cwd();
            shell.run_user_line("mkdir next; cd next");
            assert_eq!(shell.last_user_output().unwrap().cwd, cwd.to_string_lossy());

            shell.configure_output_capture(CaptureUserOutput::Off, None);
            shell.run_user_line("printf ''");
            assert_eq!(
                shell.last_user_output().unwrap().state,
                OutputState::NotCaptured
            );
            shell.configure_output_capture(CaptureUserOutput::Last, None);
            shell.run_user_line("printf ''");
            assert_eq!(
                shell.last_user_output().unwrap().state,
                OutputState::Unavailable(OutputUnavailable::NoPty)
            );
        }
        "flush" => {
            crossterm::terminal::enable_raw_mode().unwrap();
            print!("READY_FLUSH\r\n");
            std::io::stdout().flush().unwrap();
            let mut input = libc::pollfd {
                fd: 0,
                events: libc::POLLIN,
                revents: 0,
            };
            assert_eq!(unsafe { libc::poll(&mut input, 1, 5000) }, 1);
            nosh_shell::term::flush_input().unwrap();
            print!("READY_FRESH\r\n");
            std::io::stdout().flush().unwrap();
            let mut key = [0];
            std::io::stdin().read_exact(&mut key).unwrap();
            assert_eq!(key, *b"n", "old typeahead reached the new prompt");
            crossterm::terminal::disable_raw_mode().unwrap();
        }
        other => panic!("unknown probe {other}"),
    }
    shell.end_interactive();
}

fn run_probe(case: &str) -> (Vec<u8>, usize) {
    let home = tempfile::tempdir().unwrap();
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: the output descriptors and window size are valid.
    assert_eq!(
        unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                &raw mut size,
            )
        },
        0
    );
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
            -1
        );
    }
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "pty_probe", "--nocapture", "--test-threads=1"])
        .env(ROLE, "relay")
        .env(CASE, case)
        .env("HOME", home.path())
        .env("NOSH_HOME", home.path())
        .env("TERM", "xterm-256color")
        .env("LC_ALL", "C.UTF-8")
        .env("NOSH_DISABLE_AI", "1")
        .env_remove("NOSH_INTERNAL_PTY_FD")
        .current_dir(home.path())
        .stdin(Stdio::from(slave.try_clone().unwrap()))
        .stdout(Stdio::from(slave.try_clone().unwrap()))
        .stderr(Stdio::from(slave));
    // SAFETY: the child only performs async-signal-safe terminal setup before exec.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(command);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut tail = Vec::new();
    let mut x_count = 0;
    let stale = vec![b'y'; 1024];
    let mut sent = [false; 6];
    let actions: [(&[u8], &[u8]); 6] = [
        (b"READY_INPUT", b"hello\n"),
        (b"READY_INT", b"\x03"),
        (b"READY_STOP", b"\x1a"),
        (b"READY_RESIZE", b"\n"),
        (b"READY_FLUSH", &stale),
        (b"READY_FRESH", b"n"),
    ];
    loop {
        if Instant::now() >= deadline {
            let _ = master.write_all(b"\x03");
            let _ = child.kill();
            let _ = child.wait();
            panic!("PTY probe timed out: {}", String::from_utf8_lossy(&tail));
        }
        let mut descriptor = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
        if ready > 0 {
            let mut bytes = [0; 16384];
            match master.read(&mut bytes) {
                Ok(0) => break,
                Ok(n) => {
                    x_count += bytes[..n].iter().filter(|b| **b == b'X').count();
                    tail.extend_from_slice(&bytes[..n]);
                    if tail.len() > 32 * 1024 {
                        tail.drain(..tail.len() - 32 * 1024);
                    }
                    for (i, (marker, keys)) in actions.iter().enumerate() {
                        if matches!(case, "input-signals" | "flush")
                            && !sent[i]
                            && tail.windows(marker.len()).any(|w| w == *marker)
                        {
                            if i == 3 {
                                let size = libc::winsize {
                                    ws_row: 35,
                                    ws_col: 111,
                                    ws_xpixel: 0,
                                    ws_ypixel: 0,
                                };
                                assert_eq!(
                                    unsafe {
                                        libc::ioctl(master.as_raw_fd(), libc::TIOCSWINSZ, &size)
                                    },
                                    0
                                );
                            }
                            master.write_all(keys).unwrap();
                            sent[i] = true;
                        }
                    }
                }
                Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => panic!("{error}"),
            }
        }
        if ready == 0
            && let Some(status) = child.try_wait().unwrap()
        {
            assert!(
                status.success(),
                "{status}: {}",
                String::from_utf8_lossy(&tail)
            );
            break;
        }
    }
    let status = child.wait().unwrap();
    assert!(
        status.success(),
        "{status}: {}",
        String::from_utf8_lossy(&tail)
    );
    (tail, x_count)
}

#[test]
fn session_pty_preserves_boundaries_state_and_redirections() {
    let (terminal, _) = run_probe("boundaries");
    let raw = b"\x1b[?1049h\x1b[31mraw\xff\r\r\n\x1b[?1049l";
    assert!(terminal.windows(raw.len()).any(|window| window == raw));
}

#[test]
fn session_pty_preserves_input_signals_jobs_and_resize() {
    run_probe("input-signals");
}

#[test]
fn session_pty_forwards_100_mib_with_a_bounded_tail() {
    let (_, x_count) = run_probe("large");
    assert_eq!(x_count, 100 * 1024 * 1024);
}

#[test]
fn captured_failures_are_not_rerun_or_assigned_to_new_commands() {
    run_probe("evidence");
}

#[test]
fn approval_flush_discards_relay_typeahead() {
    run_probe("flush");
}
