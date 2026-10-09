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
            .step(1, vec![], &mut crate::RecordUi::default())
            .unwrap_err();
        assert_eq!(error.to_string(), format!("{failure} failed"));
        assert_eq!(*calls.lock().unwrap(), expected);
    }
}

#[test]
fn non_context_error_rolls_back_the_appended_messages() {
    let (mut agent, calls) = recovery_agent(Some("append"));
    let error = agent
        .step(
            1,
            vec![Message::User("evidence".into())],
            &mut crate::RecordUi::default(),
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
        agent.step(1, vec![], &mut crate::RecordUi::default()),
        Err(EngineError::ContextFull { .. })
    ));
    assert_eq!(
        *calls.lock().unwrap(),
        ["step", "rewind", "compact", "step", "rewind"]
    );
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
    agent.reset_conversation();
    assert!(agent.carry.is_empty());
    assert!(agent.guidance_sent.is_none());
    assert_eq!(agent.sid, None);
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
