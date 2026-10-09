use super::support::*;

#[test]
fn ask_user_supports_selection_free_text_and_preserves_piped_stdin() {
    for terminal in ["xterm-256color", "dumb"] {
        for (keys, expected) in [
            (b"\x1b[B\x1b[B\r".as_slice(), "zip"),
            (b"7z\r".as_slice(), "7z"),
            (b"1\r".as_slice(), "1"),
            (b"\rcustom\r".as_slice(), "custom"),
        ] {
            let steps: &[KeyStep<'_>] = &[("answer> ", keys)];
            let (out, err) = Probe {
                mode: "ask-user-pipe",
                terminal: Some(terminal),
                stdin_pipe: true,
                stderr_tty: true,
                steps,
                ..Default::default()
            }
            .run();
            let value: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
            assert_eq!(value["answer"], expected, "{err}");
            assert_eq!(value["attachment"], "attachment\n");
            assert!(value["error"].is_null());
            assert!(err.contains("Archive format?"));
        }
    }
}

#[test]
fn ask_user_cancellation_eof_and_missing_terminal_are_not_answers() {
    for keys in [b"\x03".as_slice(), b"\x04".as_slice(), b"\x1b".as_slice()] {
        let (out, _) = Probe {
            mode: "ask-user",
            stderr_tty: true,
            steps: &[("answer> ", keys)],
            ..Default::default()
        }
        .run();
        let value: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(value["answer"].is_null());
        assert_eq!(value["error"], "user input cancelled");
    }
    for mode in ["ask-user", "ask-user-cancel"] {
        let (out, _) = Probe {
            mode,
            stderr_tty: mode == "ask-user-cancel",
            ..Default::default()
        }
        .run();
        let value: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(value["answer"].is_null());
        assert!(value["error"].is_string());
    }
}

#[test]
fn f2_returns_to_the_editor_without_dialogue_or_execution() {
    Probe {
        mode: "repl-generate",
        stdout_tty: true,
        stderr_tty: true,
        input_assist: false,
        steps: &[
            ("probe> ", b"create a file\x1bOQ"),
            ("touch accepted", b"\x15exit\r"),
        ],
        ..Default::default()
    }
    .run();
}

#[test]
fn automatic_assistance_is_displayed_and_accepting_never_executes_without_enter() {
    for terminal in ["xterm-256color", "dumb"] {
        for execute in [false, true] {
            let finish: &[u8] = if execute { b"\rexit\r" } else { b"\x15exit\r" };
            let steps: &[KeyStep<'_>] = &[
                ("probe> ", b"true\r"),
                ("next: touch accepted", b"\x1bOQ"),
                ("touch accepted", finish),
            ];
            Probe {
                mode: "repl-assist",
                terminal: Some(terminal),
                stdout_tty: true,
                stderr_tty: true,
                input_assist: false,
                initial: if execute { "execute" } else { "skip" },
                steps,
                ..Default::default()
            }
            .run();
        }
    }
}
