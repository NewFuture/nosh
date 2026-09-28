use super::support::*;

pub(super) fn terminal_probe() {
    let Ok(mode) = std::env::var("NOSH_TERMINAL_PROBE") else {
        return;
    };
    print!("{BEGIN}");
    eprint!("{BEGIN}");
    match mode.as_str() {
        "text" => {
            let mut ui = TermUi::new(true);
            ui.show_think = true;
            ui.think("thinking");
            ui.text("answer\r");
            ui.text("\n\u{1f469}\u{200d}\u{1f4bb}");
            ui.pause();
        }
        "lines" | "resize" => {
            let mut ui = TermUi::new(false);
            ui.output(&("\u{4e2d}".repeat(50) + "\n"), false);
            if mode == "resize" {
                resize(libc::STDERR_FILENO, 20);
                ui.output(&("\u{4e2d}".repeat(50) + "\n"), false);
            } else {
                ui.output(&("x\t".repeat(30) + "\n"), true);
            }
        }
        "status" => {
            let mut ui = TermUi::new(false);
            ui.prefill(1, 1000);
            ui.pause();
        }
        "approval-state" => {
            let mut ui = TermUi::new(false);
            for mode in [
                nosh_permissions::ApprovalMode::Confirm,
                nosh_permissions::ApprovalMode::Auto,
                nosh_permissions::ApprovalMode::Yolo,
            ] {
                ui.state(mode, nosh_core::ui::Activity::Running);
                ui.state(mode, nosh_core::ui::Activity::NeedsUser);
            }
        }
        "approval-card" => {
            let mut approval = nosh_core::TerminalApproval::default();
            let answer = nosh_core::ApprovalChannel::request(
                &mut approval,
                &nosh_core::ApprovalRequest {
                    tool: "run_command".into(),
                    command: "fixture-prohibited-operation".into(),
                    risk: Risk::Forbidden,
                    reasons: vec!["built-in prohibition".into()],
                    strong: true,
                    can_grant: false,
                    can_edit: true,
                    mode: nosh_permissions::ApprovalMode::Confirm,
                },
            );
            println!("{answer:?}");
        }
        "proposal" => {
            let mut ui = TermUi::new(false);
            ui.proposed("printf hello", Some("Suggested explanation.\nMore detail."));
            ui.text("Final answer.\n");
            ui.pause();
        }
        "tool-labels" => {
            let mut ui = TermUi::new(false);
            for (risk, label) in TOOL_LABELS {
                ui.tool_start("run_command", "echo \u{4e2d}\u{6587}", Some(risk), label);
            }
        }
        "input" | "input-pipe" => {
            let initial = std::env::var("NOSH_TERMINAL_INITIAL").unwrap();
            let answer = term::read_text("input> ", &initial);
            if mode == "input-pipe" {
                let mut attachment = String::new();
                std::io::stdin().read_to_string(&mut attachment).unwrap();
                println!(
                    "{}",
                    serde_json::json!({"answer": answer, "attachment": attachment})
                );
            } else {
                println!("{}", serde_json::to_string(&answer).unwrap());
            }
            // The raw-mode guard must also restore the terminal after cancellation.
            assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
        }
        "progress" => {
            let progress = BarProgress::new();
            progress.start("model.gguf", 100, 0);
            progress.note("source changed");
            progress.finish(false);
        }
        "available" => println!("{}", term::available()),
        "json" => {
            let mut ui = JsonUi;
            ui.state(
                nosh_permissions::ApprovalMode::Auto,
                nosh_core::ui::Activity::Running,
            );
            ui.text("\u{1f469}\u{200d}\u{1f4bb}");
            ui.output("\x1b[31mraw\r\n", true);
            for (risk, label) in TOOL_LABELS {
                ui.tool_start("run_command", "echo \u{4e2d}\u{6587}", Some(risk), label);
            }
        }
        "repl-assist" => {
            let directory = tempfile::tempdir().unwrap();
            let mut shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
                interactive: true,
                working_dir: Some(directory.path().into()),
                ..Default::default()
            })
            .unwrap();
            shell.run_user_line("PATH=/usr/bin:/bin; PS1='probe> '");
            let mut ai = nosh_core::ShellAi::new(
                Box::new(|_| {
                    Ok(nosh_core::LoadedEngine {
                        engine: Box::new(nosh_llm::MockChatEngine::with_responder(|_| {
                            vec![nosh_llm::mock::call(
                                "finish",
                                serde_json::json!({
                                    "kind": "command", "text": "touch accepted"
                                }),
                            )]
                        })),
                        description: "mock".into(),
                    })
                }),
                nosh_core::AgentConfig::default(),
                Box::new(nosh_core::NoTerminal),
            );
            let config = nosh_shell::ReplConfig {
                input_assist: nosh_shell::input_assist::Config {
                    enabled: false,
                    worker: None,
                },
                ..Default::default()
            };
            assert_eq!(nosh_shell::repl::run(&mut shell, &mut ai, config), 0);
            assert_eq!(
                directory.path().join("accepted").exists(),
                std::env::var("NOSH_TERMINAL_INITIAL").as_deref() == Ok("execute")
            );
        }
        "repl" | "repl-blocked" => {
            let mut shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
                interactive: true,
                ..nosh_shell::ShellOptions::default()
            })
            .unwrap();
            shell.run_user_line("PATH=/usr/bin:/bin; PS1='probe> '");
            let worker = nosh_shell::input_assist::WorkerCommand {
                program: std::env::current_exe().unwrap(),
                args: vec![
                    "--exact".into(),
                    if mode == "repl-blocked" {
                        "blocked_input_worker_probe".into()
                    } else {
                        "input_worker_probe".into()
                    },
                    "--nocapture".into(),
                ],
            };
            let config = nosh_shell::ReplConfig {
                trigger: nosh_shell::TriggerConfig {
                    ai_enabled: false,
                    ..Default::default()
                },
                input_assist: nosh_shell::input_assist::Config {
                    enabled: std::env::var("NOSH_TERMINAL_INPUT_ASSIST").as_deref() != Ok("0"),
                    worker: Some(worker),
                },
                ..Default::default()
            };
            assert_eq!(
                nosh_shell::repl::run(&mut shell, &mut nosh_shell::repl::NoAi, config),
                if mode == "repl-blocked" { 130 } else { 0 }
            );
        }
        _ => panic!("unknown probe"),
    }
    println!("{END}");
    eprintln!("{END}");
}

pub(super) fn input_worker_probe() {
    if let Some(code) = nosh_shell::input_assist::run_worker_from_env() {
        std::process::exit(code);
    }
}

pub(super) fn blocked_input_worker_probe() {
    if std::env::var("NOSH_INPUT_WORKER").as_deref() == Ok("lookup") {
        // SAFETY: getppid takes no pointers and reports this worker's parent.
        let parent = unsafe { libc::getppid() };
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(blocked_input_marker(parent as u32))
            .unwrap();
        while unsafe { libc::getppid() } == parent {
            thread::park_timeout(Duration::from_secs(1));
        }
        std::process::exit(0);
    }
    input_worker_probe();
}
