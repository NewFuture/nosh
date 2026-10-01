use super::support::*;

#[test]
fn redirected_answers_have_no_thinking_newline_or_decoration() {
    for stderr_tty in [false, true] {
        let (out, err) = Probe {
            stderr_tty,
            ..Probe::default()
        }
        .run();
        assert_eq!(out, "answer\n\u{1f469}\u{200d}\u{1f4bb}\n");
        assert!(style::strip_ansi(&err).contains("thinking\n"));
        assert_eq!(err.contains("\x1b["), stderr_tty);
    }
    let (out, err) = Probe {
        stdout_tty: true,
        ..Probe::default()
    }
    .run();
    assert!(
        out.contains("\x1b[36m"),
        "stdout colors do not depend on stderr"
    );
    assert!(!err.contains('\x1b'));
}

#[test]
fn terminal_and_locale_matrix_has_safe_fallbacks() {
    for terminal in [None, Some("dumb"), Some("unknown"), Some("")] {
        let (out, err) = Probe {
            terminal,
            stdout_tty: true,
            stderr_tty: true,
            ..Probe::default()
        }
        .run();
        assert!(!out.contains('\x1b') && !err.contains('\x1b'));
        assert!(out.starts_with("| answer\n"), "{out:?}");
        let (_, err) = Probe {
            mode: "status",
            terminal,
            stderr_tty: true,
            ..Probe::default()
        }
        .run();
        assert!(err.is_empty());
    }
    for terminal in ["xterm-256color", "screen", "tmux-256color", "rxvt-unicode"] {
        let (out, err) = Probe {
            terminal: Some(terminal),
            no_color: "1",
            stdout_tty: true,
            stderr_tty: true,
            ..Probe::default()
        }
        .run();
        assert!(!out.contains('\x1b') && !err.contains('\x1b'));
        assert!(out.starts_with("\u{2503} answer\n"));
    }
    for locale in [
        None,
        Some(""),
        Some("C"),
        Some("POSIX"),
        Some("zh_CN.GB18030"),
    ] {
        let (out, _) = Probe {
            locale,
            stdout_tty: true,
            no_color: "1",
            ..Probe::default()
        }
        .run();
        assert!(out.starts_with("| answer\n"));
        assert!(
            out.contains("\u{1f469}\u{200d}\u{1f4bb}"),
            "data is never transliterated"
        );
    }
    let (_, err) = Probe {
        mode: "status",
        stderr_tty: true,
        no_color: "1",
        ..Probe::default()
    }
    .run();
    assert!(
        err.contains("\r\x1b[K"),
        "NO_COLOR alone does not disable cursor control"
    );
    assert!(!err.contains("\x1b[2m"));
}

#[test]
fn tool_labels_follow_the_destination_without_rewriting_command_text() {
    for (terminal, locale, stderr_tty, unicode) in [
        ("xterm-256color", Some("C.UTF-8"), true, true),
        ("dumb", Some("C.UTF-8"), true, false),
        ("xterm-256color", Some("C"), true, false),
        ("xterm-256color", None, true, false),
        ("xterm-256color", Some("C.UTF-8"), false, false),
    ] {
        for no_color in ["", "1"] {
            let (out, err) = Probe {
                mode: "tool-labels",
                terminal: Some(terminal),
                locale,
                no_color,
                stderr_tty,
                ..Probe::default()
            }
            .run();
            assert!(out.is_empty());
            let text = style::strip_ansi(&err);
            let lines: Vec<_> = text.lines().collect();
            let stride = if stderr_tty { 3 } else { 2 };
            assert_eq!(lines.len(), TOOL_LABELS.len() * stride);
            let (bar, marker, separator) = if unicode {
                ("\u{2503}", "\u{2699}", " \u{b7} ")
            } else {
                ("|", "*", " | ")
            };
            for (i, (_, label)) in TOOL_LABELS.iter().enumerate() {
                let start = stride * i;
                if stderr_tty {
                    assert_eq!(
                        lines[start],
                        format!("{bar} Ctrl+C interrupt command; again abort task")
                    );
                }
                let header = start + usize::from(stderr_tty);
                assert_eq!(
                    lines[header],
                    format!(
                        "{bar} {marker} run_command  {}",
                        label.replace(" \u{b7} ", separator)
                    ),
                    "{terminal} {locale:?} tty={stderr_tty} NO_COLOR={no_color:?}"
                );
                if !unicode {
                    assert!(lines[header].is_ascii());
                }
                assert_eq!(
                    lines[header + 1],
                    format!("{bar}   $ echo \u{4e2d}\u{6587}")
                );
            }
        }
    }
}

#[test]
fn proposal_explanations_keep_the_detail_prefix_in_both_renderers() {
    for (terminal, bar, arrow) in [
        ("xterm-256color", "\u{2503}", "\u{21b3}"),
        ("dumb", "|", "->"),
    ] {
        let (out, err) = Probe {
            mode: "proposal",
            terminal: Some(terminal),
            stderr_tty: true,
            no_color: "1",
            ..Probe::default()
        }
        .run();
        assert!(out.is_empty());
        assert_eq!(
            err,
            format!(
                "{bar} {arrow} printf hello\n{bar}   Suggested explanation.\n{bar}   More detail.\n{bar} Final answer.\n"
            )
        );
    }
}

#[test]
fn clipping_uses_stderr_size_and_observes_resizes() {
    for (columns, chinese) in [(80, 37), (20, 7), (8, 1)] {
        let (_, err) = Probe {
            mode: "lines",
            stdout_tty: true,
            stderr_tty: true,
            columns,
            no_color: "1",
            ..Probe::default()
        }
        .run();
        let lines: Vec<_> = err.lines().collect();
        assert_eq!(lines[0].matches('\u{4e2d}').count(), chinese);
        assert!(
            lines.iter().all(|l| style::width(l) < usize::from(columns)),
            "{err:?}"
        );
        assert!(!err.contains('\t'));
    }
    let (_, err) = Probe {
        mode: "resize",
        stderr_tty: true,
        no_color: "1",
        ..Probe::default()
    }
    .run();
    let lines: Vec<_> = err.lines().collect();
    assert_eq!(lines[0].matches('\u{4e2d}').count(), 37);
    assert_eq!(lines[1].matches('\u{4e2d}').count(), 7);
}

#[test]
fn hidden_prompts_are_unavailable_and_plain_download_logs_keep_notes() {
    let (out, _) = Probe {
        mode: "available",
        stdout_tty: true,
        ..Probe::default()
    }
    .run();
    assert_eq!(out, "false\n");
    for stderr_tty in [false, true] {
        let (_, err) = Probe {
            mode: "progress",
            terminal: Some("dumb"),
            stderr_tty,
            ..Probe::default()
        }
        .run();
        assert!(err.contains("downloading model.gguf") && err.contains("source changed"));
        assert!(!err.contains('\x1b'));
    }
}

#[test]
fn json_output_keeps_original_text_on_both_pipes_and_terminals() {
    for stdout_tty in [false, true] {
        for (terminal, locale) in [
            ("xterm-256color", Some("C.UTF-8")),
            ("dumb", Some("C.UTF-8")),
            ("xterm-256color", Some("C")),
        ] {
            let (out, err) = Probe {
                mode: "json",
                terminal: Some(terminal),
                locale,
                stdout_tty,
                ..Probe::default()
            }
            .run();
            let events: Vec<serde_json::Value> = out
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect();
            assert_eq!(events.len(), 2 + TOOL_LABELS.len());
            assert_eq!(events[0]["text"], "\u{1f469}\u{200d}\u{1f4bb}");
            assert_eq!(events[1]["text"], "\x1b[31mraw\r\n");
            assert_eq!(events[1]["stream"], "stderr");
            for (event, (risk, label)) in events[2..].iter().zip(TOOL_LABELS) {
                assert_eq!(event["risk"], risk.label());
                assert_eq!(event["decision"], label);
                assert_eq!(event["detail"], "echo \u{4e2d}\u{6587}");
            }
            assert!(!out.contains('\x1b') && err.is_empty());
        }
    }
}
