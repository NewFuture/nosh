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
