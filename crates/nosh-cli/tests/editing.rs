#![cfg(unix)]

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .env("NOSH_HOME", home)
        .env("HOME", home)
        .env("NOSH_LANG", "en")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .env_remove("VISUAL")
        .env_remove("EDITOR");
    command
}

fn interactive(mut launch: Command, steps: &[(&str, &[u8])]) -> String {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 120,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: all output pointers are valid; optional name/termios are unused.
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
    // SAFETY: openpty returned two distinct owned descriptors.
    let (mut master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    // SAFETY: these descriptors are open for all calls below.
    unsafe {
        assert_ne!(
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC),
            -1
        );
        assert_ne!(
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC),
            -1
        );
        let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
        assert_ne!(flags, -1);
        assert_ne!(
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
            -1
        );
    }
    launch
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    // SAFETY: the pre-exec hook performs only async-signal-safe terminal syscalls.
    unsafe {
        launch.pre_exec(|| {
            if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = launch.spawn().unwrap();
    drop(launch);
    drop(slave);
    let mut output = Vec::new();
    let mut chunk = [0; 8192];
    let mut step = 0;
    let mut observed = 0;
    let mut replies = 0;
    let deadline = Instant::now() + Duration::from_secs(10);
    let status = loop {
        match master.read(&mut chunk) {
            Ok(n) if n > 0 => {
                output.extend_from_slice(&chunk[..n]);
                let queries = output
                    .windows(4)
                    .filter(|bytes| *bytes == b"\x1b[6n")
                    .count();
                for _ in replies..queries {
                    master.write_all(b"\x1b[1;1R").unwrap();
                }
                replies = queries;
            }
            Ok(_) => {}
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    || error.kind() == std::io::ErrorKind::Interrupted
                    || error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("PTY read: {error}"),
        }
        if let Some((needle, bytes)) = steps.get(step)
            && String::from_utf8_lossy(&output[observed..]).contains(needle)
        {
            master.write_all(bytes).unwrap();
            observed = output.len();
            step += 1;
        }
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "editing PTY timed out at step {step}: {:?}",
                String::from_utf8_lossy(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    assert!(status.success(), "{:?}", String::from_utf8_lossy(&output));
    assert_eq!(step, steps.len(), "{:?}", String::from_utf8_lossy(&output));
    String::from_utf8(output).unwrap()
}

#[test]
fn file_configuration_reaches_the_real_vi_editor_and_remapped_undo() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        r#"
[shell]
edit_mode = "vi"
capture_output = "off"
input_assist = false
status_bar = false
command_assist = false
[shell.keybindings]
undo = ["F4"]
"#,
    )
    .unwrap();
    let mut launch = command(home.path());
    launch.args(["--safe", "-i"]);
    let output = interactive(
        launch,
        &[
            ("[I]", "printf '%s\\n' CLI_CONFIG_\u{4e2d}a".as_bytes()),
            ("CLI_CONFIG_\u{4e2d}a", b"\x1b"),
            ("[N]", b"x"),
            ("[N]", b"\x1bOS"),
            ("[N]", b"\r"),
            ("CLI_CONFIG_\u{4e2d}a", b"exit 0\r"),
        ],
    );
    assert!(output.contains("CLI_CONFIG_\u{4e2d}a"));
}

#[test]
fn inline_prefix_configuration_reaches_interactive_commands_without_a_model() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[shell]\nai_prefix = '?'\ncapture_output = 'off'\ninput_assist = false\n\
         status_bar = false\ncommand_assist = false\ncompletion = false\n",
    )
    .unwrap();
    let mut launch = command(home.path());
    launch
        .env("PS1", "prefix-test> ")
        .env_remove("NOSH_DISABLE_AI")
        .args(["--offline", "--no-download", "--norc", "-i"]);
    let output = interactive(
        launch,
        &[
            ("prefix-test> ", b"?\r"),
            ("mode [confirm|auto|yolo]", b"?mode confirm\r"),
            ("Confirm", b"?mode bad\r"),
            ("invalid arguments; usage: ?mode", b"?mode\r"),
            ("Confirm", b"?think on\r"),
            ("think: on", b"?status\r"),
            ("not loaded (loads on first use)", b"?out invalid\r"),
            ("invalid arguments; usage: ?out", b"?unknown\r"),
            (
                "unknown command ?unknown",
                b"ai() { printf 'ORDINARY_AI:%s\\n' \"$*\"; }; ai mode auto\r",
            ),
            ("ORDINARY_AI:mode auto", b"exit 0\r"),
        ],
    );
    assert!(
        output.contains("?help") && output.contains("?fix"),
        "{output}"
    );
    assert!(
        !output.contains("not installed") && !output.contains("Downloading"),
        "{output}"
    );
}

#[test]
fn invalid_whitespace_prefix_warns_and_keeps_default_commands_available() {
    for prefix in [" ?", "\u{3000}?"] {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            format!(
                "[shell]\nai_prefix = {}\ncapture_output = 'off'\ninput_assist = false\n\
                 status_bar = false\ncommand_assist = false\ncompletion = false\n",
                serde_json::to_string(prefix).unwrap(),
            ),
        )
        .unwrap();
        let mut launch = command(home.path());
        launch
            .env("PS1", "prefix-validation> ")
            .env_remove("NOSH_DISABLE_AI")
            .args(["--offline", "--no-download", "--norc", "-i"]);
        let output = interactive(
            launch,
            &[
                ("prefix-validation> ", b"#help\r"),
                ("mode [confirm|auto|yolo]", b"#mode confirm\r"),
                ("Confirm", b"exit 0\r"),
            ],
        );
        assert!(
            output.contains("shell.ai_prefix: must not start with whitespace"),
            "{output}"
        );
        assert!(
            output.contains("#help") && output.contains("#fix"),
            "{output}"
        );
        assert!(
            !output.contains("not installed") && !output.contains("Downloading"),
            "{output}"
        );
    }
}

#[test]
fn invalid_editing_configuration_does_not_change_noninteractive_execution() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        r#"
[shell]
edit_mode = "invalid"
[shell.keybindings]
undo = ["F2"]
redo = ["F2"]
"#,
    )
    .unwrap();
    let script = home.path().join("script.sh");
    std::fs::write(&script, "printf '%s\\n' \"$1\"\nexit 7\n").unwrap();
    for mut command in [
        {
            let mut command = command(home.path());
            command.args(["-c", "printf raw; exit 7"]);
            command
        },
        {
            let mut command = command(home.path());
            command.arg(&script).arg("raw");
            command
        },
    ] {
        let output = command.stdin(Stdio::null()).output().unwrap();
        assert_eq!(output.status.code(), Some(7));
        assert!(output.stdout.starts_with(b"raw"));
        assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    }
}

#[test]
fn disabled_ai_leaves_hash_commands_as_shell_comments() {
    for safe in [false, true] {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join("config.toml"),
            "[shell]\ncapture_output = 'off'\ninput_assist = false\n\
                 status_bar = false\ncommand_assist = false\ncompletion = false\n",
        )
        .unwrap();
        let mut launch = command(home.path());
        launch
            .env("PS1", "disabled-prefix> ")
            .args(["--norc", "-i"]);
        if safe {
            launch.env_remove("NOSH_DISABLE_AI").arg("--safe");
        } else {
            launch.env("NOSH_DISABLE_AI", "1");
        }
        let output = interactive(
            launch,
            &[
                (
                    "disabled-prefix> ",
                    b"\x1b[200~#help\n#mode yolo\nprintf 'STILL_SHELL\\n'\x1b[201~\r",
                ),
                ("STILL_SHELL\r\n", b"exit 0\r"),
            ],
        );
        assert!(!output.contains("mode [confirm|auto|yolo]"), "{output}");
        assert!(!output.contains("YOLO:"), "{output}");
        assert!(output.contains("STILL_SHELL\r\n"), "{output}");
    }
}
