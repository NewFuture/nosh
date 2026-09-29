use super::support::*;

#[test]
fn completion_assistance_is_latest_only_and_reuses_the_agent_engine() {
    use nosh_shell::{AiHandler, Assistance};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};
    let _guard = setup();
    let directory = tmpdir("automatic-assistance");
    let mut sh = EmbeddedShell::new(ShellOptions {
        working_dir: Some(directory.clone()),
        ..Default::default()
    })
    .unwrap();
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    let model = MockChatEngine::with_responder(move |history| {
        let assistance =
            matches!(&history[0], Message::System(s) if s.contains("command assistant"));
        if !assistance {
            return vec![text("agent reply")];
        }
        let round = seen.fetch_add(1, Ordering::SeqCst);
        if round == 0 {
            // This bounded wait makes the first result arrive after invalidation.
            std::thread::sleep(Duration::from_millis(150));
        }
        vec![call(
            "finish",
            json!({"kind":"command","text":format!("echo candidate-{round}")}),
        )]
    });
    let specs = model.specs();
    let loads = Arc::new(AtomicUsize::new(0));
    let loaded = loads.clone();
    let mut model = Some(model);
    let mut ai = ShellAi::new(
        Box::new(move |_| {
            loaded.fetch_add(1, Ordering::SeqCst);
            Ok(nosh_core::LoadedEngine {
                engine: Box::new(model.take().unwrap()),
                description: "mock".into(),
            })
        }),
        AgentConfig::default(),
        Box::new(Scripted::new([])),
    );
    ai.handle(
        &mut sh,
        nosh_shell::AiRequest {
            trigger: Trigger::Hash,
            text: "hello".into(),
            failed: None,
            user_output: None,
        },
    );
    sh.run_user_line("true");
    ai.after_command(&sh, sh.recent_commands().last().unwrap().clone(), None);
    let deadline = Instant::now() + Duration::from_secs(3);
    while count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert_eq!(count.load(Ordering::SeqCst), 1);
    ai.assistance().unwrap().invalidate();
    sh.run_user_line("true");
    let current = sh.recent_commands().last().unwrap().clone();
    ai.after_command(&sh, current.clone(), None);
    while ai.assistance().unwrap().result().is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(ai.assistance().unwrap().result(),
            Some(Assistance::Command { command_id, program, .. })
            if command_id == current.id && program == "echo candidate-1"));
    ai.handle(
        &mut sh,
        nosh_shell::AiRequest {
            trigger: Trigger::Hash,
            text: "follow up".into(),
            failed: None,
            user_output: None,
        },
    );
    assert_eq!(loads.load(Ordering::SeqCst), 1);
    assert_eq!(
        specs
            .lock()
            .unwrap()
            .iter()
            .filter(|spec| spec.label == "agent")
            .count(),
        1
    );
    assert!(ai.assistance().unwrap().result().is_none());
    drop(ai);
    std::fs::remove_dir_all(directory).unwrap();
}

#[test]
fn generate_and_ctrl_g_use_finish_without_target_execution() {
    use nosh_shell::AiHandler;
    let _g = setup();
    let mut sh = shell();
    let dir = tmpdir("suggest");
    sh.run_user_line(&format!("cd {}", dir.display()));
    for response in ["touch suggested", "for f in *.txt; do\n  echo \"$f\"\ndone"] {
        let mut engine = MockChatEngine::new(vec![vec![call(
            "finish",
            json!({"kind": "command", "text": response}),
        )]]);
        let specs = engine.specs();
        let result = generate(&mut engine, &sh, "suggest", &AgentConfig::default()).unwrap();
        assert_eq!(result.result, AssistResult::Command(response.into()));
        assert!(!dir.join("suggested").exists());
        assert_eq!(
            specs.lock().unwrap()[0]
                .tools
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            ["command_info", "read_file", "grep", "finish"]
        );
        assert_eq!(specs.lock().unwrap()[0].sampling.temperature, 1.0);
    }
    let mut engine = Some(MockChatEngine::new(vec![vec![call(
        "finish",
        json!({"kind": "command", "text": "touch suggested"}),
    )]]));
    let mut ai = ShellAi::new(
        Box::new(move |_| {
            Ok(nosh_core::LoadedEngine {
                engine: Box::new(engine.take().unwrap()),
                description: "mock".into(),
            })
        }),
        AgentConfig::default(),
        Box::new(Scripted::new([])),
    );
    assert_eq!(
        ai.suggest(&mut sh, "create suggested").as_deref(),
        Some("touch suggested")
    );
    assert!(!dir.join("suggested").exists());
    let _ = std::fs::remove_dir_all(dir);
}
