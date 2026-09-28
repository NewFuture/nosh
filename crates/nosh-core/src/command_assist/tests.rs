use super::*;
use nosh_llm::{
    MockChatEngine,
    mock::{call, text},
};
use nosh_shell::ShellOptions;

fn shell(path: &Path) -> EmbeddedShell {
    EmbeddedShell::new(ShellOptions {
        working_dir: Some(path.into()),
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn finish_is_typed_strict_and_never_executes_the_program() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    for program in [
        "touch not-created",
        "for f in *.txt; do\n  echo \"$f\"\ndone",
        "if test -d src; then\n echo yes\nelse\n echo no\nfi",
        "echo \"$(echo ok)\"",
        "{ f(){ g; }; g(){ echo ok; }; f; }",
    ] {
        let mut engine = MockChatEngine::new(vec![vec![call(
            "finish",
            json!({"kind":"command","text":program}),
        )]]);
        let outcome = generate(&mut engine, &shell, "generate", &AgentConfig::default()).unwrap();
        assert_eq!(outcome.result, AssistResult::Command(program.into()));
        assert!(!dir.path().join("not-created").exists());
    }
    for args in [
        json!({"kind":"command","text":""}),
        json!({"kind":"command","text":"```bash\necho ok\n```"}),
        json!({"kind":"command","text":"echo \u{202e}hidden"}),
        json!({"kind":"command","text":"echo \"$(nosh_missing_command)\""}),
        json!({"kind":"command","text":"cat <(nosh_missing_command)"}),
        json!({"kind":"command","text":"for f in *; do echo \"$f\""}),
        json!({"kind":"command","text":"echo ok\nThis prints ok."}),
        json!({"kind":"command","text":"echo ok","execute":true}),
        json!({"kind":"none","text":"error"}),
        json!({"kind":"clarify","text":13}),
        json!({"kind":"unknown"}),
    ] {
        let mut engine = MockChatEngine::new(vec![vec![call("finish", args)]]);
        assert!(matches!(
            generate(&mut engine, &shell, "generate", &AgentConfig::default()),
            Err(AssistError::Protocol(_))
        ));
    }
}

#[test]
fn plain_text_and_mixed_finish_are_not_success_shaped_fallbacks() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    for events in [
        vec![text("echo ok")],
        vec![
            text("Here is your command"),
            call("finish", json!({"kind":"command","text":"echo ok"})),
        ],
        vec![
            call("read_file", json!({"path":"missing"})),
            call("finish", json!({"kind":"none"})),
        ],
        vec![
            call("finish", json!({"kind":"none"})),
            call("finish", json!({"kind":"none"})),
        ],
    ] {
        let mut engine = MockChatEngine::new(vec![events]);
        assert!(generate(&mut engine, &shell, "generate", &AgentConfig::default()).is_err());
    }
    for (args, expected) in [
        (json!({"kind":"none"}), AssistResult::NoSuggestion),
        (
            json!({"kind":"clarify","text":"Which directory?"}),
            AssistResult::Clarify("Which directory?".into()),
        ),
    ] {
        let mut engine = MockChatEngine::new(vec![vec![call("finish", args)]]);
        assert_eq!(
            generate(&mut engine, &shell, "generate", &AgentConfig::default())
                .unwrap()
                .result,
            expected
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
        vec![call(
            "command_info",
            json!({"name":"echo","query":"resolve"}),
        )],
        vec![call("read_file", json!({"path":"secret"}))],
        vec![call("run_command", json!({"command":"touch forbidden"}))],
        vec![call("finish", json!({"kind":"command","text":"echo ok"}))],
    ]);
    let received = engine.received();
    let outcome = generate(&mut engine, &shell, "generate", &cfg).unwrap();
    assert_eq!(outcome.steps, 4);
    let records = received.lock().unwrap();
    let results = format!("{:?}", &records[1..]);
    assert!(results.contains("builtin"));
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
fn batches_and_queries_after_finish_correction_are_not_executed() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let lookup = call("command_info", json!({"name":"echo","query":"resolve"}));
    for turns in [
        vec![vec![lookup.clone(), lookup.clone()]],
        vec![vec![text("checking"), lookup.clone()]],
        vec![
            vec![call(
                "finish",
                json!({"kind":"command","text":"not a command"}),
            )],
            vec![lookup.clone()],
        ],
    ] {
        let mut engine = MockChatEngine::new(turns);
        let received = engine.received();
        assert!(matches!(
            generate(&mut engine, &shell, "generate", &AgentConfig::default()),
            Err(AssistError::Protocol(_))
        ));
        assert!(!format!("{:?}", received.lock().unwrap()).contains("\"builtin\""));
    }
}

#[test]
fn rejected_finish_can_be_corrected_once_within_the_original_budget() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let mut engine = MockChatEngine::new(vec![
        vec![call(
            "finish",
            json!({"kind":"command", "text":"Which directory?"}),
        )],
        vec![call(
            "finish",
            json!({"kind":"clarify", "text":"Which directory?"}),
        )],
    ]);
    let outcome = generate(&mut engine, &shell, "generate", &AgentConfig::default()).unwrap();
    assert_eq!(outcome.steps, 2);
    assert_eq!(
        outcome.result,
        AssistResult::Clarify("Which directory?".into())
    );
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
        "command_info",
        json!({"name":"echo","query":"resolve"}),
    )]]);
    assert!(matches!(
        run(&mut engine, &request, &cfg, &cancel),
        Err(AssistError::Cancelled)
    ));
    cancel.reset();
    assert!(matches!(
        run(&mut engine, &request, &cfg, &cancel),
        Err(AssistError::Budget)
    ));
}

#[test]
fn unavailable_query_is_not_reported_as_a_normal_empty_suggestion() {
    let dir = tempfile::tempdir().unwrap();
    let shell = shell(dir.path());
    let mut engine = MockChatEngine::new(vec![
        vec![call("read_file", json!({"path":"missing"}))],
        vec![call("finish", json!({"kind":"none"}))],
    ]);
    let error = generate(&mut engine, &shell, "generate", &AgentConfig::default()).unwrap_err();
    assert!(error.to_string().contains("failed query"));
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
            name: "command_info".into(),
            args: json!({"name":name,"query":"help"})
                .as_object()
                .unwrap()
                .clone(),
        };
        assert!(query(&request, &cfg, &tool, &CancelHandle::default()).is_err());
    }
    assert!(!dir.path().join("ran").exists());
    let tool = ToolCall {
        name: "command_info".into(),
        args: json!({"name":"ls","query":"help"})
            .as_object()
            .unwrap()
            .clone(),
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
