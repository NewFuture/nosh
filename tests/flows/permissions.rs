use super::support::*;

#[test]
fn redundant_cd_is_corrected_before_approval_without_executing_or_rewriting() {
    let _g = setup();
    for (index, command, responses, status, runs) in [
        (0, "printf inspected", vec![], TaskStatus::Completed, 1),
        (
            1,
            "touch created.txt",
            vec![ApprovalResponse::Deny { reason: None }],
            TaskStatus::Incomplete,
            0,
        ),
        (
            2,
            "touch created.txt",
            vec![ApprovalResponse::Approve],
            TaskStatus::Completed,
            1,
        ),
    ] {
        let dir = tmpdir(&format!("cd-correction-{index}"));
        let mut sh = shell();
        sh.run_user_line(&format!("cd {}", dir.display()));
        let oldpwd = sh.var("OLDPWD");
        let engine = MockChatEngine::new(vec![
            vec![
                call("exec", json!({"command": format!("cd . && {command}")})),
                call("exec", json!({"command": "touch should-not-run"})),
            ],
            vec![call("exec", json!({"command": command}))],
            vec![text("Task ended.")],
        ]);
        let received = engine.received();
        let observations = engine.observations();
        let mut agent = agent(
            engine,
            AgentConfig {
                mode: ApprovalMode::Confirm,
                ..Default::default()
            },
        );
        let mut approval = Scripted::new(responses);
        let result = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, "inspect or create the requested file"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(result.status, status);
        assert_eq!(result.commands_run, runs);
        assert_eq!(approval.seen.len(), usize::from(index != 0));
        assert_eq!(result.denied, usize::from(index == 1));
        assert_eq!(sh.cwd(), dir);
        assert_eq!(sh.var("OLDPWD"), oldpwd);
        assert!(!dir.join("should-not-run").exists());
        assert_eq!(dir.join("created.txt").exists(), index == 2);
        let results = tool_results(&received.lock().unwrap());
        assert!(results[0].contains("exec already runs in"));
        assert!(results[0].contains("Nothing was run or sent for approval"));
        assert!(results[1].contains("[skipped]"));
        let observations = observations.lock().unwrap();
        assert_eq!(observations.len(), 3);
        assert_eq!(observations[0].1["status"], "correction");
        assert!(observations[0].1["execution"].is_null());
        assert_eq!(observations[0].1["result"], results[0]);
        assert_eq!(observations[1].1["status"], "skipped");
        assert_eq!(
            observations[2].1["status"],
            if index == 1 { "denied" } else { "executed" }
        );
        std::fs::remove_dir_all(dir).unwrap();
    }
}

#[test]
fn redundant_cd_retries_are_bounded_and_do_not_poison_the_next_task() {
    let _g = setup();
    let dir = tmpdir("cd-correction-limit");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "cd . && touch never"}))],
        vec![call("exec", json!({"command": "cd . && ls"}))],
        vec![call("exec", json!({"command": "cd . && pwd"}))],
        vec![call("exec", json!({"command": "printf recovered"}))],
        vec![text("Recovered.")],
    ]);
    let mut agent = agent(engine, AgentConfig::default());
    let mut approval = Scripted::new([]);
    let outcome = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "inspect"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(outcome.status, TaskStatus::Failed);
    assert_eq!(outcome.steps, 3);
    assert_eq!(outcome.commands_run, 0);
    assert_eq!(outcome.denied, 0);
    assert!(outcome.error.unwrap().contains("unnecessary cd prefix"));
    assert!(approval.seen.is_empty());
    assert!(!dir.join("never").exists());
    let next = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "try a different task"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(next.status, TaskStatus::Completed);
    assert_eq!(next.commands_run, 1);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn actual_tool_results_reset_cd_corrections_without_extending_the_step_budget() {
    let _g = setup();
    let dir = tmpdir("cd-correction-progress");
    std::fs::write(dir.join("notes.txt"), "error: text from a real file\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    for (tool, args, commands) in [
        ("exec", json!({"command": "printf observed"}), 3),
        ("exec", json!({"command": "false"}), 3),
        ("read_file", json!({"path": "notes.txt"}), 0),
        ("grep", json!({"path": "notes.txt", "pattern": "error"}), 0),
    ] {
        for max_steps in [10, 4] {
            let engine = MockChatEngine::new(vec![
                vec![call("exec", json!({"command": "cd . && pwd"}))],
                vec![call(tool, args.clone())],
                vec![call("exec", json!({"command": "cd . && pwd"}))],
                vec![call(tool, args.clone())],
                vec![call("exec", json!({"command": "cd . && pwd"}))],
                vec![call(tool, args.clone())],
                vec![text("Inspection complete.")],
            ]);
            let choices = engine.tool_choices();
            let mut agent = agent(
                engine,
                AgentConfig {
                    max_steps,
                    ..Default::default()
                },
            );
            let mut approval = Scripted::new([]);
            let outcome = agent.run_task(
                &mut sh,
                TaskInput::new(Trigger::Hash, "inspect"),
                &mut approval,
                &mut RecordUi::default(),
            );
            assert_eq!(
                outcome.status,
                if max_steps == 10 {
                    TaskStatus::Completed
                } else {
                    TaskStatus::Incomplete
                }
            );
            assert_eq!(outcome.steps, if max_steps == 10 { 7 } else { 5 });
            assert_eq!(
                outcome.commands_run,
                if max_steps == 10 {
                    commands
                } else {
                    commands.min(2)
                }
            );
            assert!(approval.seen.is_empty());
            if max_steps == 4 {
                assert_eq!(choices.lock().unwrap()[0].1, nosh_engine::ToolChoice::None);
            } else {
                assert!(choices.lock().unwrap().is_empty());
            }
        }
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn tool_errors_do_not_reset_cd_correction_retries() {
    let _g = setup();
    let dir = tmpdir("cd-correction-errors");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    for (tool, args) in [
        ("read_file", json!({"path": "missing.txt"})),
        ("grep", json!({"path": ".", "pattern": "["})),
        ("exec", json!({})),
        ("exec", json!({"command": "if"})),
        ("unknown_tool", json!({})),
    ] {
        let engine = MockChatEngine::new(vec![
            vec![call("exec", json!({"command": "cd . && pwd"}))],
            vec![call(tool, args.clone())],
            vec![call("exec", json!({"command": "cd . && pwd"}))],
            vec![call(tool, args)],
            vec![call("exec", json!({"command": "cd . && pwd"}))],
        ]);
        let mut agent = agent(engine, AgentConfig::default());
        let mut approval = Scripted::new([]);
        let outcome = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, "inspect"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(outcome.status, TaskStatus::Failed);
        assert_eq!(outcome.steps, 5);
        assert_eq!(outcome.commands_run, 0);
        assert!(outcome.error.unwrap().contains("unnecessary cd prefix"));
        assert!(approval.seen.is_empty());
    }
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "cd . && pwd"}))],
        vec![call("exec", json!({"command": "cd . && pwd"}))],
        vec![call("exec", json!({"command": "touch denied.txt"}))],
        vec![call("exec", json!({"command": "cd . && pwd"}))],
    ]);
    let mut agent = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..Default::default()
        },
    );
    let mut approval = Scripted::new([ApprovalResponse::Deny { reason: None }]);
    let outcome = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "inspect"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(outcome.status, TaskStatus::Failed);
    assert_eq!(outcome.steps, 4);
    assert_eq!(outcome.commands_run, 0);
    assert_eq!(approval.seen.len(), 1);
    assert!(!dir.join("denied.txt").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn explicit_directory_changes_and_user_edits_keep_normal_approval() {
    let _g = setup();
    let dir = tmpdir("cd-intentional");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "cd ."}))],
        vec![call("exec", json!({"command": "touch proposed.txt"}))],
        vec![text("Done.")],
    ]);
    let observations = engine.observations();
    let mut agent = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..Default::default()
        },
    );
    let mut approval = Scripted::new([
        ApprovalResponse::Approve,
        ApprovalResponse::Edit("cd . && touch edited.txt".into()),
        ApprovalResponse::Approve,
    ]);
    let outcome = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "set directory state and prepare a file"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(outcome.status, TaskStatus::Completed);
    assert_eq!(outcome.commands_run, 2);
    assert_eq!(approval.seen.len(), 3);
    assert!(dir.join("edited.txt").exists());
    assert!(!dir.join("proposed.txt").exists());
    assert_eq!(sh.var("OLDPWD").as_deref(), dir.to_str());
    let observations = observations.lock().unwrap();
    assert_eq!(observations.len(), 2);
    assert_eq!(
        observations[1].1["call"]["args"]["command"],
        "touch proposed.txt"
    );
    assert_eq!(
        observations[1].1["execution"]["command"],
        "cd . && touch edited.txt"
    );
    assert_eq!(observations[1].1["execution"]["edited"], true);
    assert_eq!(observations[1].1["execution"]["exit_code"], 0);
    assert_eq!(
        observations[1].1["execution"]["cwd"],
        dir.display().to_string()
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn mutating_needs_approval_and_denial_reason_reaches_model() {
    let _g = setup();
    let dir = tmpdir("deny");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "touch created.txt"}))],
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
    assert!(results[0].contains("User reason: not now"));
    assert!(results[0].contains("Approval request:"));
    for reason in &approval.seen[0].reasons {
        assert!(results[0].contains(reason), "{results:?}");
    }

    // Approved this time: the file is created in the shared session's cwd.
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "touch created.txt"}))],
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
fn repeated_denial_finishes_once_and_a_new_task_can_request_again() {
    let _g = setup();
    let dir = tmpdir("repeated-denial");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "touch created.txt"}))],
        vec![
            call("exec", json!({"command": "touch created.txt"})),
            call("exec", json!({"command": "printf should-not-run"})),
        ],
        vec![text("Not created.")],
        vec![call("exec", json!({"command": "touch created.txt"}))],
        vec![text("Created.")],
    ]);
    let choices = engine.tool_choices();
    let received = engine.received();
    let mut agent = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..Default::default()
        },
    );
    let mut approval = Scripted::new([
        ApprovalResponse::Deny {
            reason: Some("not now".into()),
        },
        ApprovalResponse::Approve,
    ]);
    let mut ui = RecordUi::default();
    let first = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "create a file"),
        &mut approval,
        &mut ui,
    );
    assert_eq!(first.status, TaskStatus::Incomplete);
    assert_eq!(first.steps, 3);
    assert_eq!(first.commands_run, 0);
    assert_eq!(first.denied, 2);
    assert_eq!(approval.seen.len(), 1);
    assert!(!dir.join("created.txt").exists());
    assert_eq!(choices.lock().unwrap()[0].1, nosh_engine::ToolChoice::None);
    let results = tool_results(&received.lock().unwrap());
    assert!(results[1].contains("[repeated denial]"));
    assert!(results[1].contains("User reason: not now"));
    assert!(results[2].contains("[skipped]"));
    assert!(!ui.output.contains("should-not-run"));
    let second = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "create it now"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(second.status, TaskStatus::Completed);
    assert_eq!(approval.seen.len(), 2);
    assert_eq!(choices.lock().unwrap().len(), 1);
    assert!(dir.join("created.txt").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn repeated_protected_reads_do_not_prompt_again_or_return_contents() {
    let _g = setup();
    let dir = tmpdir("repeated-read-denial");
    std::fs::create_dir(dir.join("private")).unwrap();
    std::fs::write(dir.join("private/secret"), "needle: private contents\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    for (tool, args) in [
        ("read_file", json!({"path": "private/secret"})),
        ("grep", json!({"pattern": "needle", "path": "private"})),
        ("grep", json!({"pattern": "needle", "path": "."})),
    ] {
        let engine = MockChatEngine::new(vec![
            vec![call(tool, args.clone())],
            vec![call(tool, args)],
            vec![text("Not read.")],
        ]);
        let received = engine.received();
        let choices = engine.tool_choices();
        let mut agent = Agent::new(
            Box::new(engine),
            AgentConfig {
                protected: vec![dir.join("private")],
                ..Default::default()
            },
            env(),
            ToolSet::ReadOnly,
        );
        let mut approval = Scripted::new([ApprovalResponse::Deny { reason: None }]);
        let outcome = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Pipe, "find text"),
            &mut approval,
            &mut RecordUi::default(),
        );
        assert_eq!(outcome.status, TaskStatus::Incomplete);
        assert_eq!(outcome.steps, 3);
        assert_eq!(approval.seen.len(), 1);
        assert_eq!(choices.lock().unwrap()[0].1, nosh_engine::ToolChoice::None);
        let results = tool_results(&received.lock().unwrap());
        assert!(results[1].contains("[repeated denial]"), "{results:?}");
        assert!(
            results
                .iter()
                .all(|result| !result.contains("private contents"))
        );
    }
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn denial_in_one_directory_does_not_block_a_different_scope() {
    let _g = setup();
    let dir = tmpdir("denial-scope");
    std::fs::create_dir(dir.join("child")).unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "touch created.txt"}))],
        vec![call("exec", json!({"command": "cd child"}))],
        vec![call("exec", json!({"command": "touch created.txt"}))],
        vec![text("Created in child.")],
    ]);
    let mut agent = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..Default::default()
        },
    );
    let mut approval = Scripted::new([
        ApprovalResponse::Deny { reason: None },
        ApprovalResponse::Approve,
        ApprovalResponse::Approve,
    ]);
    let outcome = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "create in child"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(outcome.status, TaskStatus::Completed);
    assert_eq!(approval.seen.len(), 3);
    assert!(!dir.join("created.txt").exists());
    assert!(dir.join("child/created.txt").exists());
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn denial_without_user_reason_reports_the_approval_trigger_without_inventing_a_reason() {
    let _g = setup();
    let dir = tmpdir("deny-cwd");
    std::fs::create_dir(dir.join("child")).unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let before = sh.cwd();
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "cd child && ls"}))],
        vec![text("The command was not run.")],
    ]);
    let received = engine.received();
    let mut agent = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..AgentConfig::default()
        },
    );
    let mut approval = Scripted::new([ApprovalResponse::Deny { reason: None }]);
    let outcome = agent.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "inspect"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(approval.seen.len(), 1);
    assert_eq!(outcome.commands_run, 0);
    assert_eq!(outcome.denied, 1);
    assert_eq!(sh.cwd(), before);
    let results = tool_results(&received.lock().unwrap());
    assert_eq!(results.len(), 1);
    assert!(results[0].contains("[denied by user]"));
    assert!(results[0].contains("no command was run"));
    assert!(results[0].contains("Approval request:"));
    assert!(results[0].contains("changes directory"));
    assert!(!results[0].contains("User reason:"));
    assert!(results[0].contains("Do not retry or bypass this decision"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn dangerous_requires_strong_confirmation_and_edit_is_reassessed() {
    let _g = setup();
    let dir = tmpdir("danger");
    std::fs::create_dir_all(dir.join("build")).unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "rm -rf build"}))],
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
            call("exec", json!({"command": "exec bash"})),
            call("exec", json!({"command": "echo skipped"})),
        ],
        vec![call("exec", json!({"command": "exit 3"}))],
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
            vec![call("exec", json!({"command": "mkdir made"}))],
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
        vec![call("exec", json!({"command": "rm -rf ~/x #\u{202e} sl"}))],
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
    for (set, name, names) in [
        (ToolSet::Full, "list_dir", "exec, read_file, grep"),
        (ToolSet::Full, "run_command", "exec, read_file, grep"),
        (ToolSet::ReadOnly, "exec", "read_file, grep"),
    ] {
        let engine = MockChatEngine::new(vec![
            vec![call(name, json!({"command": "touch should-not-exist"}))],
            vec![text("No command was run.")],
        ]);
        let received = engine.received();
        let specs = engine.specs();
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
        let specs = specs.lock().unwrap();
        if set == ToolSet::ReadOnly {
            assert!(specs[0].system.contains("read-only: use read_file, grep"));
            assert!(!specs[0].system.contains("Available:"));
        } else {
            assert!(specs[0].system.contains("Available:"));
        }
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
            "exec",
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
