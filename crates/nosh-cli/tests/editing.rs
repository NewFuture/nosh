#![cfg(unix)]

#[path = "support/terminal.rs"]
mod terminal;

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use terminal::Terminal;

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

fn interactive(launch: Command, steps: &[(&str, &[u8])]) -> String {
    let mut terminal = Terminal::new(launch, 24, 120);
    for (needle, bytes) in steps {
        terminal.wait_for(needle);
        terminal.send(bytes);
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = terminal.poll() {
            assert!(
                status.success(),
                "{:?}",
                String::from_utf8_lossy(&terminal.output)
            );
            return String::from_utf8(terminal.output.clone()).unwrap();
        }
        assert!(Instant::now() < deadline, "editing PTY did not exit");
    }
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
            ("not loaded (loads on first use)", b"?assist off\r"),
            ("assist: off", b"?status\r"),
            ("assist: off", b"?assist on\r"),
            ("assist: on", b"?auto off\r"),
            ("unknown command ?auto", b"?out invalid\r"),
            ("unknown command ?out", b"?unknown\r"),
            (
                "unknown command ?unknown",
                b"ai() { printf 'ORDINARY_AI:%s\\n' \"$*\"; }; ai mode auto\r",
            ),
            ("ORDINARY_AI:mode auto", b"exit 0\r"),
        ],
    );
    assert!(
        output.contains("?help") && output.contains("?fix") && output.contains("?assist"),
        "{output}"
    );
    assert!(
        !output.contains("not installed") && !output.contains("Downloading"),
        "{output}"
    );
}

#[test]
fn foreground_loading_failure_preserves_the_draft_without_a_no_suggestion_fallback() {
    let home = tempfile::tempdir().unwrap();
    std::fs::write(
        home.path().join("config.toml"),
        "[shell]\ncapture_output = 'off'\ninput_assist = false\n\
         status_bar = false\ncommand_assist = false\ncompletion = false\n",
    )
    .unwrap();
    let mut launch = command(home.path());
    launch
        .env("PS1", "load-result> ")
        .env_remove("NOSH_DISABLE_AI")
        .args(["--offline", "--no-download", "--norc", "-i", "--model-path"])
        .arg(home.path().join("missing.gguf"));
    let output = interactive(
        launch,
        &[
            ("load-result> ", b"describe a task\x1bOQ"),
            ("does not exist", b"\x15#status\r"),
            ("model: load failed:", b"exit 0\r"),
        ],
    );
    assert!(output.contains("describe a task"), "{output}");
    assert!(!output.contains("no suggestion"), "{output}");
    assert!(
        !output.contains("not loaded (loads on first use)"),
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
