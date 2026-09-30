use super::*;
use nosh_llm::{
    MockChatEngine,
    mock::{call, text},
};
use nosh_shell::ShellOptions;

fn generate(
    engine: &mut dyn ChatEngine,
    shell: &EmbeddedShell,
    text: &str,
    cfg: &AgentConfig,
) -> Result<AssistOutcome, AssistError> {
    super::generate(
        engine,
        shell,
        text,
        cfg,
        &mut crate::user_input::NoUserInput,
    )
}

fn run(
    engine: &mut dyn ChatEngine,
    request: &AssistRequest,
    cfg: &AgentConfig,
    cancel: &CancelHandle,
    deliver: impl FnOnce(&Result<AssistOutcome, AssistError>) -> bool,
) -> Result<AssistOutcome, AssistError> {
    super::run(
        engine,
        request,
        cfg,
        cancel,
        &mut crate::user_input::NoUserInput,
        deliver,
    )
}

fn shell(path: &Path) -> EmbeddedShell {
    EmbeddedShell::new(ShellOptions {
        working_dir: Some(path.into()),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn generate_direct_final_is_strict_and_never_executes_the_program() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    for program in [
        "touch not-created",
        "for f in *.txt; do\n  echo \"$f\"\ndone",
        "if test -d src; then\n echo yes\nelse\n echo no\nfi",
        "echo \"$(echo ok)\"",
        "{ f(){ g; }; g(){ echo ok; }; f; }",
    ] {
        let mut engine = MockChatEngine::new(vec![vec![text(program)]]);
        let outcome = generate(&mut engine, &shell, "generate", &AgentConfig::default()).unwrap();
        assert_eq!(outcome.result, AssistResult::Command(program.into()));
        assert!(!dir.path().join("not-created").exists());
    }
    for reply in [
        "",
        "```bash\necho ok\n```",
        "echo \u{202e}hidden",
        "echo \"$(nosh_missing_command)\"",
        "cat <(nosh_missing_command)",
        "for f in *; do echo \"$f\"",
        "echo ok\nThis prints ok.",
        "Which directory?",
    ] {
        let mut engine = MockChatEngine::new(vec![vec![text(reply)]]);
        assert!(matches!(
            generate(&mut engine, &shell, "generate", &AgentConfig::default()),
            Err(AssistError::Protocol(_))
        ));
    }
}

#[test]
fn generate_rejects_unavailable_tools_and_accepts_commands_or_none() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    for name in ["exec", "finish", "command_info"] {
        let mut engine =
            MockChatEngine::new(vec![vec![call(name, json!({}))], vec![text("[None]")]]);
        let error = generate(&mut engine, &shell, "generate", &AgentConfig::default()).unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown command-assistance tool")
        );
    }
    for (reply, expected) in [
        ("[None]", AssistResult::NoSuggestion),
        ("echo ready", AssistResult::Command("echo ready".into())),
    ] {
        let mut engine = MockChatEngine::new(vec![vec![text(reply)]]);
        let specs = engine.specs();
        let choices = engine.tool_choices();
        assert_eq!(
            generate(&mut engine, &shell, "generate", &AgentConfig::default())
                .unwrap()
                .result,
            expected
        );
        let specs = specs.lock().unwrap();
        assert!(
            specs[0]
                .tools
                .iter()
                .all(|tool| tool.name != "finish" && tool.name != "ask_user")
        );
        assert!(
            choices
                .lock()
                .unwrap()
                .iter()
                .all(|(_, choice)| *choice == nosh_llm::ToolChoice::Auto)
        );
    }
}

#[test]
fn query_results_are_used_without_executing_targets_or_reading_protected_files() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("secret"), "do-not-expose").unwrap();
    let shell = shell(dir.path());
    let cfg = AgentConfig {
        protected: vec![dir.path().join("secret")],
        ..Default::default()
    };
    let mut engine = MockChatEngine::new(vec![
        vec![call("command_help", json!({"name":"ls","query":"--help"}))],
        vec![call("read_file", json!({"path":"secret"}))],
        vec![call("exec", json!({"command":"touch forbidden"}))],
        vec![text("echo ok")],
    ]);
    let received = engine.received();
    let outcome = generate(&mut engine, &shell, "generate", &cfg).unwrap();
    assert_eq!(outcome.steps, 4);
    let records = received.lock().unwrap();
    let results = format!("{:?}", &records[1..]);
    assert!(results.contains("[command_help]"));
    assert!(results.contains("requires permission"));
    assert!(results.contains("unknown command-assistance tool"));
    assert!(!results.contains("do-not-expose"));
    assert!(!dir.path().join("forbidden").exists());
}

#[test]
fn failure_evidence_is_bound_and_automatic_requests_do_not_forge_users() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("sh -c 'exit 7'");
    let command = shell.recent_commands().last().unwrap().clone();
    let cfg = AgentConfig::default();
    let request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Fix,
        String::new(),
        Some(command.clone()),
        None,
    )
    .unwrap();
    let messages = request.messages().unwrap();
    assert!(messages.iter().all(|m| matches!(m, Message::System(_))));
    assert!(format!("{messages:?}").contains("command_id"));
    assert!(
        AssistRequest::capture(
            &shell,
            &cfg,
            Intent::Next,
            String::new(),
            Some(command),
            None
        )
        .is_err()
    );
    assert!(AssistRequest::capture(&shell, &cfg, Intent::Fix, String::new(), None, None).is_err());
}

#[test]
fn completion_context_is_a_suggestion_request_with_one_execution_identity() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("sh -c 'exit 7'");
    let command = shell.recent_commands().last().unwrap().clone();
    let mut output = shell.last_user_output().unwrap().clone();
    output.state = nosh_shell::OutputState::Captured;
    output.terminal_source = true;
    output.text = "recorded failure\n".into();
    output.observed_bytes = Some(output.text.len() as u64);
    let cfg = AgentConfig::default();
    let request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Fix,
        String::new(),
        Some(command.clone()),
        Some(output.clone()),
    )
    .unwrap();
    let messages = request.messages().unwrap();
    let [Message::System(background), Message::System(task)] = messages.as_slice() else {
        panic!("automatic assistance must retain host-originated system messages");
    };
    assert_eq!(background.matches(&command.line).count(), 1);
    assert_eq!(background.matches(command.cwd.to_str().unwrap()).count(), 1);
    let execution: serde_json::Value = serde_json::from_str(
        background
            .split_once("[execution]\n")
            .unwrap()
            .1
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(execution["execution_cwd"], ".");
    assert_eq!(execution["exit"], 7);
    assert_eq!(execution["status"], "failed");
    assert!(background.contains("recorded failure"));
    assert_eq!(background.matches("\"command_id\":").count(), 2);
    assert!(task.contains("intent: fix"));
    assert!(task.contains("Draft a command suggestion"));
    assert!(task.contains("current project state"));
    assert!(task.contains("will not be executed automatically"));
    assert!(!task.contains("grants no task-execution permission"));
    assert_eq!(request.output.as_ref().unwrap(), &output);

    for corrupt_command in [true, false] {
        let mut invalid = output.clone();
        if corrupt_command {
            invalid.command = "unrelated command".into();
        } else {
            invalid.command_truncated = true;
        }
        assert!(
            AssistRequest::capture(
                &shell,
                &cfg,
                Intent::Fix,
                String::new(),
                Some(command.clone()),
                Some(invalid),
            )
            .is_err()
        );
    }

    shell.run_user_line("true");
    let success = shell.recent_commands().last().unwrap().clone();
    let request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Next,
        String::new(),
        Some(success),
        None,
    )
    .unwrap();
    let messages = request.messages().unwrap();
    let Message::System(background) = &messages[0] else {
        panic!("missing context")
    };
    assert!(background.contains("\"exit\":0"));
    assert!(background.contains("\"status\":\"succeeded\""));
    assert!(!background.contains("[user_output "));
    let Message::System(task) = messages.last().unwrap() else {
        panic!("missing request")
    };
    assert!(task.contains("intent: next"));
    assert!(task.contains("Draft a command suggestion"));
}

#[test]
fn completion_context_keeps_a_distinct_execution_directory_after_cd() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("nested")).unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("cd nested && sh -c 'exit 7'");
    let command = shell.recent_commands().last().unwrap().clone();
    assert_ne!(command.cwd, shell.cwd());
    let request = AssistRequest::capture(
        &shell,
        &AgentConfig::default(),
        Intent::Fix,
        String::new(),
        Some(command.clone()),
        shell.last_user_output().cloned(),
    )
    .unwrap();
    let messages = request.messages().unwrap();
    let Message::System(background) = &messages[0] else {
        panic!("missing context")
    };
    assert!(background.contains(&format!("\ncwd: {}\n", shell.cwd().display())));
    let execution: serde_json::Value = serde_json::from_str(
        background
            .split_once("[execution]\n")
            .unwrap()
            .1
            .lines()
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(execution["execution_cwd"], command.cwd.to_str().unwrap());
    assert_eq!(execution["command"], command.line);
    assert!(background.contains("\"state\":\"not_captured\""));
    assert!(background.contains("No captured output is available"));
}

#[test]
fn environment_context_uses_the_captured_permission_snapshot() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("VIRTUAL_ENV=/tmp/env-one");
    let request = AssistRequest::capture(
        &shell,
        &AgentConfig::default(),
        Intent::Generate,
        "generate".into(),
        None,
        None,
    )
    .unwrap();
    shell.run_user_line("VIRTUAL_ENV=/tmp/env-two");
    let messages = format!("{:?}", request.messages().unwrap());
    assert!(messages.contains("venv: env-one"));
    assert!(!messages.contains("env-two"));
}

#[test]
fn generate_query_turns_allow_narration_and_read_only_batches() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let lookup = call("command_help", json!({"name":"ls","query":"--help"}));
    for query_turn in [
        vec![lookup.clone(), lookup.clone()],
        vec![text("checking"), lookup.clone()],
    ] {
        let mut engine = MockChatEngine::new(vec![query_turn, vec![text("echo ok")]]);
        let received = engine.received();
        assert_eq!(
            generate(&mut engine, &shell, "generate", &AgentConfig::default())
                .unwrap()
                .result,
            AssistResult::Command("echo ok".into())
        );
        assert!(format!("{:?}", received.lock().unwrap()).contains("[command_help]"));
    }
}

#[test]
fn invalid_generate_final_does_not_add_a_correction_round() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let mut engine = MockChatEngine::new(vec![vec![text("echo '")], vec![text("echo ok")]]);
    let received = engine.received();
    assert!(generate(&mut engine, &shell, "generate", &AgentConfig::default()).is_err());
    assert_eq!(received.lock().unwrap().len(), 1);
}

#[test]
fn cancellation_and_step_exhaustion_are_not_no_suggestion() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let cfg = AgentConfig {
        max_steps: 1,
        ..Default::default()
    };
    let request =
        AssistRequest::capture(&shell, &cfg, Intent::Generate, "command".into(), None, None)
            .unwrap();
    let cancel = CancelHandle::default();
    cancel.cancel();
    let mut engine = MockChatEngine::new(vec![vec![call(
        "command_help",
        json!({"name":"ls","query":"--help"}),
    )]]);
    assert!(matches!(
        run(&mut engine, &request, &cfg, &cancel, |_| true),
        Err(AssistError::Cancelled)
    ));
    cancel.reset();
    assert!(matches!(
        run(&mut engine, &request, &cfg, &cancel, |_| true),
        Err(AssistError::Budget)
    ));
}

#[test]
fn unavailable_query_is_not_reported_as_a_normal_empty_suggestion() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let mut engine = MockChatEngine::new(vec![
        vec![call("read_file", json!({"path":"missing"}))],
        vec![text("[None]")],
    ]);
    let error = generate(&mut engine, &shell, "generate", &AgentConfig::default()).unwrap_err();
    assert!(error.to_string().contains("failed query"));
}

#[test]
fn fix_and_next_use_direct_final_without_finish_or_execution() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let cfg = AgentConfig::default();
    for (intent, command) in [(Intent::Next, "true"), (Intent::Fix, "sh -c 'exit 7'")] {
        shell.run_user_line(command);
        let request = AssistRequest::capture(
            &shell,
            &cfg,
            intent,
            String::new(),
            shell.recent_commands().last().cloned(),
            None,
        )
        .unwrap();
        for (reply, expected) in [
            (" \t[None]\n", AssistResult::NoSuggestion),
            (
                "touch not-created",
                AssistResult::Command("touch not-created".into()),
            ),
            (
                "if true; then\n echo ready\nfi",
                AssistResult::Command("if true; then\n echo ready\nfi".into()),
            ),
        ] {
            let mut engine = MockChatEngine::new(vec![vec![text(reply)]]);
            let specs = engine.specs();
            let choices = engine.tool_choices();
            let observations = engine.observations();
            let outcome = run(
                &mut engine,
                &request,
                &cfg,
                &CancelHandle::default(),
                |_| true,
            )
            .unwrap();
            assert_eq!(outcome.result, expected);
            assert!(!dir.path().join("not-created").exists());
            let specs = specs.lock().unwrap();
            assert_eq!(
                specs[0]
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                ["command_help", "read_file", "grep"]
            );
            assert!(specs[0].system.contains("return exactly [None]"));
            assert!(!specs[0].system.contains("finish"));
            assert!(
                choices
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|(_, choice)| *choice == nosh_llm::ToolChoice::Auto)
            );
            assert_eq!(
                observations.lock().unwrap()[0].1["response_format"],
                "command_or_none"
            );
        }
    }
}

#[test]
fn direct_final_rejects_empty_prose_and_wrong_markers_without_clarifying() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("true");
    let cfg = AgentConfig::default();
    let request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Next,
        String::new(),
        shell.recent_commands().last().cloned(),
        None,
    )
    .unwrap();
    for reply in [
        "",
        " \n\t",
        "NONE",
        "[none]",
        "[None] explanation",
        "Which directory?",
        "Here is a command:\necho ready",
        "```bash\necho ready\n```",
        "echo \u{202e}hidden",
    ] {
        let mut engine = MockChatEngine::new(vec![vec![text(reply)]]);
        let observations = engine.observations();
        let outcome = run(
            &mut engine,
            &request,
            &cfg,
            &CancelHandle::default(),
            |_| true,
        );
        assert!(
            matches!(outcome, Err(AssistError::Protocol(_))),
            "{reply:?}: {outcome:?}"
        );
        let observations = observations.lock().unwrap();
        assert_eq!(observations[0].1["status"], "failed");
        assert!(observations[0].1.get("kind").is_none());
    }
}

#[test]
fn direct_query_turns_keep_narration_and_allow_bounded_read_only_batches() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("evidence.txt"), "observed\n").unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("true");
    let cfg = AgentConfig::default();
    let request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Next,
        String::new(),
        shell.recent_commands().last().cloned(),
        None,
    )
    .unwrap();
    let mut engine = MockChatEngine::new(vec![
        vec![
            text("Checking the available evidence."),
            call("read_file", json!({"path":"evidence.txt"})),
            call(
                "grep",
                json!({"path":".","pattern":"observed","glob":"*.txt"}),
            ),
        ],
        vec![text("echo ready")],
    ]);
    let received = engine.received();
    let choices = engine.tool_choices();
    let outcome = run(
        &mut engine,
        &request,
        &cfg,
        &CancelHandle::default(),
        |_| true,
    )
    .unwrap();
    assert_eq!(outcome.steps, 2);
    assert_eq!(outcome.result, AssistResult::Command("echo ready".into()));
    let received = received.lock().unwrap();
    assert!(matches!(&received[1][0], Message::Tool(body) if body.contains("observed")));
    assert!(
        matches!(&received[1][1], Message::Tool(body) if body.contains("evidence.txt") && body.contains("observed"))
    );
    assert!(received[1].last() == Some(&request.final_message(&[])));
    assert!(
        choices
            .lock()
            .unwrap()
            .iter()
            .all(|(_, choice)| *choice == nosh_llm::ToolChoice::Auto)
    );
}

#[test]
fn final_round_reuses_the_output_instruction_for_every_intent_and_budget() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("fact.txt"), "known\n").unwrap();
    let mut shell = shell(dir.path());
    for intent in [Intent::Generate, Intent::Fix, Intent::Next] {
        shell.run_user_line(if intent == Intent::Fix {
            "sh -c 'exit 7'"
        } else {
            "true"
        });
        for limit in [1, 2, 4, 8] {
            let cfg = AgentConfig {
                max_steps: limit,
                ..AgentConfig::default()
            };
            let request = AssistRequest::capture(
                &shell,
                &cfg,
                intent,
                "suggest a command".into(),
                (intent != Intent::Generate)
                    .then(|| shell.recent_commands().last().unwrap().clone()),
                None,
            )
            .unwrap();
            let steps = limit.min(if intent == Intent::Next { 2 } else { 4 });
            let mut replies = vec![vec![call("read_file", json!({"path":"fact.txt"}))]; steps - 1];
            replies.push(vec![text("echo ready")]);
            let mut engine = MockChatEngine::new(replies);
            let specs = engine.specs();
            let received = engine.received();
            let result = run(
                &mut engine,
                &request,
                &cfg,
                &CancelHandle::default(),
                |_| true,
            )
            .unwrap();
            assert_eq!(result.steps, steps);
            assert_eq!(result.result, AssistResult::Command("echo ready".into()));
            let specs = specs.lock().unwrap();
            assert!(specs[0].system.ends_with(FINAL_RESPONSE_RULE));
            assert!(specs[0].tools.iter().all(|tool| tool.name != "exec"));
            let received = received.lock().unwrap();
            let reminder = request.final_message(&[]);
            for (index, messages) in received.iter().enumerate() {
                assert_eq!(messages.contains(&reminder), index + 1 == steps);
            }
        }
    }
}

#[test]
fn final_reminder_quotes_actual_inputs_without_inventing_an_automatic_user_request() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let cfg = AgentConfig::default();
    for intent in [Intent::Generate, Intent::Fix, Intent::Next] {
        shell.run_user_line(if intent == Intent::Fix {
            "sh -c 'exit 7'"
        } else {
            "true"
        });
        let original = if intent == Intent::Generate {
            "Use \"logs\";\nkeep backups."
        } else {
            ""
        };
        let request = AssistRequest::capture(
            &shell,
            &cfg,
            intent,
            original.into(),
            (intent != Intent::Generate).then(|| shell.recent_commands().last().unwrap().clone()),
            None,
        )
        .unwrap();
        let Message::System(reminder) = request.final_message(&[]) else {
            unreachable!()
        };
        assert!(reminder.starts_with(intent.instruction()));
        assert!(reminder.ends_with(FINAL_RESPONSE_RULE));
        assert_eq!(
            reminder.contains("User request (quoted):"),
            intent == Intent::Generate
        );
        assert_eq!(
            reminder.contains("Recorded command (quoted):"),
            intent != Intent::Generate
        );
        assert!(!reminder.contains("Clarification"));
        assert!(!reminder.contains("Fulfill this request"));
        if !original.is_empty() {
            let quoted = reminder.lines().nth(2).unwrap();
            assert_eq!(serde_json::from_str::<String>(quoted).unwrap(), original);
        }
    }
}

#[test]
fn direct_final_cannot_turn_query_parse_cancel_or_budget_errors_into_none() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("true");
    let cfg = AgentConfig::default();
    let request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Next,
        String::new(),
        shell.recent_commands().last().cloned(),
        None,
    )
    .unwrap();
    let mut engine = MockChatEngine::new(vec![
        vec![call("read_file", json!({"path":"missing"}))],
        vec![text("[None]")],
    ]);
    assert!(
        run(
            &mut engine,
            &request,
            &cfg,
            &CancelHandle::default(),
            |_| true
        )
        .unwrap_err()
        .to_string()
        .contains("failed query")
    );
    let mut engine = MockChatEngine::new(vec![vec![
        text("[None]"),
        nosh_llm::mock::bad_call(nosh_llm::CallErrorKind::Malformed, "invalid call"),
    ]]);
    assert!(matches!(
        run(
            &mut engine,
            &request,
            &cfg,
            &CancelHandle::default(),
            |_| true
        ),
        Err(AssistError::Protocol(_))
    ));
    let cancel = CancelHandle::default();
    let model_cancel = cancel.clone();
    let mut engine = MockChatEngine::with_responder(move |_| {
        model_cancel.cancel();
        vec![text("[None]")]
    });
    assert!(matches!(
        run(&mut engine, &request, &cfg, &cancel, |_| true),
        Err(AssistError::Cancelled)
    ));
    let mut engine = MockChatEngine::with_responder(|_| {
        std::thread::sleep(Duration::from_millis(30));
        vec![text("[None]")]
    });
    let timed = AgentConfig {
        command_timeout: Duration::from_millis(10),
        ..cfg.clone()
    };
    assert!(matches!(
        run(
            &mut engine,
            &request,
            &timed,
            &CancelHandle::default(),
            |_| true
        ),
        Err(AssistError::Budget)
    ));
    let mut engine = MockChatEngine::new(vec![vec![call("read_file", json!({"path":"missing"}))]]);
    let received = engine.received();
    let limited = AgentConfig {
        max_steps: 1,
        ..cfg
    };
    assert!(matches!(
        run(
            &mut engine,
            &request,
            &limited,
            &CancelHandle::default(),
            |_| true
        ),
        Err(AssistError::Budget)
    ));
    assert_eq!(received.lock().unwrap().len(), 1);
}

#[test]
fn background_completion_is_recorded_only_after_current_version_publication() {
    use nosh_shell::{AssistDisplay, Assistance};
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("true");
    let cfg = AgentConfig::default();
    let mut request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Next,
        String::new(),
        shell.recent_commands().last().cloned(),
        None,
    )
    .unwrap();
    request.background = true;
    for stale in [false, true] {
        for (reply, kind) in [("echo ready", "command"), ("[None]", "none")] {
            let display = AssistDisplay::default();
            let version = display.invalidate();
            let mut engine = MockChatEngine::new(vec![vec![text(reply)]]);
            let observations = engine.observations();
            let outcome = run(
                &mut engine,
                &request,
                &cfg,
                &CancelHandle::default(),
                |result| {
                    assert!(result.is_ok());
                    assert!(
                        observations.lock().unwrap().is_empty(),
                        "no completion before delivery"
                    );
                    if stale {
                        display.invalidate();
                    }
                    let presentation = match &result.as_ref().unwrap().result {
                        AssistResult::Command(program) => {
                            Some(Assistance::Message(program.clone()))
                        }
                        AssistResult::NoSuggestion => None,
                    };
                    display.publish(version, presentation)
                },
            );
            let observed = observations.lock().unwrap();
            assert_eq!(observed.len(), 1);
            let value = &observed[0].1;
            assert_eq!(value["response_format"], "command_or_none");
            if stale {
                assert!(matches!(outcome, Err(AssistError::Cancelled)));
                assert!(display.result().is_none());
                assert_eq!(value["status"], "cancelled");
                assert!(value.get("kind").is_none());
            } else {
                assert!(outcome.is_ok());
                assert_eq!(value["status"], "completed");
                assert_eq!(value["kind"], kind);
            }
        }
    }
}

#[test]
fn help_queries_do_not_execute_workspace_programs_or_functions() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("tar");
    std::fs::write(&fake, "#!/bin/sh\ntouch ran\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut shell = shell(dir.path());
    shell.run_user_line("f(){ touch ran; }");
    let cfg = AgentConfig::default();
    let request = AssistRequest::capture(
        &shell,
        &cfg,
        Intent::Generate,
        "generate".into(),
        None,
        None,
    )
    .unwrap();
    for name in ["./tar", "f"] {
        let tool = ToolCall {
            name: "command_help".into(),
            args: json!({"name":name}).as_object().unwrap().clone(),
        };
        assert!(query(&request, &cfg, &tool, &CancelHandle::default()).is_err());
    }

    assert!(!dir.path().join("ran").exists());
    let tool = ToolCall {
        name: "command_help".into(),
        args: json!({"name":"ls"}).as_object().unwrap().clone(),
    };
    let result = query(&request, &cfg, &tool, &CancelHandle::default()).unwrap();
    assert!(result.contains("Usage:") || result.contains("usage:"));
    assert_eq!(
        nosh_permissions::assess_command("tar --help", &request.context).risk(),
        Risk::Safe
    );
    assert!(
        nosh_permissions::assess_command("tar --help -cf archive .", &request.context).risk()
            > Risk::Safe
    );
}

#[test]
fn help_tool_is_shared_by_all_intents_and_relevant_evidence_reaches_direct_final() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = shell(dir.path());
    let cfg = AgentConfig::default();
    for (intent, line) in [
        (Intent::Generate, "true"),
        (Intent::Fix, "sh -c 'exit 7'"),
        (Intent::Next, "true"),
    ] {
        shell.run_user_line(line);
        let request = AssistRequest::capture(
            &shell,
            &cfg,
            intent,
            String::new(),
            (intent != Intent::Generate).then(|| shell.recent_commands().last().unwrap().clone()),
            None,
        )
        .unwrap();
        let mut engine = MockChatEngine::new(vec![
            vec![call("command_help", json!({"name":"ls","query":"--help"}))],
            vec![text("echo ready")],
        ]);
        let received = engine.received();
        let opened = engine.specs();
        assert_eq!(
            run(
                &mut engine,
                &request,
                &cfg,
                &CancelHandle::default(),
                |_| true
            )
            .unwrap()
            .result,
            AssistResult::Command("echo ready".into())
        );
        assert!(
            matches!(&received.lock().unwrap()[1][0], Message::Tool(body) if body.starts_with("[command_help]\n") && body.contains("\"query\":\"--help\"") && !body.contains("\"topic\""))
        );
        assert_eq!(opened.lock().unwrap()[0].tools, specs(false));
        let unavailable = ToolCall {
            name: "command_info".into(),
            args: json!({"name":"ls","query":"help"})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert!(
            query(&request, &cfg, &unavailable, &CancelHandle::default())
                .unwrap_err()
                .contains("unknown command-assistance tool")
        );
    }
}
