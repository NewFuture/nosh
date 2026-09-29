use super::support::*;

#[test]
fn input_assist_updates_without_another_key_and_keeps_unicode_input_unchanged() {
    for no_color in ["", "1"] {
        let initial = "printf '%s\\n' '中文e\u{301}👩\u{200d}💻";
        let steps: &[KeyStep<'_>] = &[("Incomplete:", b"'\rexit 0\r")];
        let (out, err) = Probe {
            mode: "repl",
            stdout_tty: true,
            stderr_tty: true,
            keys: Some(initial.as_bytes()),
            steps,
            no_color,
            ..Default::default()
        }
        .run();
        assert_eq!(style::strip_ansi(&out).trim(), "中文e\u{301}👩\u{200d}💻");
        assert!(err.contains("Incomplete:"), "{err:?}");
        if no_color == "1" {
            for color in ["\x1b[31m", "\x1b[32m", "\x1b[33m", "\x1b[35m"] {
                assert!(!err.contains(color), "{err:?}");
            }
        }
    }
}

#[test]
fn input_assist_blocked_worker_does_not_delay_edit_cancel_or_exit() {
    let steps: &[KeyStep<'_>] = &[
        ("@worker-blocked", b"printf '%s\\n' EDITx"),
        ("EDITx", b"\x7f\r"),
        ("EDIT\r\n", b"\x03"),
        ("probe> ", b"\x04"),
    ];
    let (out, _) = Probe {
        mode: "repl-blocked",
        stdout_tty: true,
        stderr_tty: true,
        no_color: "1",
        steps,
        ..Default::default()
    }
    .run();
    assert_eq!(style::strip_ansi(&out).trim(), "EDIT");
}

#[test]
fn input_assist_can_be_disabled_without_changing_submission() {
    let keys = b"printf '%s\\n' unchanged\rexit 0\r";
    for enabled in [false, true] {
        let (out, _) = Probe {
            mode: "repl",
            stdout_tty: true,
            stderr_tty: true,
            input_assist: enabled,
            keys: Some(keys),
            ..Default::default()
        }
        .run();
        assert_eq!(style::strip_ansi(&out).trim(), "unchanged");
    }
}

#[test]
#[ignore = "fixed-device input latency comparison; no model"]
fn input_assist_latency_comparison() {
    for single_key in [true, false] {
        let mut off = Vec::new();
        let mut on = Vec::new();
        let mut startup_off = Vec::new();
        let mut startup_on = Vec::new();
        for round in 0..5 {
            for enabled in if round % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let text: Vec<String> = (0..40)
                    .map(|i| {
                        if single_key {
                            format!("echo {}", "x".repeat(i + 1))
                        } else {
                            format!("echo latency_{i:02}")
                        }
                    })
                    .collect();
                let keys: Vec<Vec<u8>> = text
                    .iter()
                    .skip(1)
                    .map(|line| {
                        if single_key {
                            vec![b'x']
                        } else {
                            format!("\x15{line}").into_bytes()
                        }
                    })
                    .chain(std::iter::once(b"\x15exit 0\r".to_vec()))
                    .collect();
                let steps: Vec<KeyStep<'_>> = text
                    .iter()
                    .zip(&keys)
                    .map(|(text, keys)| (text.as_str(), keys.as_slice()))
                    .collect();
                let (_, _, timings) = Probe {
                    mode: "repl",
                    stdout_tty: true,
                    stderr_tty: true,
                    no_color: "1",
                    input_assist: enabled,
                    keys: Some(text[0].as_bytes()),
                    steps: &steps,
                    ..Default::default()
                }
                .run_with_timings();
                if enabled { &mut on } else { &mut off }
                    .extend(timings.input.into_iter().skip(usize::from(single_key)));
                if enabled {
                    &mut startup_on
                } else {
                    &mut startup_off
                }
                .push(timings.startup);
            }
        }
        for (enabled, mut values, mut startup) in
            [(false, off, startup_off), (true, on, startup_on)]
        {
            values.sort_unstable();
            startup.sort_unstable();
            println!(
                "{}",
                serde_json::json!({
                    "input_assist": enabled,
                    "input_pattern": if single_key { "single-key" } else { "replacement-burst" },
                    "samples": values.len(),
                    "observer_poll_ms": 5,
                    "p50_us": values[values.len() / 2].as_micros(),
                    "p95_us": values[values.len() * 95 / 100].as_micros(),
                    "p99_us": values[values.len() * 99 / 100].as_micros(),
                    "max_us": values.last().unwrap().as_micros(),
                    "startup_p50_us": startup[startup.len() / 2].as_micros(),
                    "startup_max_us": startup.last().unwrap().as_micros(),
                })
            );
        }
    }
}

#[test]
fn editing_unicode_keeps_echo_and_buffer_in_sync() {
    for initial in [
        "\u{1f600}",
        "\u{20bb7}",
        "e\u{301}",
        "\u{1f469}\u{200d}\u{1f4bb}",
        "\u{1f44d}\u{1f3fd}",
        "\u{1f1e8}\u{1f1f3}",
        "\u{2764}\u{fe0f}",
    ] {
        for terminal in ["xterm-256color", "dumb"] {
            let (out, err) = Probe {
                mode: "input",
                initial,
                keys: Some(b"\x7f\r"),
                stderr_tty: true,
                terminal: Some(terminal),
                ..Probe::default()
            }
            .run();
            assert_eq!(
                serde_json::from_str::<Option<String>>(out.trim()).unwrap(),
                Some(String::new())
            );
            assert_eq!(err.contains('\x1b'), terminal != "dumb");
        }
    }
    let (out, _) = Probe {
        mode: "input",
        initial: "old",
        stderr_tty: true,
        columns: 12,
        keys: Some(
            "\x15\x1b[200~\u{4e2d}\u{6587}\u{1f469}\u{200d}\u{1f4bb}\x1b[201~\x7f\r".as_bytes(),
        ),
        ..Probe::default()
    }
    .run();
    assert_eq!(
        serde_json::from_str::<Option<String>>(out.trim())
            .unwrap()
            .as_deref(),
        Some("\u{4e2d}\u{6587}")
    );
    for keys in [b"\x03", b"\x04"] {
        let (out, _) = Probe {
            mode: "input",
            stderr_tty: true,
            keys: Some(keys),
            ..Probe::default()
        }
        .run();
        assert_eq!(out, "null\n");
    }
}

#[test]
fn dumb_repl_executes_multiline_commands_without_escape_sequences() {
    let (out, err) = Probe {
        mode: "repl",
        terminal: Some("dumb"),
        stderr_tty: true,
        stdout_tty: true,
        keys: Some(b"for x in one two; do\rprintf '%s\\n' \"$x\"\rdone\rexit\r"),
        ..Probe::default()
    }
    .run();
    assert_eq!(out, "one\ntwo\n");
    assert!(!err.contains('\x1b'), "{err:?}");
}

#[test]
fn prompts_use_the_controlling_terminal_without_consuming_piped_stdin() {
    let (out, err) = Probe {
        mode: "input-pipe",
        stdin_pipe: true,
        stderr_tty: true,
        keys: Some(b"yes\r"),
        ..Probe::default()
    }
    .run();
    let result: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(result["answer"], "yes", "{err:?}");
    assert_eq!(result["attachment"], "attachment\n", "{err:?}");
}
