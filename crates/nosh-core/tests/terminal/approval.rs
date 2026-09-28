use super::support::*;

#[test]
fn approval_modes_and_activity_are_separate_and_do_not_decorate_pipes() {
    for terminal in ["xterm-256color", "dumb"] {
        let (_, err) = Probe {
            mode: "approval-state",
            terminal: Some(terminal),
            stderr_tty: true,
            ..Default::default()
        }
        .run();
        for mode in ["Confirm", "Auto", "YOLO"] {
            assert!(err.contains(&format!("Approval: {mode}")), "{err}");
        }
        assert!(err.contains("| Running"), "{err}");
        assert!(err.contains("| Needs your attention"), "{err}");
        if terminal == "dumb" {
            assert!(!err.contains('\x1b'));
        }
    }
    let (out, err) = Probe {
        mode: "approval-state",
        ..Default::default()
    }
    .run();
    assert!(out.trim().is_empty());
    assert!(err.trim().is_empty());
}

#[test]
fn built_in_prohibition_requires_yes_and_never_offers_a_session_grant() {
    let (out, err) = Probe {
        mode: "approval-card",
        stderr_tty: true,
        keys: Some(b"yes\r"),
        ..Default::default()
    }
    .run();
    assert_eq!(out.trim(), "Approve");
    assert!(err.contains("Approval: Confirm"));
    assert!(err.contains("Awaiting approval"));
    assert!(err.contains("type yes"));
    assert!(!err.contains("[a]"));
}
