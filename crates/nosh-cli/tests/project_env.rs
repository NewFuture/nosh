#![cfg(unix)]

use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

fn fixture() -> tempfile::TempDir {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(home.path().join("config.toml"), "[shell]\nproject_env='direnv'\nterminal_integration='on'\ncapture_output='off'\ninput_assist=false\ncompletion=false\nstatus_bar=false\ncommand_assist=false\n").unwrap();
    std::fs::create_dir(home.path().join("bin")).unwrap();
    let tool = home.path().join("bin/direnv");
    std::fs::write(
        &tool,
        "#!/bin/sh\nprintf x >> \"$HOME/environment-invoked\"\nprintf '{}'\nexit 7\n",
    )
    .unwrap();
    std::fs::set_permissions(tool, std::fs::Permissions::from_mode(0o700)).unwrap();
    home
}

fn command(home: &std::path::Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .current_dir(home)
        .env("HOME", home)
        .env("NOSH_HOME", home)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", home.join("bin").display()),
        )
        .env("TERM", "dumb")
        .env("TERM_PROGRAM", "WezTerm")
        .env("NOSH_LANG", "en")
        .env_remove("NOSH_DISABLE_AI")
        .env_remove("NOSH_MODEL_PATH")
        .env_remove("NOSH_EVAL_TRACE")
        .env_remove("NOSH_STATS")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn interactive(home: &std::path::Path, safe: bool) -> Vec<u8> {
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 100,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
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
    let (mut master, slave) = unsafe {
        (
            std::fs::File::from_raw_fd(master),
            std::fs::File::from_raw_fd(slave),
        )
    };
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
        assert_ne!(
            libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
            -1
        );
    }
    let mut launch = command(home);
    launch
        .args([if safe { "--safe" } else { "--norc" }, "-i"])
        .env("NOSH_DISABLE_AI", "1")
        .stdin(slave.try_clone().unwrap())
        .stdout(slave.try_clone().unwrap())
        .stderr(slave.try_clone().unwrap());
    unsafe {
        launch.pre_exec(|| {
            if libc::setsid() < 0 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = launch.spawn().unwrap();
    drop(launch);
    drop(slave);
    let mut output = Vec::new();
    let mut sent = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let mut bytes = [0; 8192];
        match master.read(&mut bytes) {
            Ok(n) => {
                output.extend_from_slice(&bytes[..n]);
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) || error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("{error}"),
        }
        let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
        if !sent
            && unsafe { libc::tcgetattr(master.as_raw_fd(), &raw mut attributes) } == 0
            && attributes.c_lflag & libc::ICANON == 0
        {
            master
                .write_all(b"printf 'MANUAL-WORKED\\n'\rexit 0\r")
                .unwrap();
            sent = true;
        }
        if let Some(status) = child.try_wait().unwrap() {
            assert!(
                status.success(),
                "{status}: {:?}",
                String::from_utf8_lossy(&output)
            );
            break;
        }
        if Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!(
                "interactive environment test timed out: {:?}",
                String::from_utf8_lossy(&output)
            );
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    output
}

#[test]
fn plain_commands_scripts_and_stdin_never_initialize_project_environment() {
    let home = fixture();
    let script = home.path().join("script.sh");
    std::fs::write(&script, "printf raw").unwrap();
    for args in [vec!["-c", "printf raw"], vec![script.to_str().unwrap()]] {
        let output = command(home.path()).args(args).output().unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout, b"raw");
        assert!(output.stderr.is_empty());
    }
    let mut child = command(home.path()).stdin(Stdio::piped()).spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"printf raw\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.stdout, b"raw");
    assert!(output.stderr.is_empty());
    assert!(!home.path().join("environment-invoked").exists());
}

#[test]
fn safe_skips_project_code_but_norc_keeps_explicit_environment_selection() {
    for safe in [true, false] {
        let home = fixture();
        let output = interactive(home.path(), safe);
        assert!(String::from_utf8_lossy(&output).contains("MANUAL-WORKED"));
        assert_eq!(home.path().join("environment-invoked").exists(), !safe);
        assert!(!String::from_utf8_lossy(&output).contains("\x1b]133;"));
        assert!(!String::from_utf8_lossy(&output).contains("\x1b]7;"));
    }
}

#[test]
fn one_shot_agent_initializes_before_model_loading_and_preserves_json_stdout() {
    let home = fixture();
    let output = command(home.path())
        .args([
            "--no-download",
            "--json",
            "-a",
            "do not execute after environment failure",
        ])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(home.path().join("environment-invoked").exists());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("project environment is not ready"));
}

#[test]
fn suggest_and_pipe_attachment_do_not_run_environment_hooks() {
    for suggest in [true, false] {
        let home = fixture();
        let missing = home.path().join("missing.gguf");
        let mut launch = command(home.path());
        launch
            .args(["--no-download", "--model-path"])
            .arg(missing)
            .args([if suggest { "-s" } else { "-a" }, "describe this"]);
        let output = if suggest {
            launch.output().unwrap()
        } else {
            let mut child = launch.stdin(Stdio::piped()).spawn().unwrap();
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"read-only attachment")
                .unwrap();
            child.wait_with_output().unwrap()
        };
        assert!(!output.status.success());
        assert!(!home.path().join("environment-invoked").exists());
        assert!(!String::from_utf8_lossy(&output.stdout).contains("\x1b]"));
    }
}
