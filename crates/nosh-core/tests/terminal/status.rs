use super::support::*;

fn assert_normal_flow(text: &str) {
    for sequence in [
        "\x1b[1;23r",
        "\x1b[24;1H",
        "\x1b[r",
        "\x1b[?1049h",
        "\x1b[3J",
    ] {
        assert!(
            !text.contains(sequence),
            "unexpected reserved/full-screen display: {text:?}"
        );
    }
}

#[test]
fn inline_feedback_keeps_no_color_and_cancels_without_submission() {
    for no_color in ["", "1"] {
        let (_, err) = Probe {
            mode: "repl-inline",
            stdout_tty: true,
            stderr_tty: true,
            columns: 120,
            no_color,
            steps: &[
                ("(main)", b"echo '"),
                ("Incomplete:", b"\x03"),
                ("(main)", b"exit 0\r"),
            ],
            ..Default::default()
        }
        .run();
        assert_normal_flow(&err);
        let text = style::strip_ansi(&err);
        assert!(
            text.contains("^C cancel") && text.contains("Incomplete:"),
            "{text}"
        );
        assert!(
            !text.contains("Ctrl+G") && !text.contains("Approval:"),
            "{text}"
        );
        if no_color == "1" {
            for color in ["\x1b[31m", "\x1b[32m", "\x1b[36m", "\x1b[1;34m"] {
                assert!(!err.contains(color), "{err:?}");
            }
        }
    }
}

#[test]
fn inline_menu_search_and_acceptance_keep_one_input_owner() {
    let (_, err) = Probe {
        mode: "repl-inline",
        stdout_tty: true,
        stderr_tty: true,
        columns: 120,
        no_color: "1",
        steps: &[
            ("(main)", b"cat candidate_\t"),
            ("Completion", b"\x1b"),
            ("Tab", b"\x15\x12history_accepted"),
            ("History search: history_accepted", b"\x1b"),
            ("❯ touch history_accepted", b"\x15exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    assert_normal_flow(&err);
    let text = style::strip_ansi(&err);
    assert!(text.contains("Esc") && text.contains("Enter"), "{text}");
}

#[test]
fn inline_missing_command_explanation_arrives_without_another_key() {
    let (_, err) = Probe {
        mode: "repl-inline",
        stdout_tty: true,
        stderr_tty: true,
        columns: 120,
        no_color: "1",
        steps: &[
            ("(main)", b"absent_inline_fixture_command"),
            ("No cmd", b"\x15exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    assert_normal_flow(&err);
}

#[test]
fn inline_background_suggestion_is_explicitly_accepted_but_not_executed() {
    let (_, err) = Probe {
        mode: "repl-inline-assist",
        stdout_tty: true,
        stderr_tty: true,
        columns: 140,
        input_assist: false,
        steps: &[
            ("probe> ", b"true\r"),
            ("next: touch accepted", b"\x1bOQ"),
            ("touch accepted", b"\x15exit\r"),
        ],
        ..Default::default()
    }
    .run();
    assert_normal_flow(&err);
    assert!(style::strip_ansi(&err).contains("F2"), "{err:?}");
}

#[test]
fn disabled_inline_prompt_keeps_the_original_environment() {
    let (_, err) = Probe {
        mode: "repl-inline-off",
        stdout_tty: true,
        stderr_tty: true,
        no_color: "1",
        steps: &[("(main)", b"exit 0\r")],
        ..Default::default()
    }
    .run();
    let text = style::strip_ansi(&err);
    assert!(
        !text.contains("Ctrl+R") && !text.contains("Emacs"),
        "{text}"
    );
    assert_normal_flow(&err);
}

#[test]
fn ai_stages_keep_stop_hints_in_body_without_a_footer() {
    let (_, err) = Probe {
        mode: "status-stages",
        stderr_tty: true,
        ..Default::default()
    }
    .run();
    assert!(err.contains("Ctrl+C cancel task"), "{err:?}");
    assert!(
        err.contains("Ctrl+C interrupt command; again abort task"),
        "{err:?}"
    );
    assert!(err.contains("original tool output"), "{err:?}");
    assert_normal_flow(&err);
}

#[test]
fn inline_local_correction_is_explicit_undoable_and_only_enter_executes() {
    for (mode, after_adoption) in [
        ("repl-inline", b"\x1a".as_slice()),
        ("repl-inline-correct-execute", b"\r".as_slice()),
    ] {
        let mut steps: Vec<KeyStep<'_>> = vec![
            ("(main)", b"gti status"),
            ("try git", b"\x1b[C"),
            ("git status", after_adoption),
        ];
        if mode == "repl-inline" {
            steps.push(("try git", b"\x05\x15exit 0\r"));
        } else {
            steps.push(("Ctrl+R", b"exit 0\r"));
        }
        let (_, err) = Probe {
            mode,
            stdout_tty: true,
            stderr_tty: true,
            columns: 120,
            no_color: "1",
            steps: &steps,
            ..Default::default()
        }
        .run();
        assert_normal_flow(&err);
        let text = style::strip_ansi(&err);
        assert!(text.contains("try git") && text.contains("Tab"), "{text}");
        assert!(!text.contains("Emacs") && !text.contains("Approval:"));
    }
}

#[test]
fn inline_correction_does_not_use_a_snapshot_from_earlier_in_the_same_key_batch() {
    let (_, err) = Probe {
        mode: "repl-inline",
        stdout_tty: true,
        stderr_tty: true,
        columns: 120,
        no_color: "1",
        steps: &[
            ("(main)", b"gti status"),
            ("try git", b"x\x1b[C"),
            ("gti statusx", b"\x05\x15exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    assert_normal_flow(&err);
}

#[test]
fn inline_no_color_and_ascii_still_show_real_error_and_key_labels() {
    let (_, err) = Probe {
        mode: "repl-inline",
        stdout_tty: true,
        stderr_tty: true,
        columns: 120,
        no_color: "1",
        locale: Some("C"),
        steps: &[
            ("(main)", b"gti status"),
            ("try git", b"\x03"),
            ("Ctrl+R", b"exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    let text = style::strip_ansi(&err);
    assert!(text.contains("Right") && !text.contains("Emacs"), "{text}");
    for color in ["\x1b[31m", "\x1b[33m", "\x1b[34m", "\x1b[1;36;40m"] {
        assert!(!err.contains(color), "{err:?}");
    }
}

#[test]
fn inline_clicolor_zero_disables_keycap_color_without_hiding_semantics() {
    let (_, err) = Probe {
        mode: "repl-inline",
        stdout_tty: true,
        stderr_tty: true,
        columns: 100,
        clicolor: Some("0"),
        steps: &[
            ("(main)", b"echo '"),
            ("Incomplete:", b"\x03"),
            ("Ctrl+R", b"exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    let text = style::strip_ansi(&err);
    assert!(
        text.contains("^C cancel") && text.contains("Incomplete:"),
        "{text}"
    );
    for code in ["\x1b[31m", "\x1b[33m", "\x1b[34m", "\x1b[1;36;40m"] {
        assert!(!err.contains(code), "{err:?}");
    }
}

#[test]
fn stable_full_row_keeps_cursor_line_through_async_menu_search_and_resizes() {
    for (no_color, clicolor, locale, colorterm) in [
        ("", None, "C.UTF-8", Some("truecolor")),
        ("", None, "C.UTF-8", None),
        ("1", None, "C.UTF-8", Some("truecolor")),
        ("", Some("0"), "C.UTF-8", Some("truecolor")),
        ("1", None, "C", None),
    ] {
        let (_, err, observations) = Probe {
            mode: "repl-inline",
            stdout_tty: true,
            stderr_tty: true,
            columns: 120,
            no_color,
            clicolor,
            colorterm,
            locale: Some(locale),
            track_frames: true,
            resizes: &[(10, 48), (11, 160)],
            steps: &[
                ("(main)", b"e"),
                ("Tab", b"cho hi"),
                ("echo hi", b"\x15absent_stable_fixture"),
                ("No cmd", b"\x15gti status"),
                ("try git", b"\x1b[C"),
                ("git status", b"\x15cat candidate_\t"),
                ("Completion", b"\x1b"),
                ("Tab", b"\x15\x12history_accepted"),
                ("History search: history_accepted", b"\x1b"),
                ("Tab", b"\x15echo yz"),
                ("echo yz", b"x"),
                ("echo yzx", "\x15echo 中e\u{301}".as_bytes()),
                ("echo 中e\u{301}", b"\x15"),
                ("Ctrl+R", b"exit 0\r"),
            ],
            ..Default::default()
        }
        .run_with_timings();
        assert_normal_flow(&err);
        let mut expected_row = None;
        let mut widths = std::collections::BTreeSet::new();
        let mut frames = 0;
        let resizing = observations
            .frames
            .iter()
            .filter(|frame| frame.resizing)
            .count();
        for frame in &observations.frames {
            // Kernel size changes precede SIGWINCH processing; type only after the
            // application has painted a full row for the acknowledged new size.
            if frame.resizing {
                continue;
            }
            let (row, _) = frame.cursor;
            let input = &frame.lines[row];
            if !["❯ ", "> ", "? "]
                .iter()
                .any(|prefix| input.starts_with(*prefix))
                && !input.starts_with("cat candidate_")
            {
                continue;
            }
            frames += 1;
            widths.insert(frame.columns);
            assert!(row > 0, "input lost its status row: {frame:?}");
            if let Some(previous) = expected_row {
                assert_eq!(row, previous, "input row moved during editing: {frame:?}");
            }
            expected_row = Some(row);
            assert!(
                frame.painted[row - 1][..frame.columns - 1]
                    .iter()
                    .all(|painted| *painted),
                "status did not fill its safe width: {frame:?}"
            );
            assert!(
                !frame.painted[row - 1][frame.columns - 1],
                "status wrote the autowrap column: {frame:?}"
            );
            let colored = no_color.is_empty() && clicolor != Some("0");
            assert!(
                frame.backgrounds[row - 1][..frame.columns - 1]
                    .iter()
                    .all(|background| background.is_some() == colored),
                "missing/forbidden background: {frame:?}"
            );
            assert!(
                frame.backgrounds[row].iter().all(Option::is_none),
                "background leaked into the input line: {frame:?}"
            );
        }
        assert!(
            frames >= 12,
            "not enough actual redraw observations: {frames}: {err:?}"
        );
        assert_eq!(widths, [48, 120, 160].into_iter().collect());
        println!(
            "stable_row_frames={frames}, resize_pending_frames={resizing}, input_row={expected_row:?}, colored={}, rgb={}",
            no_color.is_empty() && clicolor != Some("0"),
            colorterm == Some("truecolor")
        );
    }
}

#[test]
fn full_row_background_never_reaches_submitted_command_output() {
    let (_, err, observations) = Probe {
        mode: "repl-inline",
        stdout_tty: true,
        stderr_tty: true,
        columns: 120,
        track_frames: true,
        colorterm: Some("truecolor"),
        steps: &[
            ("(main)", b"printf '%s\\n' ROW_OUTPUT\r"),
            ("ROW_OUTPUT\r\n", b"exit 0\r"),
        ],
        ..Default::default()
    }
    .run_with_timings();
    assert_normal_flow(&err);
    let text: String = observations
        .printed
        .iter()
        .map(|glyph| glyph.character)
        .collect();
    let marker = "ROW_OUTPUT";
    let mut matches = 0;
    for offset in text
        .match_indices(marker)
        .map(|(offset, _)| text[..offset].chars().count())
    {
        matches += 1;
        assert!(
            observations.printed[offset..offset + marker.len()]
                .iter()
                .all(|glyph| glyph.background.is_none()),
            "background leaked onto input/command output"
        );
    }
    assert!(
        matches >= 2,
        "command output was not independently observed: {err:?}"
    );
}

#[test]
fn theme_updates_repaint_real_edit_menu_and_search_without_restarting_the_editor() {
    let (_, err, observations) = Probe {
        mode: "repl-inline-theme",
        stdout_tty: true,
        stderr_tty: true,
        columns: 120,
        colorterm: Some("truecolor"),
        track_frames: true,
        steps: &[
            ("(main)", b"echo theme_draft"),
            ("echo theme_draft", b"@theme-switch"),
            (
                "\x1b[0;38;2;238;243;248;48;2;30;64;83m",
                b"\x15cat candidate_\t",
            ),
            ("Completion", b"@theme-switch"),
            ("\x1b[0;38;2;238;243;248;48;2;41;54;70m", b"\x1b"),
            ("Tab", b"\x15\x12history_accepted"),
            ("History search: history_accepted", b"@theme-switch"),
            ("\x1b[0;38;2;238;243;248;48;2;20;60;64m", b"\x1b"),
            ("❯ touch history_accepted", b"\x15exit 0\r"),
        ],
        ..Default::default()
    }
    .run_with_timings();
    assert_normal_flow(&err);
    for (background, draft, state) in [
        ([30, 64, 83], "echo theme_draft", ""),
        ([41, 54, 70], "cat candidate_", "Completion"),
        ([20, 60, 64], "touch history_accepted", "History search"),
    ] {
        let frame = observations
            .frames
            .iter()
            .find(|frame| {
                let row = frame.cursor.0;
                row > 0
                    && frame.lines[row].contains(draft)
                    && frame.backgrounds[row - 1][0] == Some(super::screen::Color::Rgb(background))
                    && frame.lines[row - 1].contains(state)
            })
            .unwrap_or_else(|| {
                panic!("theme update did not reach the live {draft} interaction: {err:?}")
            });
        assert_eq!(frame.cursor.0, 2);
        assert!(frame.painted[1][..119].iter().all(|cell| *cell));
        assert!(!frame.painted[1][119]);
        assert!(frame.backgrounds[2].iter().all(Option::is_none));
    }
}
