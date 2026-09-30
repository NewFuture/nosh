use super::support::*;

#[test]
fn user_output_is_attached_once_per_conversation() {
    let _g = setup();
    let mut sh = shell();
    assert_eq!(sh.run_user_line("sh -c 'exit 17'").exit_code, 17);
    let command = sh.recent_commands().last().unwrap().clone();
    let mut output = sh.last_user_output().unwrap().clone();
    output.state = nosh_shell::OutputState::Captured;
    output.terminal_source = true;
    output.text = "REGION is unset\n".into();
    output.observed_bytes = Some(output.text.len() as u64);

    let engine = MockChatEngine::with_responder(|_| vec![text("done")]);
    let received = engine.received();
    let mut agent = agent(engine, AgentConfig::default());
    let mut input = TaskInput::new(Trigger::Failed { exit: 17 }, "");
    input.failed = Some(command);
    input.user_output = Some(output);
    let mut ui = RecordUi::default();

    for _ in 0..2 {
        let result = agent.run_task(&mut sh, input.clone(), &mut Scripted::new([]), &mut ui);
        assert_eq!(result.status, TaskStatus::Completed);
    }
    agent.reset_conversation();
    let result = agent.run_task(&mut sh, input, &mut Scripted::new([]), &mut ui);
    assert_eq!(result.status, TaskStatus::Completed);

    let received = received.lock().unwrap();
    let task = |index: usize| match &received[index][0] {
        Message::System(text) => text,
        other => panic!("expected system context, got {other:?}"),
    };
    assert!(task(0).contains("[user_output "));
    assert!(!task(1).contains("[user_output "));
    assert!(task(2).contains("[user_output "));
    for messages in received.iter() {
        let [Message::System(_), Message::User(request)] = messages.as_slice() else {
            panic!("captured output must remain separate from the real request");
        };
        assert_eq!(request, "Explain why the command failed and how to fix it.");
    }
}

#[test]
fn project_context_refreshes_between_tasks_and_after_agent_cd() {
    let _g = setup();
    let root = tmpdir("project-context");
    let rust = root.join("rust");
    let node = root.join("node");
    let plain = root.join("plain");
    for dir in [&rust, &node, &plain] {
        std::fs::create_dir_all(dir).unwrap();
    }
    std::fs::write(rust.join("Cargo.toml"), "[package]\nname='rust-project'\n").unwrap();
    std::fs::write(
        node.join("package.json"),
        r#"{"name":"node-project","scripts":{"build":"must-not-run"}}"#,
    )
    .unwrap();
    std::fs::write(rust.join("AGENTS.md"), "Rust scoped instruction.").unwrap();
    std::fs::write(node.join("README.md"), "Node project reference.").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", rust.display()));
    let destination = node.clone();
    let engine = MockChatEngine::with_responder(move |history| match history.last() {
        Some(Message::User(text)) if text == "change project" => {
            vec![call(
                "exec",
                json!({"command": format!("cd {}", destination.display())}),
            )]
        }
        _ => vec![text("Context received.")],
    });
    let received = engine.received();
    let specs = engine.specs();
    let mut agent = agent(engine, AgentConfig::default());
    for request in ["describe", "change project", "describe"] {
        let result = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, request),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
        assert_eq!(result.status, TaskStatus::Completed);
    }
    sh.run_user_line(&format!("cd {}", plain.display()));
    agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "describe"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    let records = received.lock().unwrap();
    let contexts: Vec<_> = records
        .iter()
        .flatten()
        .filter_map(|message| {
            if let Message::System(text) = message
                && text.starts_with("[context]\n")
            {
                Some(text.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(contexts.len(), 5);
    assert!(contexts[0].contains("[AGENTS.md \"AGENTS.md\"]"));
    assert!(contexts[0].contains("Rust scoped instruction."));
    assert!(!contexts[1].contains("Rust scoped instruction."));
    assert!(contexts[2].contains("[project documents cleared]"));
    assert!(contexts[2].contains("[README reference \"README.md\"]"));
    assert!(contexts[2].contains("Node project reference."));
    assert!(!contexts[2].contains("Rust scoped instruction."));
    assert!(!contexts[2].contains(&node.join("README.md").display().to_string()));
    assert!(!contexts[3].contains("Node project reference."));
    assert!(contexts[4].contains("[project documents cleared]"));
    assert!(
        context_field(contexts[0], "project")
            .unwrap()
            .starts_with("rust;")
    );
    assert!(
        context_field(contexts[2], "project")
            .unwrap()
            .starts_with("node;")
    );
    assert_eq!(
        context_field(contexts[2], "cwd"),
        Some(node.to_str().unwrap())
    );
    assert!(context_field(contexts[2], "project_root").is_none());
    assert_eq!(
        context_field(contexts[4], "project"),
        Some("no known manifest")
    );
    assert_eq!(context_field(contexts[4], "git"), Some("none detected"));
    let results = tool_results(&records);
    assert!(!results[0].contains("[context]"));
    assert!(
        context_field(contexts[3], "project")
            .unwrap()
            .starts_with("node;")
    );
    let [Message::Tool(_), Message::System(updated)] = records[2].as_slice() else {
        panic!("cwd changes must append system context after tool results");
    };
    assert_eq!(context_field(updated, "cwd"), Some(node.to_str().unwrap()));
    assert_eq!(
        specs.lock().unwrap().len(),
        1,
        "project changes must not replace the system prefix"
    );
    drop(records);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn automatic_project_context_honors_custom_protection_in_agent_and_suggestions() {
    use nosh_shell::AiHandler;
    let _g = setup();
    let root = tmpdir("protected-project-context");
    let manifest = root.join("package.json");
    std::fs::write(&manifest, r#"{"name":"never-expose-this-package-name"}"#).unwrap();
    let cfg = AgentConfig {
        protected: vec![manifest],
        ..AgentConfig::default()
    };
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", root.display()));
    let engine = MockChatEngine::new(vec![vec![text("ok")]]);
    let received = engine.received();
    let mut a = agent(engine, cfg.clone());
    a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "describe"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    let check = |messages: &[Vec<Message>]| {
        let Message::System(message) = &messages[0][0] else {
            panic!("expected task")
        };
        assert!(
            context_field(message, "project")
                .unwrap()
                .contains("metadata=unavailable")
        );
        assert!(!message.contains("never-expose-this-package-name"));
    };
    check(&received.lock().unwrap());

    let mut engine = MockChatEngine::new(vec![vec![text("echo ok")]]);
    let received = engine.received();
    let suggestion = generate(&mut engine, &sh, "suggest", &cfg, &mut NoUserInput).unwrap();
    assert_eq!(suggestion.result, AssistResult::Command("echo ok".into()));
    check(&received.lock().unwrap());

    let engine = MockChatEngine::new(vec![vec![text("echo ok")]]);
    let received = engine.received();
    let mut engine = Some(engine);
    let mut ai = ShellAi::new(
        Box::new(move |_| {
            Ok(nosh_core::LoadedEngine {
                engine: Box::new(engine.take().unwrap()),
                description: "mock".into(),
            })
        }),
        cfg,
        Box::new(Scripted::new([])),
    );
    assert_eq!(ai.suggest(&mut sh, "suggest").as_deref(), Some("echo ok"));
    check(&received.lock().unwrap());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn agents_guidance_is_scoped_cached_refreshed_and_cleared() {
    let _g = setup();
    let root = tmpdir("agents-guidance");
    let repo = root.join("repo");
    let outside = root.join("outside");
    std::fs::create_dir_all(repo.join(".git")).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(repo.join("AGENTS.md"), "first scoped instruction").unwrap();
    std::fs::write(repo.join("NOSH.md"), "must-not-load-legacy").unwrap();
    std::fs::write(repo.join("README.md"), "must-not-load-readme").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", repo.display()));
    let engine = MockChatEngine::with_responder(|_| vec![text("ok")]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    let run = |a: &mut Agent, sh: &mut EmbeddedShell| {
        a.run_task(
            sh,
            TaskInput::new(Trigger::Hash, "describe"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        )
    };
    assert_eq!(run(&mut a, &mut sh).status, TaskStatus::Completed);
    run(&mut a, &mut sh);
    std::fs::write(repo.join("AGENTS.md"), "newer scoped instruction").unwrap();
    run(&mut a, &mut sh);
    sh.run_user_line(&format!("cd {}", outside.display()));
    run(&mut a, &mut sh);
    sh.run_user_line(&format!("cd {}", repo.display()));
    a.reset_conversation();
    run(&mut a, &mut sh);
    let records = received.lock().unwrap();
    let messages: Vec<_> = records
        .iter()
        .flatten()
        .filter_map(|message| match message {
            Message::System(text) if text.starts_with("[context]\n") => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(messages[0].contains("first scoped instruction"));
    assert!(!messages[1].contains("first scoped instruction"));
    assert!(messages[2].contains("newer scoped instruction"));
    assert!(messages[3].contains("[project documents cleared]"));
    assert!(messages[4].contains("newer scoped instruction"));
    for message in messages {
        assert!(!message.contains("must-not-load-legacy"));
        assert!(!message.contains("must-not-load-readme"));
    }
    drop(records);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn incomplete_guidance_does_not_suppress_its_restored_version() {
    let _g = setup();
    let root = tmpdir("restored-guidance");
    std::fs::create_dir(root.join(".git")).unwrap();
    let source = root.join("AGENTS.md");
    std::fs::write(&source, "Unchanged scoped instruction.").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", root.display()));
    let engine = MockChatEngine::with_responder(|_| vec![text("ok")]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    for protected in [false, true, false, false] {
        a.cfg.protected = if protected {
            vec![source.clone()]
        } else {
            vec![]
        };
        a.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, "describe"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
    }
    let records = received.lock().unwrap();
    let messages: Vec<_> = records
        .iter()
        .flatten()
        .filter_map(|message| match message {
            Message::System(text) if text.starts_with("[context]\n") => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(messages.len(), 4);
    assert!(messages[0].contains("Unchanged scoped instruction."));
    assert!(messages[1].contains("[project documents cleared]"));
    assert!(messages[1].contains("guidance unavailable"));
    assert!(!messages[1].contains("Unchanged scoped instruction."));
    assert!(messages[2].contains("Unchanged scoped instruction."));
    assert!(!messages[3].contains("Unchanged scoped instruction."));
    drop(records);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn cancelled_generation_resends_guidance_on_the_next_task() {
    use nosh_llm::{CancelHandle, ChatEngine};
    use std::sync::{Arc, OnceLock};

    let _g = setup();
    let root = tmpdir("cancelled-guidance");
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(
        root.join("AGENTS.md"),
        "scoped instruction after cancellation",
    )
    .unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", root.display()));
    let cancel: Arc<OnceLock<CancelHandle>> = Arc::default();
    let handle = Arc::clone(&cancel);
    let mut first = true;
    let engine = MockChatEngine::with_responder(move |_| {
        if std::mem::take(&mut first) {
            handle.get().unwrap().cancel();
        }
        vec![text("ok")]
    });
    assert!(cancel.set(engine.cancel_handle()).is_ok());
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    for status in [TaskStatus::Cancelled, TaskStatus::Completed] {
        let outcome = a.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, "describe"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
        assert_eq!(outcome.status, status);
    }
    let received = received.lock().unwrap();
    assert_eq!(received.len(), 2);
    for append in received.iter() {
        let Message::System(message) = &append[0] else {
            panic!("expected task with scoped guidance");
        };
        assert!(message.contains("scoped instruction after cancellation"));
    }
    drop(received);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn suggestions_do_not_guess_when_agents_guidance_cannot_be_loaded() {
    let _g = setup();
    let root = tmpdir("blocked-agents");
    std::fs::create_dir_all(root.join(".git")).unwrap();
    let path = root.join("AGENTS.md");
    std::fs::write(&path, "protected instruction content").unwrap();
    std::fs::write(root.join("README.md"), "must-not-fall-back").unwrap();
    let cfg = AgentConfig {
        protected: vec![path],
        ..AgentConfig::default()
    };
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", root.display()));
    let mut engine = MockChatEngine::new(vec![vec![text("echo should-not-be-generated")]]);
    let specs = engine.specs();
    let result = generate(&mut engine, &sh, "suggest", &cfg, &mut NoUserInput);
    let error = result.unwrap_err().to_string();
    assert!(error.contains("AGENTS.md guidance is incomplete"));
    assert!(!error.contains("protected instruction content"));
    assert!(!error.contains("must-not-fall-back"));
    assert!(
        specs.lock().unwrap().is_empty(),
        "no model session opens with incomplete guidance"
    );
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn readme_references_refresh_and_remain_optional_for_suggestions() {
    let _g = setup();
    let root = tmpdir("readme-reference");
    std::fs::create_dir(root.join(".git")).unwrap();
    let readme = root.join("README.md");
    std::fs::write(&readme, "# Project\nFirst reference.\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", root.display()));
    let engine = MockChatEngine::with_responder(|_| vec![text("ok")]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    for index in 0..5 {
        if index == 2 {
            std::fs::write(&readme, "# Project\nUpdated reference.\n").unwrap();
        } else if index == 3 {
            std::fs::write(root.join("AGENTS.md"), "New scoped instruction.").unwrap();
        } else if index == 4 {
            std::fs::remove_file(root.join("AGENTS.md")).unwrap();
        }
        let result = a.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, "describe"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
        assert_eq!(result.status, TaskStatus::Completed);
    }
    let received = received.lock().unwrap();
    let messages: Vec<_> = received
        .iter()
        .flatten()
        .filter_map(|message| match message {
            Message::System(text) if text.starts_with("[context]\n") => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(messages[0].contains("First reference."));
    assert!(!messages[0].contains("[project documents cleared]"));
    assert!(!messages[1].contains("README reference"));
    assert!(messages[2].contains("Updated reference."));
    assert!(messages[3].contains("New scoped instruction."));
    assert!(!messages[3].contains("README reference"));
    assert!(messages[4].contains("[project documents cleared]"));
    assert!(messages[4].contains("Updated reference."));
    drop(received);

    for protected in [vec![], vec![readme.clone()]] {
        let blocked = !protected.is_empty();
        let cfg = AgentConfig {
            protected,
            ..AgentConfig::default()
        };
        let mut engine = MockChatEngine::new(vec![vec![text("echo ok")]]);
        let received = engine.received();
        let result = generate(&mut engine, &sh, "suggest", &cfg, &mut NoUserInput).unwrap();
        assert_eq!(result.result, AssistResult::Command("echo ok".into()));
        let received = received.lock().unwrap();
        let Message::System(message) = &received[0][0] else {
            panic!("expected reference context in the task");
        };
        assert_eq!(message.contains("reference unavailable"), blocked);
        assert_eq!(message.contains("Updated reference."), !blocked);
    }
    std::fs::remove_dir_all(root).unwrap();
}
