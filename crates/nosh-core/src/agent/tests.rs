use super::*;
use nosh_engine::{CancelHandle, Event};
use std::sync::Mutex;

struct RecoveryEngine {
    fail: Option<&'static str>,
    calls: Arc<Mutex<Vec<&'static str>>>,
    messages: usize,
    append_failed: bool,
}

impl RecoveryEngine {
    fn record(&self, operation: &'static str) -> Result<(), EngineError> {
        self.calls.lock().unwrap().push(operation);
        if self.fail == Some(operation) {
            Err(EngineError::Config(format!("{operation} failed")))
        } else {
            Ok(())
        }
    }
}

impl ChatEngine for RecoveryEngine {
    fn set_tool_choice(&mut self, _: SessionId, choice: ToolChoice) -> Result<(), EngineError> {
        assert_eq!(choice, ToolChoice::None);
        self.record("choice")
    }

    fn open(&mut self, _spec: SessionSpec) -> Result<SessionId, EngineError> {
        self.record("open")?;
        Ok(1)
    }

    fn step(
        &mut self,
        _sid: SessionId,
        append: Vec<Message>,
        _sink: &mut dyn FnMut(Event),
    ) -> Result<StepOutcome, EngineError> {
        self.record("step")?;
        self.messages += append.len();
        if self.fail == Some("observation") {
            return Ok(StepOutcome {
                text: String::new(),
                think: String::new(),
                tool_calls: ["printf recorded", "printf should-not-run"]
                    .into_iter()
                    .map(|command| ToolCall {
                        name: "exec".into(),
                        args: serde_json::json!({"command": command})
                            .as_object()
                            .unwrap()
                            .clone(),
                    })
                    .collect(),
                errors: Vec::new(),
                stop: StopReason::EndOfTurn,
                usage: Usage::default(),
            });
        }
        if self.fail == Some("append") {
            self.fail = None;
            self.append_failed = true;
            return Err(EngineError::Config("append failed".into()));
        }
        if self.append_failed {
            return Ok(StepOutcome {
                text: "done".into(),
                think: String::new(),
                tool_calls: Vec::new(),
                errors: Vec::new(),
                stop: StopReason::EndOfTurn,
                usage: Usage::default(),
            });
        }
        Err(EngineError::ContextFull {
            used: 100,
            max: 100,
        })
    }

    fn rewind(&mut self, _sid: SessionId, keep: usize) -> Result<(), EngineError> {
        self.record("rewind")?;
        self.messages = keep;
        Ok(())
    }

    fn compact_tool_results(
        &mut self,
        _sid: SessionId,
        _keep_recent: usize,
    ) -> Result<usize, EngineError> {
        self.record("compact")?;
        Ok(0)
    }

    fn message_count(&self, _sid: SessionId) -> usize {
        self.messages
    }

    fn context_usage(&self, _sid: SessionId) -> (usize, usize) {
        (90, 100)
    }

    fn cancel_handle(&self) -> CancelHandle {
        CancelHandle::default()
    }

    fn record_observation(
        &mut self,
        _: SessionId,
        _: serde_json::Value,
    ) -> Result<(), EngineError> {
        self.record("observation")
    }

    fn close(&mut self, _sid: SessionId) {
        self.calls.lock().unwrap().push("close");
    }
}

fn recovery_agent(fail: Option<&'static str>) -> (Agent, Arc<Mutex<Vec<&'static str>>>) {
    let calls = Arc::default();
    let agent = Agent::new(
        Box::new(RecoveryEngine {
            fail,
            calls: Arc::clone(&calls),
            messages: 1,
            append_failed: false,
        }),
        AgentConfig::default(),
        Environment {
            os: "Linux".into(),
            arch: "x86_64".into(),
            user: "test".into(),
            available: vec![],
        },
        ToolSet::Full,
    );
    (agent, calls)
}

#[test]
fn context_recovery_stops_at_the_first_error() {
    for (failure, expected) in [
        ("rewind", vec!["step", "rewind"]),
        ("compact", vec!["step", "rewind", "compact"]),
    ] {
        let (mut agent, calls) = recovery_agent(Some(failure));
        let error = agent
            .step(1, vec![], &mut crate::RecordUi::default(), false)
            .unwrap_err();
        assert_eq!(error.to_string(), format!("{failure} failed"));
        assert_eq!(*calls.lock().unwrap(), expected);
    }
}

#[test]
fn observation_failure_preserves_executed_results_and_stops_later_calls() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let (mut agent, calls) = recovery_agent(Some("observation"));
    let mut ui = crate::RecordUi::default();
    let result = agent.run_task(
        &mut shell,
        TaskInput::new(nosh_shell::Trigger::Hash, "inspect"),
        &mut crate::Scripted::new([]),
        &mut ui,
    );
    assert_eq!(result.status, TaskStatus::Failed);
    assert_eq!(result.commands_run, 1);
    assert!(
        result
            .error
            .unwrap()
            .contains("could not record tool outcome")
    );
    assert_eq!(agent.carry.len(), 2);
    assert!(matches!(&agent.carry[0], Message::Tool(text) if text.contains("recorded")));
    assert!(matches!(&agent.carry[1], Message::Tool(text) if text.contains("[skipped]")));
    assert!(!ui.output.contains("should-not-run"));
    assert_eq!(
        *calls.lock().unwrap(),
        ["open", "step", "observation", "observation"]
    );
}

#[test]
fn execution_errors_report_unknown_effects_without_running_later_calls() {
    fn incomplete_run(
        shell: &mut EmbeddedShell,
        _: &str,
        _: &AgentExecOpts,
        _: &mut dyn nosh_shell::OutputSink,
    ) -> Result<nosh_shell::CommandResult, nosh_shell::ShellError> {
        std::fs::write(shell.cwd().join("effect.txt"), "already happened")?;
        Err(std::io::Error::other("result unavailable").into())
    }
    let dir = tempfile::tempdir().unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(dir.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let engine = nosh_engine::MockChatEngine::new(vec![vec![
        nosh_engine::mock::call("exec", serde_json::json!({"command": "printf first"})),
        nosh_engine::mock::call("exec", serde_json::json!({"command": "touch later"})),
    ]]);
    let observations = engine.observations();
    let received = engine.received();
    let mut agent = Agent::new(
        Box::new(engine),
        AgentConfig::default(),
        Environment::default(),
        ToolSet::Full,
    );
    agent.command_runner = incomplete_run;
    let outcome = agent.run_task(
        &mut shell,
        TaskInput::new(nosh_shell::Trigger::Hash, "inspect"),
        &mut crate::Scripted::new([]),
        &mut crate::RecordUi::default(),
    );
    assert_eq!(outcome.status, TaskStatus::Failed);
    assert_eq!(outcome.commands_run, 1);
    assert!(outcome.error.unwrap().contains("Effects may have occurred"));
    assert!(dir.path().join("effect.txt").exists());
    assert!(!dir.path().join("later").exists());
    assert_eq!(received.lock().unwrap().len(), 1);
    let observations = observations.lock().unwrap();
    assert_eq!(observations.len(), 2);
    assert_eq!(observations[0].1["status"], "execution_error");
    assert_eq!(observations[0].1["execution"]["command"], "printf first");
    assert!(observations[0].1["execution"]["exit_code"].is_null());
    assert!(observations[0].1["execution"]["timed_out"].is_null());
    assert_eq!(observations[1].1["status"], "skipped");
    assert!(observations[1].1["execution"].is_null());
}

#[test]
fn non_context_error_rolls_back_the_appended_messages() {
    let (mut agent, calls) = recovery_agent(Some("append"));
    let error = agent
        .step(
            1,
            vec![Message::User("evidence".into())],
            &mut crate::RecordUi::default(),
            false,
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "append failed");
    assert_eq!(agent.engine.message_count(1), 1);
    assert_eq!(*calls.lock().unwrap(), ["step", "rewind"]);
}

#[test]
fn failed_append_does_not_suppress_project_guidance() {
    let directory = tempfile::tempdir().unwrap();
    let notes = directory.path().join("AGENTS.md");
    std::fs::write(&notes, "project instructions").unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(directory.path().to_path_buf()),
        ..nosh_shell::ShellOptions::default()
    })
    .unwrap();
    let (mut agent, _) = recovery_agent(Some("append"));
    let mut ui = crate::RecordUi::default();

    let first = agent.run_task(
        &mut shell,
        TaskInput::new(nosh_shell::Trigger::Hash, "first"),
        &mut crate::Scripted::new([]),
        &mut ui,
    );
    assert_eq!(first.status, TaskStatus::Failed);
    assert!(agent.guidance_sent.is_none());

    let second = agent.run_task(
        &mut shell,
        TaskInput::new(nosh_shell::Trigger::Hash, "second"),
        &mut crate::Scripted::new([]),
        &mut ui,
    );
    assert_eq!(second.status, TaskStatus::Completed);
    assert!(agent.guidance_sent.is_some());
}

#[test]
fn compaction_failure_carries_results_without_replaying_user_messages() {
    let (mut agent, calls) = recovery_agent(Some("compact"));
    let first = Message::Tool("first executed result".into());
    let second = Message::UserAnswer("keep the backup files".into());
    let error = agent
        .step(
            1,
            vec![
                first.clone(),
                Message::User("failed task".into()),
                second.clone(),
                Message::System(SUMMARIZE.into()),
            ],
            &mut crate::RecordUi::default(),
            false,
        )
        .unwrap_err();
    assert_eq!(error.to_string(), "compact failed");
    assert_eq!(agent.carry, [first, second]);
    assert_eq!(*calls.lock().unwrap(), ["step", "rewind", "compact"]);
}

#[test]
fn context_recovery_only_retries_once() {
    let (mut agent, calls) = recovery_agent(None);
    assert!(matches!(
        agent.step(1, vec![], &mut crate::RecordUi::default(), false),
        Err(EngineError::ContextFull { .. })
    ));
    assert_eq!(
        *calls.lock().unwrap(),
        ["step", "rewind", "compact", "step", "rewind"]
    );
}

#[test]
fn final_response_keeps_tool_choice_none_after_context_recovery() {
    let (mut agent, calls) = recovery_agent(None);
    assert!(matches!(
        agent.step(1, vec![], &mut crate::RecordUi::default(), true),
        Err(EngineError::ContextFull { .. })
    ));
    assert_eq!(
        *calls.lock().unwrap(),
        [
            "choice", "step", "rewind", "compact", "choice", "step", "rewind"
        ]
    );
    let (mut agent, calls) = recovery_agent(Some("choice"));
    let pending = vec![
        Message::Tool("already executed".into()),
        Message::UserAnswer("keep the backup".into()),
    ];
    let error = agent
        .step(1, pending.clone(), &mut crate::RecordUi::default(), true)
        .unwrap_err();
    assert_eq!(error.to_string(), "choice failed");
    assert_eq!(agent.carry, pending);
    assert_eq!(*calls.lock().unwrap(), ["choice", "rewind"]);
}

#[test]
fn pre_task_compaction_failure_does_not_reset_the_conversation() {
    let (mut agent, calls) = recovery_agent(Some("compact"));
    agent.sid = Some(1);
    assert_eq!(
        agent.ensure_session().unwrap_err().to_string(),
        "compact failed"
    );
    assert_eq!(agent.sid, Some(1));
    assert_eq!(*calls.lock().unwrap(), ["compact"]);
}

#[test]
fn session_setup_errors_preserve_carried_results() {
    for (failure, expected_calls) in [
        ("open", vec!["close", "open"]),
        ("compact", vec!["compact"]),
    ] {
        let (mut agent, calls) = recovery_agent(Some(failure));
        agent.sid = Some(1);
        if failure == "open" {
            agent.cfg.idle_reset = Duration::ZERO;
            agent.last_task = Some(Instant::now() - Duration::from_secs(1));
        }
        let result = Message::Tool("already executed".into());
        agent.carry.push(result.clone());
        let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
        let outcome = agent.run_task(
            &mut shell,
            TaskInput::new(nosh_shell::Trigger::Hash, "continue"),
            &mut crate::Scripted::new([]),
            &mut crate::RecordUi::default(),
        );
        assert_eq!(outcome.status, TaskStatus::Failed);
        assert_eq!(outcome.error, Some(format!("{failure} failed")));
        assert_eq!(agent.carry, [result]);
        assert_eq!(*calls.lock().unwrap(), expected_calls);
    }
}

#[test]
fn explicit_conversation_reset_discards_carried_results() {
    let (mut agent, calls) = recovery_agent(None);
    agent.sid = Some(1);
    agent.carry.push(Message::Tool("already executed".into()));
    agent.guidance_sent = Some("AGENTS.md version".into());
    agent.model_turn = 5;
    agent.reset_conversation();
    assert!(agent.carry.is_empty());
    assert!(agent.guidance_sent.is_none());
    assert_eq!(agent.sid, None);
    assert_eq!(agent.model_turn, 0);
    assert_eq!(*calls.lock().unwrap(), ["close"]);
}

#[test]
fn password_handoff_requires_a_rewritten_sudo_and_explicit_diagnostic() {
    let mut result = nosh_shell::CommandResult {
        exit_code: 1,
        stderr: "sudo: a password is required\n".into(),
        ..Default::default()
    };
    assert!(needs_handoff(&result, true));
    assert!(!needs_handoff(&result, false));
    result.exit_code = 0;
    assert!(
        needs_handoff(&result, true),
        "a later successful command must not mask sudo's diagnostic"
    );
    result.stderr = "sudo: user is not in the sudoers file\n".into();
    assert!(!needs_handoff(&result, true));
    result.needed_terminal = true;
    assert!(needs_handoff(&result, false));
}
