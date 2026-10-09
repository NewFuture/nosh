use super::support::*;
use nosh_shell::{AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell};

struct Ai;

impl AiHandler for Ai {
    fn handle(&mut self, shell: &mut EmbeddedShell, _: AiRequest) -> AiOutcome {
        struct Sink;
        impl nosh_shell::OutputSink for Sink {
            fn stdout(&mut self, text: &str) {
                eprint!("{text}");
            }
            fn stderr(&mut self, text: &str) {
                eprint!("{text}");
            }
        }
        shell
            .run_agent_command("printf 'AI-TOOL-ONE\\n'", &Default::default(), &mut Sink)
            .unwrap();
        shell
            .run_agent_command(
                "printf 'AI-TOOL-TWO\\n'; false",
                &Default::default(),
                &mut Sink,
            )
            .unwrap();
        eprintln!("AI-FINISHED");
        AiOutcome::default()
    }
    fn command(
        &mut self,
        shell: &mut EmbeddedShell,
        _: nosh_shell::ManagementCommand,
    ) -> AiOutcome {
        self.handle(
            shell,
            AiRequest {
                trigger: nosh_shell::Trigger::Hash,
                text: "test".into(),
                failed: None,
                user_output: None,
            },
        )
    }
    fn suggest(&mut self, _: &mut EmbeddedShell, _: &str) -> Option<String> {
        Some("printf generated".into())
    }
    fn badge(&self) -> Badge {
        Badge::default()
    }
}

pub(super) fn probe(mode: &str) {
    let directory = tempfile::tempdir().unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        interactive: true,
        working_dir: Some(directory.path().into()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line("export TERM_PROGRAM=WezTerm; unset TMUX STY; PATH=/usr/bin:/bin; PS1='integration> '; PS2='more> '");
    if mode.ends_with("-unknown") {
        shell.run_user_line("TERM_PROGRAM=unknown");
    }
    if mode.ends_with("-external") {
        shell.run_user_line("__vsc_prompt_cmd() { :; }");
    }
    if mode.ends_with("-tmux") {
        shell.run_user_line("TMUX=fake-session");
    }
    let wait_cancel = mode.ends_with("-wait-cancel");
    let slow_environment = mode.ends_with("-slow-environment") || wait_cancel;
    let ready_environment = mode.ends_with("-ready-environment");
    let process_file =
        std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("environment-pids");
    if slow_environment {
        use std::os::unix::fs::PermissionsExt;
        let bin = directory.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        let tool = bin.join("direnv");
        std::fs::write(&tool, "#!/bin/sh\necho $$ >> \"$HOME/environment-pids\"\nsleep 30 &\necho $! >> \"$HOME/environment-pids\"\nwait\n").unwrap();
        std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o700)).unwrap();
        shell.run_user_line(&format!("export PATH='{}':/usr/bin:/bin", bin.display()));
        shell.configure_project_env(Ok(nosh_shell::project_env::Provider::Direnv));
    }
    if ready_environment {
        use std::os::unix::fs::PermissionsExt;
        for name in ["bin", "project-bin"] {
            std::fs::create_dir(directory.path().join(name)).unwrap();
        }
        let delta = serde_json::json!({"PATH": format!("{}:/usr/bin:/bin", directory.path().join("project-bin").display())}).to_string();
        let script = format!(
            "#!/bin/sh\nprintf x >> \"$HOME/environment-calls\"\nsleep 0.15\nprintf '%s' '{}'\n",
            delta.replace('\'', "'\\''")
        );
        for (name, script) in [
            ("bin/direnv", script),
            (
                "project-bin/provided-command",
                "#!/bin/sh\nprintf 'ENV-TOOL-EXEC\\n'\n".into(),
            ),
        ] {
            let path = directory.path().join(name);
            std::fs::write(&path, script).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        shell.run_user_line(&format!(
            "export PATH='{}':/usr/bin:/bin",
            directory.path().join("bin").display()
        ));
        shell.configure_project_env(Ok(nosh_shell::project_env::Provider::Direnv));
    }
    let config = nosh_shell::ReplConfig {
        terminal_integration: if mode.ends_with("-off") {
            nosh_shell::terminal_integration::Mode::Off
        } else {
            nosh_shell::terminal_integration::Mode::Auto
        },
        input_assist: nosh_shell::input_assist::Config {
            enabled: ready_environment,
            worker: ready_environment.then(input_worker_command),
        },
        completion: nosh_shell::completion::Config {
            enabled: ready_environment,
            worker: ready_environment.then(input_worker_command),
            ..Default::default()
        },
        status_bar: nosh_shell::status::Config {
            enabled: false,
            ..Default::default()
        },
        command_assist: false,
        ..Default::default()
    };
    let cancellation = wait_cancel.then(|| {
        let interrupts = shell.interrupts();
        std::thread::spawn(move || {
            let until = Instant::now() + Duration::from_secs(5);
            let mut editing = false;
            while Instant::now() < until {
                let mut attributes: libc::termios = unsafe { std::mem::zeroed() };
                if unsafe { libc::tcgetattr(0, &raw mut attributes) } == 0 {
                    if attributes.c_lflag & libc::ICANON == 0 {
                        editing = true;
                    } else if editing {
                        interrupts.fire();
                        return;
                    }
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            panic!("submitted input never reached the environment wait");
        })
    });
    assert_eq!(
        nosh_shell::repl::run(&mut shell, &mut Ai, config),
        if slow_environment { 130 } else { 0 }
    );
    if let Some(cancellation) = cancellation {
        cancellation.join().unwrap();
        assert!(!directory.path().join("cancelled-command").exists());
    }
    drop(shell);
    if ready_environment {
        let calls =
            std::path::PathBuf::from(std::env::var_os("HOME").unwrap()).join("environment-calls");
        assert_eq!(
            std::fs::read(calls).unwrap(),
            b"xx",
            "only two prompt boundaries may start a refresh"
        );
    }
    if slow_environment {
        for pid in std::fs::read_to_string(process_file).unwrap().lines() {
            let pid: i32 = pid.parse().unwrap();
            assert_eq!(
                unsafe { libc::kill(pid, 0) },
                -1,
                "environment process {pid} survived exit"
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ESRCH)
            );
        }
    }
}

fn exit_codes(text: &str) -> Vec<&str> {
    text.split("\x1b]133;D;")
        .skip(1)
        .map(|text| text.split_once("\x1b\\").unwrap().0)
        .collect()
}

#[test]
fn integration_cancelling_a_submitted_environment_wait_never_executes_the_draft() {
    let (_, err) = Probe {
        mode: "repl-integration-wait-cancel",
        stdout_tty: true,
        stderr_tty: true,
        steps: &[
            ("@file:environment-pids", b"touch cancelled-command\r"),
            ("refresh cancelled", b"\x15\x04"),
        ],
        ..Default::default()
    }
    .run();
    assert!(exit_codes(&err).is_empty(), "{err:?}");
    assert!(!err.contains("\x1b]133;C\x1b\\"), "{err:?}");
}

#[test]
fn integration_ready_environment_preserves_draft_and_updates_completion_without_enter() {
    let (out, err) = Probe {
        mode: "repl-integration-ready-environment",
        stdout_tty: true,
        stderr_tty: true,
        steps: &[
            ("integration> ", b"provided-c"),
            ("environment ready", b"\t"),
            ("provided-command", b"\r"),
            ("ENV-TOOL-EXEC", b"exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    assert!(out.contains("ENV-TOOL-EXEC"), "{out:?}");
    assert_eq!(exit_codes(&err), ["0", "0"]);
}

#[test]
fn integration_real_commands_and_ai_have_distinct_completion_semantics() {
    let (out, err) = Probe {
        mode: "repl-integration",
        stdout_tty: true,
        stderr_tty: true,
        no_color: "1",
        steps: &[
            ("integration> ", b"printf 'USER-OUTPUT\\n'\r"),
            ("integration> ", b"false\r"),
            ("integration> ", b"# run two tools\r"),
            ("AI-FINISHED", b"exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    assert!(out.contains("USER-OUTPUT"));
    assert!(err.contains("\x1b]7;file://"));
    assert!(err.contains("\x1b]133;A"));
    assert!(err.contains("\x1b]133;B"));
    assert_eq!(err.matches("\x1b]133;C\x1b\\").count(), 4, "{err:?}");
    assert_eq!(exit_codes(&err), ["0", "1", "0"], "{err:?}");
    let ai = err.split_once("AI-TOOL-ONE").unwrap().1;
    assert!(ai.contains("AI-TOOL-TWO"));
    assert!(ai.contains("AI-FINISHED\n\x1b]133;D\x1b\\"), "{ai:?}");
    assert!(
        !out.contains("\x1b]133"),
        "nosh markers must use the editor's stream"
    );
}

#[test]
fn integration_empty_cancel_and_suggestion_do_not_finish_shell_commands() {
    let (_, err) = Probe {
        mode: "repl-integration",
        stdout_tty: true,
        stderr_tty: true,
        steps: &[
            ("integration> ", b"\r"),
            ("integration> ", b"printf draft"),
            ("printf draft", b"\x03"),
            ("integration> ", b"please suggest"),
            ("please suggest", b"\x1bOQ"),
            ("printf generated", b"\x03"),
            ("integration> ", b"exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    assert_eq!(err.matches("\x1b]133;C\x1b\\").count(), 1, "{err:?}");
    assert_eq!(exit_codes(&err), ["0"]);
}

#[test]
fn integration_multiline_is_one_execution_region() {
    let (_, err) = Probe {
        mode: "repl-integration",
        stdout_tty: true,
        stderr_tty: true,
        steps: &[
            ("integration> ", b"printf 'one\r"),
            ("more> ", b"two'\r"),
            ("integration> ", b"exit 0\r"),
        ],
        ..Default::default()
    }
    .run();
    assert_eq!(err.matches("\x1b]133;C\x1b\\").count(), 2, "{err:?}");
    assert_eq!(exit_codes(&err), ["0", "0"]);
}

#[test]
fn integration_disables_metadata_for_unknown_external_multiplexed_and_dumb_terminals() {
    for (mode, terminal) in [
        ("repl-integration-unknown", "xterm-256color"),
        ("repl-integration-external", "xterm-256color"),
        ("repl-integration-tmux", "xterm-256color"),
        ("repl-integration-off", "xterm-256color"),
        ("repl-integration", "dumb"),
    ] {
        let (out, err) = Probe {
            mode,
            terminal: Some(terminal),
            stdout_tty: true,
            stderr_tty: true,
            steps: &[("integration> ", b"exit 0\r")],
            ..Default::default()
        }
        .run();
        for stream in [&out, &err] {
            assert!(!stream.contains("\x1b]7;"), "{mode}: {stream:?}");
            assert!(!stream.contains("\x1b]133;"), "{mode}: {stream:?}");
        }
    }
}

#[test]
fn integration_independently_gates_stdout_and_stderr_redirection() {
    for (stdout_tty, stderr_tty) in [(true, false), (false, true)] {
        let (out, err) = Probe {
            mode: "repl-integration",
            stdout_tty,
            stderr_tty,
            steps: &[("integration> ", b"exit 0\r")],
            ..Default::default()
        }
        .run();
        for stream in [&out, &err] {
            assert!(!stream.contains("\x1b]7;"), "{stream:?}");
            assert!(!stream.contains("\x1b]133;"), "{stream:?}");
        }
    }
}

#[test]
fn integration_slow_environment_does_not_block_edit_cancel_or_exit() {
    let (_, _, timings) = Probe {
        mode: "repl-integration-slow-environment",
        stdout_tty: true,
        stderr_tty: true,
        steps: &[
            ("@file:environment-pids", b"draft while loading"),
            ("draft while loading", b"\x03"),
            ("integration> ", b"\x04"),
        ],
        ..Default::default()
    }
    .run_with_timings();
    assert!(
        timings.exit < Duration::from_millis(500),
        "{:?}",
        timings.exit
    );
    assert!(
        timings.input[1] < Duration::from_millis(500),
        "{:?}",
        timings.input
    );
}
