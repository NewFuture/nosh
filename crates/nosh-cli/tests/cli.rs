//! Non-interactive modes must behave exactly like bash (no model, no extra output).

use std::io::Write;
use std::process::{Command, Stdio};

fn nosh() -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_nosh"));
    c.env("NOSH_HOME", std::env::temp_dir().join("nosh-cli-test-home"));
    c.env_remove("NOSH_MODEL");
    c.env_remove("NOSH_MODEL_PATH");
    c
}

#[test]
fn fixtures_do_not_inherit_a_real_model_path() {
    assert!(
        nosh()
            .get_envs()
            .any(|(name, value)| name == "NOSH_MODEL_PATH" && value.is_none())
    );
    assert!(
        nosh()
            .get_envs()
            .any(|(name, value)| name == "NOSH_MODEL" && value.is_none())
    );
}

#[test]
fn model_management_uses_memory_defaults_but_keeps_explicit_ids() {
    let home = empty_home("model-defaults");
    std::fs::write(
        home.join("config.toml"),
        "[model]\nid = 'minicpm5-1b:q4_k_m'\npath = 'must-not-use.gguf'\n",
    )
    .unwrap();
    let output = nosh()
        .env("NOSH_HOME", &home)
        .env("NOSH_MODEL", "minicpm5-1b:q4_k_m")
        .env("NOSH_MODEL_PATH", "must-not-use-env.gguf")
        .args(["--offline", "model", "path"])
        .output()
        .unwrap();
    let path = String::from_utf8(output.stdout).unwrap();
    if output.status.success() {
        assert!(
            path.trim_end().ends_with("minicpm5-2b-q8_0")
                || path.trim_end().ends_with("minicpm5-2b-q4_k_m"),
            "{path}"
        );
    } else {
        assert_eq!(output.status.code(), Some(1));
        assert!(path.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("3.5 GiB"),
            "{:?}",
            output.stderr
        );
    }
    for (id, directory) in [
        ("minicpm5-2b:q4_k_m", "minicpm5-2b-q4_k_m"),
        ("minicpm5-2b:q8_0", "minicpm5-2b-q8_0"),
        ("minicpm5-1b:q4_k_m", "minicpm5-1b-q4_k_m"),
        ("minicpm5-2b", "minicpm5-2b-q8_0"),
    ] {
        let output = nosh()
            .env("NOSH_HOME", &home)
            .args(["--offline", "model", "path", id])
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output.stderr);
        let path = String::from_utf8(output.stdout).unwrap();
        assert!(path.trim_end().ends_with(directory), "{id}: {path}");
    }
    let output = nosh()
        .env("NOSH_HOME", &home)
        .args(["--offline", "model", "list"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let list = String::from_utf8(output.stdout).unwrap();
    let marked: Vec<_> = list.lines().filter(|line| line.contains('*')).collect();
    match marked.as_slice() {
        [] => assert!(
            String::from_utf8_lossy(&output.stderr).contains("3.5 GiB"),
            "{:?}",
            output.stderr
        ),
        [line] => assert!(
            line.starts_with("minicpm5-2b:q8_0") || line.starts_with("minicpm5-2b:q4_k_m"),
            "{list}"
        ),
        _ => panic!("multiple automatic defaults: {list}"),
    }
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn an_explicit_missing_model_path_does_not_trigger_automatic_selection_or_download() {
    let home = empty_home("explicit-model-path");
    let missing = home.join("must-not-download-instead.gguf");
    let output = nosh()
        .env("NOSH_HOME", &home)
        .env("NOSH_LANG", "en")
        .args([
            "--offline",
            "--no-download",
            "--device",
            "cpu",
            "--model-path",
        ])
        .arg(&missing)
        .args(["-s", "print hello"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("does not exist"), "{stderr}");
    assert!(!stderr.contains("downloading"), "{stderr}");
    assert!(!stderr.contains("cannot read available RAM"), "{stderr}");
    std::fs::remove_dir_all(home).unwrap();
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

#[test]
fn capture_configuration_preserves_noninteractive_and_unavailable_paths() {
    let home = empty_home("capture");
    std::fs::write(
        home.join("config.toml"),
        "[shell]\ncapture_output = \"last\"\n",
    )
    .unwrap();
    let out = nosh()
        .env("NOSH_HOME", &home)
        .args(["-c", "printf raw"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"raw");
    assert!(out.stderr.is_empty());
    let out = nosh()
        .env("NOSH_HOME", &home)
        .env("TERM", "dumb")
        .args(["--safe", "-i"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("continuing without capture"));
    std::fs::write(
        home.join("config.toml"),
        "[shell]\ncapture_output = \"off\"\n",
    )
    .unwrap();
    let direct = nosh()
        .env("NOSH_HOME", &home)
        .env("TERM", "dumb")
        .args(["--safe", "-i"])
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), direct.status.code());
    assert_eq!(out.stdout, direct.stdout);
    std::fs::remove_dir_all(home).unwrap();
}

#[test]
fn a_plain_environment_value_cannot_authorize_an_internal_pty_host() {
    let output = nosh()
        .env("NOSH_INTERNAL_PTY_FD", "1")
        .args(["-c", "printf must-not-run"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("PTY host bootstrap"));
}

fn empty_home(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nosh-cli-empty-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
#[cfg(not(feature = "cuda"))]
fn cpu_build_reports_device_and_rejects_cuda_without_fallback() {
    let home = empty_home("device");
    let doctor = || {
        let mut command = nosh();
        command.env("NOSH_HOME", &home);
        command.args(["--offline", "doctor"]);
        command
    };
    let out = doctor().output().unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("auto · CUDA compiled: false"), "{stderr}");
    std::fs::write(home.join("config.toml"), "[model]\ndevice = 'cuda'\n").unwrap();
    let out = doctor().output().unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("CUDA is not enabled"));
    let out = doctor().args(["--device", "cpu"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("cpu · CUDA compiled: false"));
    let out = nosh()
        .env("NOSH_HOME", &home)
        .args(["-c", "printf raw"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(out.stdout, b"raw");
    assert!(
        out.stderr.is_empty(),
        "non-AI commands must not initialize CUDA"
    );
    std::fs::remove_dir_all(home).unwrap();
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
fn agent_and_suggest_report_stdin_read_errors_before_loading_a_model() {
    let home = empty_home("stdin-error");
    for args in [
        &["--offline", "-a", "summarize the input"][..],
        &["--offline", "-s"][..],
    ] {
        let output = nosh()
            .env("NOSH_HOME", &home)
            .args(args)
            .stdin(Stdio::from(std::fs::File::open(&home).unwrap()))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        assert!(
            String::from_utf8_lossy(&output.stderr).starts_with("nosh: cannot read stdin: "),
            "{:?}",
            output.stderr
        );
    }
    let output = nosh()
        .env("NOSH_HOME", &home)
        .args(["--offline", "-s", "list files"])
        .stdin(Stdio::from(std::fs::File::open(&home).unwrap()))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "no model is installed");
    assert!(output.stdout.is_empty());
    let error = String::from_utf8_lossy(&output.stderr);
    assert!(error.contains("model"), "{error}");
    assert!(!error.contains("cannot read stdin"), "{error}");
    std::fs::remove_dir_all(home).unwrap();
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
