use super::support::*;

#[test]
fn automatic_assistance_is_displayed_and_accepting_never_executes_without_enter() {
    for terminal in ["xterm-256color", "dumb"] {
        for execute in [false, true] {
            let finish: &[u8] = if execute { b"\rexit\r" } else { b"\x15exit\r" };
            let steps: &[KeyStep<'_>] = &[
                ("probe> ", b"true\r"),
                ("next: touch accepted", b"\x07"),
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
