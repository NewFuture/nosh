use super::support::*;

pub(super) fn terminal_probe() {
    let Ok(mode) = std::env::var("NOSH_TERMINAL_PROBE") else {
        return;
    };
    print!("{BEGIN}");
    eprint!("{BEGIN}");
    let original_stdout = if mode.starts_with("repl-inline") || mode.starts_with("repl-completion")
    {
        std::io::stdout().flush().unwrap();
        // SAFETY: preserve the harness framing stream, then give the editor one TTY.
        let saved = unsafe { libc::dup(1) };
        assert!(saved >= 0);
        assert_eq!(unsafe { libc::dup2(2, 1) }, 1);
        Some(saved)
    } else {
        None
    };
    match mode.as_str() {
        "status-stages" => {
            let mut ui = TermUi::new(false);
            ui.state(
                nosh_permissions::ApprovalMode::Auto,
                nosh_core::ui::Activity::Thinking,
            );
            ui.text("answer\n");
            ui.state(
                nosh_permissions::ApprovalMode::Auto,
                nosh_core::ui::Activity::Running,
            );
            ui.tool_start("exec", "true", Some(Risk::Safe), "SAFE");
            ui.output("original tool output\n", false);
            ui.tool_end("exit 0");
            ui.pause();
        }
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
        "approval-card" | "approval-card-path" => {
            let mut approval = nosh_core::TerminalApproval::default();
            let answer = nosh_core::ApprovalChannel::request(
                &mut approval,
                &nosh_core::ApprovalRequest {
                    tool: "exec".into(),
                    command: "fixture-prohibited-operation".into(),
                    cwd: std::path::PathBuf::from(if mode == "approval-card-path" {
                        "/fixture/line\nwith\ttab"
                    } else {
                        "/fixture/approval-cwd"
                    }),
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
                ui.tool_start("exec", "echo \u{4e2d}\u{6587}", Some(risk), label);
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
        "ask-user" | "ask-user-pipe" | "ask-user-cancel" => {
            use nosh_core::user_input::{TerminalUserInput, UserInput, UserQuestion};
            let cancel = nosh_llm::CancelHandle::default();
            let cancellation = if mode == "ask-user-cancel" {
                let handle = cancel.clone();
                Some(thread::spawn(move || {
                    thread::sleep(Duration::from_millis(200));
                    handle.cancel();
                }))
            } else {
                None
            };
            let answer = TerminalUserInput.ask(
                &UserQuestion {
                    question: "Archive format?".into(),
                    choices: vec!["tar.gz".into(), "zip".into()],
                },
                &cancel,
            );
            if let Some(thread) = cancellation {
                thread.join().unwrap();
            }
            let mut attachment = String::new();
            if mode == "ask-user-pipe" {
                std::io::stdin().read_to_string(&mut attachment).unwrap();
            }
            println!(
                "{}",
                serde_json::json!({
                    "answer": answer.as_ref().ok(),
                    "error": answer.as_ref().err().map(ToString::to_string),
                    "attachment": attachment,
                })
            );
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
                ui.tool_start("exec", "echo \u{4e2d}\u{6587}", Some(risk), label);
            }
        }
        "repl-assist" | "repl-inline-assist" | "repl-generate" => {
            let directory = tempfile::tempdir().unwrap();
            let mut shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
                interactive: true,
                working_dir: Some(directory.path().into()),
                ..Default::default()
            })
            .unwrap();
            shell.run_user_line("PATH=/usr/bin:/bin; PS1='probe> '");
            let mut ai = nosh_core::ShellAi::new(
                Box::new(move |_| {
                    Ok(nosh_core::LoadedEngine {
                        engine: Box::new(nosh_llm::MockChatEngine::with_responder(|_| {
                            vec![nosh_llm::mock::text("touch accepted")]
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
        mode if mode.starts_with("repl-editing") => super::editing::probe(mode),
        mode if mode.starts_with("repl-completion") => super::completion::probe(mode),
        "repl"
        | "repl-blocked"
        | "repl-inline"
        | "repl-inline-off"
        | "repl-inline-correct-execute"
        | "repl-inline-theme" => {
            let directory = tempfile::tempdir().unwrap();
            let mut shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
                interactive: true,
                working_dir: mode
                    .starts_with("repl-inline")
                    .then(|| directory.path().to_path_buf()),
                ..nosh_shell::ShellOptions::default()
            })
            .unwrap();
            if mode.starts_with("repl-inline") {
                std::fs::create_dir(directory.path().join(".git")).unwrap();
                std::fs::write(directory.path().join(".git/HEAD"), "ref: refs/heads/main\n")
                    .unwrap();
                std::fs::write(directory.path().join("candidate_one"), "").unwrap();
                std::fs::write(directory.path().join("candidate_two"), "").unwrap();
                shell.run_user_line("PATH=/usr/bin:/bin; unset PS1; PS2='continue> '");
                shell.run_user_line("git() { printf '%s\\n' executed >> correction_executed; }");
                shell.add_history("touch history_accepted");
            } else {
                shell.run_user_line("PATH=/usr/bin:/bin; PS1='probe> '");
            }
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
                completion: nosh_shell::completion::Config {
                    worker: Some(worker.clone()),
                    ..Default::default()
                },
                status_bar: nosh_shell::status::Config {
                    enabled: mode != "repl-inline-off",
                    ..Default::default()
                },
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
            let theme_updates = (mode == "repl-inline-theme").then(|| {
                let themes = config.status_bar.theme.clone();
                let home = std::path::PathBuf::from(std::env::var_os("NOSH_HOME").unwrap());
                thread::spawn(move || {
                    for revision in 1..=3 {
                        let marker = home.join(format!("theme-request-{revision}"));
                        let deadline = Instant::now() + Duration::from_secs(8);
                        while !marker.exists() {
                            assert!(
                                Instant::now() < deadline,
                                "theme fixture never received update {revision}"
                            );
                            thread::sleep(Duration::from_millis(5));
                        }
                        let theme = if revision == 3 {
                            nosh_shell::status::Theme::default()
                        } else {
                            nosh_shell::status::Theme {
                                environment: nosh_shell::status::ColorPair::new(
                                    [238, 243, 248],
                                    if revision == 1 {
                                        [30, 64, 83]
                                    } else {
                                        [41, 54, 70]
                                    },
                                    231,
                                    if revision == 1 { 24 } else { 25 },
                                )
                                .unwrap(),
                                ..Default::default()
                            }
                        };
                        assert!(themes.replace(theme));
                    }
                })
            });
            assert_eq!(
                nosh_shell::repl::run(&mut shell, &mut nosh_shell::repl::NoAi, config),
                if mode == "repl-blocked" { 130 } else { 0 }
            );
            if let Some(updates) = theme_updates {
                updates.join().unwrap();
            }
            if mode.starts_with("repl-inline") {
                assert!(
                    !directory.path().join("history_accepted").exists(),
                    "history acceptance executed input"
                );
                let executions =
                    match std::fs::read_to_string(directory.path().join("correction_executed")) {
                        Ok(text) => text.lines().count(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => 0,
                        Err(error) => panic!("cannot inspect correction execution count: {error}"),
                    };
                assert_eq!(
                    executions,
                    usize::from(mode == "repl-inline-correct-execute"),
                    "correction adoption executed a command"
                );
            }
        }
        _ => panic!("unknown probe"),
    }
    if let Some(saved) = original_stdout {
        std::io::stdout().flush().unwrap();
        assert_eq!(unsafe { libc::dup2(saved, 1) }, 1);
        assert_eq!(unsafe { libc::close(saved) }, 0);
        assert!(!crossterm::terminal::is_raw_mode_enabled().unwrap());
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
