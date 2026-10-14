#![cfg(unix)]

#[path = "support/terminal.rs"]
mod terminal;

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use terminal::Terminal;

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
    let mut launch = command(home);
    launch
        .args([if safe { "--safe" } else { "--norc" }, "-i"])
        .env("NOSH_DISABLE_AI", "1");
    let mut terminal = Terminal::new(launch, 24, 100);
    let mut sent = false;
    let deadline = Instant::now() + Duration::from_secs(8);
    loop {
        let status = terminal.poll();
        let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
        if !sent
            && unsafe { libc::tcgetattr(terminal.as_raw_fd(), &raw mut attributes) } == 0
            && attributes.c_lflag & libc::ICANON == 0
        {
            terminal.send(b"printf 'MANUAL-WORKED\\n'\rexit 0\r");
            terminal.wait_for("MANUAL-WORKED\r\n");
            sent = true;
        }
        if let Some(status) = status {
            assert!(
                status.success(),
                "{status}: {:?}",
                String::from_utf8_lossy(&terminal.output)
            );
            return std::mem::take(&mut terminal.output);
        }
        assert!(
            Instant::now() < deadline,
            "interactive environment test timed out: {:?}",
            String::from_utf8_lossy(&terminal.output)
        );
    }
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
