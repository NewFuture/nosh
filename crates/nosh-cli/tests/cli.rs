//! Non-interactive modes must behave exactly like bash (no model, no extra output).

use std::io::Write;
use std::process::{Command, Stdio};

fn nosh() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nosh"));
    c.env("NOSH_HOME", std::env::temp_dir().join("nosh-cli-test-home"));
    c
}

#[test]
fn dash_c_prints_exactly() {
    let out = nosh().args(["-c", "echo hi"]).output().unwrap();
    assert_eq!(out.stdout, b"hi\n");
    assert!(
        out.stderr.is_empty(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success());
}

#[test]
fn user_command_output_is_never_restyled_or_transcoded() {
    let script = "printf '\\033[31m\u{4e2d}\u{6587} \u{1f469}\u{200d}\u{1f4bb}\\033[0m\\r\\n\\377'";
    let mut expected = "\x1b[31m\u{4e2d}\u{6587} \u{1f469}\u{200d}\u{1f4bb}\x1b[0m\r\n"
        .as_bytes()
        .to_vec();
    expected.push(0xff);
    for term in ["xterm-256color", "screen", "dumb", ""] {
        let out = nosh()
            .env("TERM", term)
            .env("LC_ALL", "C")
            .env("NO_COLOR", "1")
            .args(["-c", script])
            .output()
            .unwrap();
        assert!(out.status.success(), "{term}: {:?}", out.stderr);
        assert_eq!(out.stdout, expected, "{term}");
        assert!(out.stderr.is_empty());
    }
}

#[test]
fn dash_c_exit_status_and_args() {
    let out = nosh().args(["-c", "exit 3"]).output().unwrap();
    assert_eq!(out.status.code(), Some(3));
    let out = nosh()
        .args(["-c", "echo \"$0|$1|$2\"", "name", "a b", "c"])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "name|a b|c\n");
    // `#` is a comment outside the interactive shell.
    let out = nosh()
        .args(["-c", "# find big files\necho ok"])
        .output()
        .unwrap();
    assert_eq!(out.stdout, b"ok\n");
    assert!(out.stderr.is_empty());
}

#[test]
fn script_file_with_args() {
    let dir = std::env::temp_dir().join(format!("nosh-cli-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let script = dir.join("s.sh");
    std::fs::write(&script, "f() { echo \"f:$1\"; }\nf \"$2\"\nexit 5\n").unwrap();
    let out = nosh().arg(&script).args(["x", "y"]).output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "f:y\n");
    assert_eq!(out.status.code(), Some(5));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn inference_thread_settings_stay_out_of_the_shell() {
    // nosh sets these for its own threads when it starts; the shell and the
    // programs it runs see the user's values.
    let show = r#"echo "${CANDLE_NUM_THREADS-unset} ${RAYON_NUM_THREADS-unset}""#;
    let out = nosh()
        .env_remove("CANDLE_NUM_THREADS")
        .env("RAYON_NUM_THREADS", "7")
        .args(["-c", &format!("{show}; sh -c '{show}'")])
        .output()
        .unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "unset 7\nunset 7\n");
    assert!(out.status.success());
}

#[test]
fn commands_from_stdin() {
    let mut child = nosh()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"read x\nhello\necho \"got:$x\"\nfor i in 1 2; do\necho $i\ndone\n")
        .unwrap();
    let out = child.wait_with_output().unwrap();
    assert_eq!(String::from_utf8_lossy(&out.stdout), "got:hello\n1\n2\n");
}

fn empty_home(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nosh-cli-empty-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn agent_modes_without_a_model_exit_2() {
    let home = empty_home("a");
    let out = nosh()
        .env("NOSH_HOME", &home)
        .args(["--offline", "-s", "list files"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(out.stdout.is_empty(), "stdout carries only a command");
    let out = nosh()
        .env("NOSH_HOME", &home)
        .args(["--no-download", "-a", "what time is it"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = nosh()
        .env("NOSH_HOME", &home)
        .arg("-a")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "usage error");
    let _ = std::fs::remove_dir_all(home);
}

#[test]
fn doctor_reports_missing_model() {
    let home = empty_home("doctor");
    let out = nosh()
        .env("NOSH_HOME", &home)
        .args(["--offline", "doctor"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("model"), "{err}");
    let _ = std::fs::remove_dir_all(home);
}
