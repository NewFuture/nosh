use super::support::*;

#[test]
fn mutating_needs_approval_and_denial_reason_reaches_model() {
    let _g = setup();
    let dir = tmpdir("deny");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("run_command", json!({"command": "touch created.txt"}))],
        vec![text("OK, I will not create it.")],
    ]);
    let received = engine.received();
    let mut a = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..AgentConfig::default()
        },
    );
    let mut approval = Scripted::new([ApprovalResponse::Deny {
        reason: Some("not now".into()),
    }]);
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "create a file"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(approval.seen.len(), 1);
    assert_eq!(approval.seen[0].risk, Risk::Mutating);
    assert!(!approval.seen[0].strong);
    assert!(!dir.join("created.txt").exists());
    assert_eq!(out.denied, 1);
    assert_eq!(out.status, TaskStatus::Incomplete);
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].contains("[denied by user]") && results[0].contains("not now"));

    // Approved this time: the file is created in the shared session's cwd.
    let engine = MockChatEngine::new(vec![
        vec![call("run_command", json!({"command": "touch created.txt"}))],
        vec![text("Created.")],
    ]);
    let mut a = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..AgentConfig::default()
        },
    );
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "create a file"),
        &mut Scripted::new([ApprovalResponse::Approve]),
        &mut RecordUi::default(),
    );
    assert_eq!(out.status, TaskStatus::Completed);
    assert!(dir.join("created.txt").exists());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn dangerous_requires_strong_confirmation_and_edit_is_reassessed() {
    let _g = setup();
    let dir = tmpdir("danger");
    std::fs::create_dir_all(dir.join("build")).unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("run_command", json!({"command": "rm -rf build"}))],
        vec![text("Listed instead.")],
    ]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    let mut approval = Scripted::new([ApprovalResponse::Edit("ls -d build".into())]);
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "clean up"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(
        approval.seen.len(),
        1,
        "the edited command is Safe: no second prompt"
    );
    assert_eq!(approval.seen[0].risk, Risk::Dangerous);
    assert!(approval.seen[0].strong);
    assert!(dir.join("build").exists());
    assert_eq!(out.status, TaskStatus::Completed);
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].contains("the user edited the command to: ls -d build"));
    assert!(results[0].contains("build\n"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn session_protection_blocks_exec_and_exit() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![
        vec![
            call("run_command", json!({"command": "exec bash"})),
            call("run_command", json!({"command": "echo skipped"})),
        ],
        vec![call("run_command", json!({"command": "exit 3"}))],
        vec![text("I cannot do that.")],
    ]);
    let received = engine.received();
    let mut a = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Yolo,
            ..AgentConfig::default()
        },
    );
    let mut approval = Scripted::new([]);
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "replace the shell"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert!(
        approval.seen.is_empty(),
        "Forbidden is denied without asking"
    );
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].contains("[denied by policy]") && results[0].contains("exec"));
    assert!(results[1].starts_with("[skipped]"));
    assert!(results[2].contains("[denied by policy]"));
    assert_eq!(out.denied, 2);
    // The session is intact.
    let r = sh
        .run_agent_command("echo alive", &Default::default(), &mut nosh_shell::NullSink)
        .unwrap();
    assert_eq!(r.stdout, "alive\n");
}

#[test]
fn no_terminal_denies_unless_auto_allows() {
    let _g = setup();
    let dir = tmpdir("notty");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    sh.set_workspace(dir.clone());
    let turns = || {
        vec![
            vec![call("run_command", json!({"command": "mkdir made"}))],
            vec![text("done")],
        ]
    };
    let mut a = agent(
        MockChatEngine::new(turns()),
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..AgentConfig::default()
        },
    );
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Cli, "make a dir"),
        &mut NoTerminal,
        &mut RecordUi::default(),
    );
    assert_eq!(out.denied, 1);
    assert!(!dir.join("made").exists());
    let mut a = agent(
        MockChatEngine::new(turns()),
        AgentConfig {
            mode: ApprovalMode::Auto,
            ..AgentConfig::default()
        },
    );
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Cli, "make a dir"),
        &mut NoTerminal,
        &mut RecordUi::default(),
    );
    assert_eq!(out.status, TaskStatus::Completed);
    assert!(dir.join("made").is_dir(), "auto allows workspace writes");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn reads_through_dotdot_or_symlinks_still_ask() {
    let _g = setup();
    let dir = tmpdir("dotdot");
    std::os::unix::fs::symlink("/etc/hostname", dir.join("host")).unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let up = "../".repeat(dir.components().count());
    let engine = MockChatEngine::new(vec![
        vec![call(
            "read_file",
            json!({"path": format!("{up}etc/hostname")}),
        )],
        vec![call("read_file", json!({"path": "host"}))],
        vec![call(
            "run_command",
            json!({"command": "rm -rf ~/x #\u{202e} sl"}),
        )],
        vec![text("ok")],
    ]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    let mut approval = Scripted::new([
        ApprovalResponse::Deny { reason: None },
        ApprovalResponse::Deny { reason: None },
    ]);
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "read the host name"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(
        approval.seen.len(),
        3,
        "both protected reads and the hidden-character command require approval"
    );
    assert_eq!(approval.seen[0].command, "read_file /etc/hostname");
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].contains("[denied by user]"), "{}", results[0]);
    assert!(results[1].contains("[denied by user]"), "{}", results[1]);
    assert!(results[2].contains("[denied by user]"), "{}", results[2]);
    assert_eq!(out.proposed, None);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn grep_checks_protected_roots_dotdot_symlinks_and_descendants() {
    let _g = setup();
    let dir = tmpdir("grep-protected");
    std::fs::create_dir(dir.join("private")).unwrap();
    std::fs::create_dir(dir.join("public")).unwrap();
    std::fs::write(dir.join("private/secret"), "needle").unwrap();
    std::fs::write(dir.join("private/second"), "needle again").unwrap();
    std::os::unix::fs::symlink(dir.join("private/secret"), dir.join("link")).unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    for path in ["public/../private/secret", "link", "."] {
        let engine = MockChatEngine::new(vec![
            vec![call("grep", json!({"pattern": "needle", "path": path}))],
            vec![text("Denied.")],
        ]);
        let received = engine.received();
        let mut a = Agent::new(
            Box::new(engine),
            AgentConfig {
                protected: vec![dir.join("private")],
                ..Default::default()
            },
            env(),
            ToolSet::ReadOnly,
        );
        let mut approval = Scripted::new([ApprovalResponse::Deny { reason: None }]);
        let outcome = a.run_task(
            &mut sh,
            TaskInput::new(Trigger::Pipe, "find text"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(outcome.denied, 1);
        assert_eq!(approval.seen.len(), 1, "{path}");
        assert_eq!(approval.seen[0].tool, "grep");
        let results = tool_results(&received.lock().unwrap());
        assert!(results[0].contains("[denied by user]"), "{results:?}");
        assert!(!results[0].contains("needle"), "{results:?}");
    }
    let engine = MockChatEngine::new(vec![
        vec![call(
            "grep",
            json!({"pattern": "needle", "path": "private"}),
        )],
        vec![text("Found.")],
    ]);
    let received = engine.received();
    let mut a = Agent::new(
        Box::new(engine),
        AgentConfig {
            protected: vec![dir.join("private")],
            ..Default::default()
        },
        env(),
        ToolSet::ReadOnly,
    );
    let mut approval = Scripted::new([ApprovalResponse::Approve]);
    a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Pipe, "find text"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(
        approval.seen.len(),
        1,
        "one approval covers the explicitly requested protected root"
    );
    assert!(tool_results(&received.lock().unwrap())[0].contains("secret:1:needle"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn grep_metadata_filtering_does_not_grant_content_access() {
    let _g = setup();
    let dir = tmpdir("grep-ignore-permissions");
    let private = dir.join("private");
    std::fs::create_dir(&private).unwrap();
    std::fs::write(private.join("one"), "needle\n").unwrap();
    std::fs::write(private.join("two"), "needle\n").unwrap();
    std::fs::write(dir.join(".gitignore"), "ignored\n").unwrap();
    let external = dir.join("external-ignore");
    std::fs::write(&external, "*\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    for (path, protected, answers, expected_approvals, denied) in [
        (".", vec![dir.join(".gitignore")], vec![], 0, false),
        (
            ".",
            vec![private.clone()],
            vec![ApprovalResponse::Approve],
            1,
            false,
        ),
        (
            ".gitignore",
            vec![dir.join(".gitignore")],
            vec![ApprovalResponse::Deny { reason: None }],
            1,
            true,
        ),
        (
            "private/.gitignore",
            vec![private.clone(), external.clone()],
            vec![ApprovalResponse::Deny { reason: None }],
            1,
            true,
        ),
    ] {
        if path == "private/.gitignore" {
            std::os::unix::fs::symlink(&external, private.join(".gitignore")).unwrap();
        }
        let engine = MockChatEngine::new(vec![
            vec![call("grep", json!({"pattern": "needle", "path": path}))],
            vec![text("done")],
        ]);
        let received = engine.received();
        let mut agent = Agent::new(
            Box::new(engine),
            AgentConfig {
                protected,
                ..Default::default()
            },
            env(),
            ToolSet::ReadOnly,
        );
        let mut approval = Scripted::new(answers);
        let result = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Pipe, "find text"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(approval.seen.len(), expected_approvals, "{path}");
        assert_eq!(result.denied > 0, denied, "{path}");
        let outputs = tool_results(&received.lock().unwrap());
        if denied {
            assert!(outputs[0].contains("[denied by user]"), "{outputs:?}");
            assert!(!outputs[0].contains("needle"), "{outputs:?}");
        } else {
            assert!(outputs[0].contains("2 matching lines"), "{outputs:?}");
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn read_only_tools_and_protected_paths() {
    let _g = setup();
    let dir = tmpdir("read");
    std::fs::write(dir.join("notes.txt"), "alpha\nbeta\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![
            call("grep", json!({"pattern": "alpha"})),
            call("read_file", json!({"path": "notes.txt"})),
            call("read_file", json!({"path": "/etc/hostname"})),
        ],
        vec![text("Read them.")],
    ]);
    let received = engine.received();
    let mut a = Agent::new(
        Box::new(engine),
        AgentConfig::default(),
        env(),
        ToolSet::ReadOnly,
    );
    let mut approval = Scripted::new([ApprovalResponse::Deny { reason: None }]);
    a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Pipe, "what is here"),
        &mut approval,
        &mut RecordUi::default(),
    );
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].contains("notes.txt"), "{}", results[0]);
    assert!(
        results[1].contains("    1  alpha\n    2  beta"),
        "{}",
        results[1]
    );
    assert_eq!(approval.seen.len(), 1, "protected read asks");
    assert!(results[2].contains("[denied by user]"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn unavailable_tools_are_rejected_before_approval_or_execution() {
    let _g = setup();
    let dir = tmpdir("tool-catalog");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    for (set, names) in [
        (ToolSet::Full, "run_command, read_file, grep"),
        (ToolSet::ReadOnly, "read_file, grep"),
    ] {
        let name = if set == ToolSet::Full {
            "list_dir"
        } else {
            "run_command"
        };
        let engine = MockChatEngine::new(vec![
            vec![call(name, json!({"command": "touch should-not-exist"}))],
            vec![text("No command was run.")],
        ]);
        let received = engine.received();
        let mut agent = Agent::new(Box::new(engine), AgentConfig::default(), env(), set);
        let mut approval = Scripted::new([]);
        let out = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Pipe, "inspect only"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(out.commands_run, 0);
        assert!(approval.seen.is_empty());
        assert!(!dir.join("should-not-exist").exists());
        assert_eq!(
            tool_results(&received.lock().unwrap()),
            [format!(
                "error: unknown tool '{name}'; available tools: {names}"
            )]
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn nosh_settings_and_state_are_protected_where_they_live() {
    let _g = setup();
    // A relative NOSH_HOME is relative to the process cwd, while permission
    // targets are absolute. It still has to protect the actual directory.
    let original_home = std::env::var_os("NOSH_HOME").unwrap();
    let relative_home =
        std::path::PathBuf::from(format!(".nosh-agent-flow-{}", std::process::id()));
    // SAFETY: tests in this process are serialized by `setup`.
    unsafe { std::env::set_var("NOSH_HOME", &relative_home) };
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![
        vec![call(
            "read_file",
            json!({"path": relative_home.join("config.toml").display().to_string()}),
        )],
        vec![call(
            "run_command",
            json!({"command": format!(
                "echo x >> {}",
                relative_home.join("state/history.jsonl").display()
            )}),
        )],
        vec![text("Left them alone.")],
    ]);
    let mut a = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Auto,
            ..AgentConfig::default()
        },
    );
    let mut approval = Scripted::new([
        ApprovalResponse::Deny { reason: None },
        ApprovalResponse::Deny { reason: None },
    ]);
    a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "tweak nosh"),
        &mut approval,
        &mut RecordUi::default(),
    );
    // SAFETY: restores the value before releasing the serial test lock.
    unsafe { std::env::set_var("NOSH_HOME", original_home) };
    assert_eq!(approval.seen.len(), 2, "even auto mode asks");
    assert_eq!(approval.seen[0].risk, Risk::Mutating, "protected read");
    assert_eq!(approval.seen[1].risk, Risk::Dangerous, "protected write");
    assert!(approval.seen[1].strong);
}
