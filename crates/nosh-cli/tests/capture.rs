//! Exercise configuration and re-exec through the actual CLI, without a model.

#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::time::{Duration, Instant};

fn probe(mode: &str, unavailable: bool) {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        if mode == "default" {
            String::new()
        } else {
            format!("[shell]\ncapture_output = \"{mode}\"\n")
        },
    )
    .unwrap();
    std::fs::write(home.path().join(".bashrc"),
        "n=$(cat \"$HOME/rc-count\" 2>/dev/null || printf 0)\nprintf '%s' \"$((n+1))\" > \"$HOME/rc-count\"\nPS1='CAPTURE_PROMPT> '\n",
    ).unwrap();
    let (mut master, mut slave) = (-1, -1);
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: all output pointers are valid; no terminal name is requested.
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .env("HOME", home.path())
        .env("NOSH_HOME", home.path())
        .env("NOSH_DISABLE_AI", "1")
        .env("TERM", "dumb")
        .env("LC_ALL", "C.UTF-8")
        .env("PS1", "CAPTURE_PROMPT> ")
        .env("RAYON_NUM_THREADS", "7")
        .env_remove("NOSH_INTERNAL_PTY_FD")
        .env_remove("CANDLE_NUM_THREADS")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave);
    // SAFETY: child setup consists only of async-signal-safe syscalls.
    unsafe {
        command.pre_exec(move || {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if unavailable {
                let limit = libc::rlimit {
                    rlim_cur: 64,
                    rlim_max: 64,
                };
                if libc::setrlimit(libc::RLIMIT_NOFILE, &limit) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    drop(command);
    let pid = child.id();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut output = Vec::new();
    let mut submitted = false;
    let mut shell_pid = None;
    let mut exit_sent = false;
    loop {
        if Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!(
                "CLI capture timeout ({mode}, unavailable={unavailable}): {}",
                String::from_utf8_lossy(&output[output.len().saturating_sub(4096)..])
            );
        }
        let mut descriptor = libc::pollfd {
            fd: master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        if unsafe { libc::poll(&mut descriptor, 1, 100) } <= 0 {
            continue;
        }
        let mut bytes = [0; 4096];
        match master.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&bytes[..n]),
            Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
            Err(error) => panic!("{error}"),
        }
        assert!(output.len() < 64 * 1024, "unexpected CLI output volume");
        let text = String::from_utf8_lossy(&output);
        if !submitted && text.contains("CAPTURE_PROMPT> ") {
            master.write_all(b"printf 'CAPTURE_PID=%s INTERNAL=%s THREADS=%s\\n' \"$$\" \"${NOSH_INTERNAL_PTY_FD-unset}\" \"$RAYON_NUM_THREADS\"\r").unwrap();
            submitted = true;
        }
        if shell_pid.is_none() {
            for line in text.lines() {
                if let Some(rest) = line.strip_prefix("CAPTURE_PID=")
                    && rest.ends_with(" INTERNAL=unset THREADS=7")
                {
                    shell_pid = Some(
                        rest.split_whitespace()
                            .next()
                            .unwrap()
                            .parse::<u32>()
                            .unwrap(),
                    );
                    break;
                }
            }
        }
        if let Some(pid) = shell_pid
            && !exit_sent
            && text
                .split_once(&format!("CAPTURE_PID={pid} INTERNAL=unset THREADS=7"))
                .is_some_and(|(_, after)| after.contains("CAPTURE_PROMPT> "))
        {
            master.write_all(b"exit 0\r").unwrap();
            exit_sent = true;
        }
    }
    let status = child.wait().unwrap();
    let text = String::from_utf8_lossy(&output);
    assert!(status.success(), "{status}: {text}");
    let shell_pid = shell_pid.unwrap_or_else(|| panic!("missing shell identity: {text}"));
    assert_eq!(shell_pid != pid, mode != "off" && !unavailable, "{text}");
    assert_eq!(
        text.contains("continuing without capture"),
        unavailable,
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(home.path().join("rc-count")).unwrap(),
        "1"
    );
    let mut attrs = std::mem::MaybeUninit::<libc::termios>::zeroed();
    assert_eq!(
        unsafe { libc::tcgetattr(master.as_raw_fd(), attrs.as_mut_ptr()) },
        0
    );
    assert_ne!(unsafe { attrs.assume_init() }.c_lflag & libc::ICANON, 0);
}

#[test]
fn capture_setting_selects_the_session_relay_and_preserves_rc_and_environment() {
    probe("off", false);
    probe("last", false);
    probe("default", false);
    probe("default", true);
}
