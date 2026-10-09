use super::support::*;
use std::os::unix::fs::PermissionsExt;

fn fixture(fail_b: bool) -> (std::path::PathBuf, EmbeddedShell) {
    let directory = tmpdir(if fail_b {
        "u5-env-failed"
    } else {
        "u5-env-batch"
    });
    let root = &directory;
    for path in ["tools", "a-bin", "b", "b/bin"] {
        std::fs::create_dir(root.join(path)).unwrap();
    }
    for (path, script) in [
        (
            "a-bin/selected-tool",
            "#!/bin/sh\nprintf 'TOOL-A\\n'\n".to_string(),
        ),
        (
            "b/bin/selected-tool",
            "#!/bin/sh\nprintf 'TOOL-B\\n'\n".to_string(),
        ),
        (
            "tools/direnv",
            format!(
                "#!/bin/sh\ncase \"$PWD\" in */b) {};; *) printf '{{\"PATH\":\"%s/a-bin:/usr/bin:/bin\",\"PROJECT\":\"A\"}}' \"$FIXTURE\";; esac\n",
                if fail_b {
                    "exit 2"
                } else {
                    "printf '{\"PATH\":\"%s/b/bin:/usr/bin:/bin\",\"PROJECT\":\"B\"}' \"$FIXTURE\""
                },
            ),
        ),
    ] {
        let path = root.join(path);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(root.into()),
        ..Default::default()
    })
    .unwrap();
    let escaped = root.to_string_lossy().replace('\'', "'\\''");
    shell.run_user_line(&format!(
        "export FIXTURE='{escaped}'; export PATH='{escaped}/tools:/usr/bin:/bin'"
    ));
    shell.configure_project_env(Ok(nosh_shell::project_env::Provider::Direnv));
    (directory, shell)
}

#[test]
fn environment_refreshes_between_tools_in_one_agent_batch_and_before_assessment() {
    let _guard = setup();
    let (directory, mut shell) = fixture(false);
    let engine = MockChatEngine::with_responder(|history| {
        if history
            .iter()
            .any(|message| matches!(message, Message::Tool(_)))
        {
            vec![text("done")]
        } else {
            vec![
                call("exec", json!({"command":"cd b"})),
                call("exec", json!({"command":"selected-tool"})),
            ]
        }
    });
    let received = engine.received();
    let mut agent = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Yolo,
            restore_cwd: true,
            ..Default::default()
        },
    );
    let result = agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Hash, "run the project tool"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(result.status, TaskStatus::Completed, "{result:?}");
    assert_eq!(result.commands_run, 2);
    assert!(
        tool_results(&received.lock().unwrap())
            .iter()
            .any(|text| text.contains("TOOL-B"))
    );
    assert_eq!(shell.cwd(), directory);
    shell.refresh_project_env().unwrap();
    assert_eq!(shell.var("PROJECT").as_deref(), Some("A"));
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn failed_environment_stops_the_batch_without_executing_or_inventing_a_command_result() {
    let _guard = setup();
    let (directory, mut shell) = fixture(true);
    let engine = MockChatEngine::with_responder(|_| {
        vec![
            call("exec", json!({"command":"cd b"})),
            call("exec", json!({"command":"selected-tool"})),
            call("exec", json!({"command":"touch should-not-exist"})),
        ]
    });
    let mut agent = agent(
        engine,
        AgentConfig {
            mode: ApprovalMode::Yolo,
            ..Default::default()
        },
    );
    let result = agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Hash, "run the project tool"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(result.status, TaskStatus::Failed, "{result:?}");
    assert_eq!(result.commands_run, 1, "only the actual cd ran");
    assert!(!directory.join("b/should-not-exist").exists());
    assert_eq!(shell.var("PROJECT").as_deref(), Some("A"));
    std::fs::remove_dir_all(directory).unwrap();
}
