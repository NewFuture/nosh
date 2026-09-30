use super::support::*;

#[test]
fn multi_step_task_uses_tool_results() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::with_responder(|history| {
        let last = history.last().unwrap();
        match last {
            Message::User(_) => vec![
                text("Let me check."),
                call("exec", json!({"command": "echo hello-from-shell"})),
            ],
            Message::Tool(t) if t.contains("hello-from-shell") => vec![text("It printed hello.")],
            _ => vec![text("unexpected")],
        }
    });
    let received = engine.received();
    let specs = engine.specs();
    let mut a = agent(engine, AgentConfig::default());
    let mut ui = RecordUi::default();
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "say hello"),
        &mut Scripted::new([]),
        &mut ui,
    );
    assert_eq!(out.status, TaskStatus::Completed);
    assert_eq!(out.steps, 2);
    assert_eq!(out.answer, "It printed hello.");
    let rec = received.lock().unwrap();
    let [Message::System(background), Message::User(task)] = rec[0].as_slice() else {
        panic!("background and request must be separate");
    };
    assert!(background.starts_with("[context]\n"), "{background}");
    assert_eq!(task, "say hello");
    let results = tool_results(&rec);
    assert!(results[0].starts_with("[exit_code=0 "), "{}", results[0]);
    assert!(results[0].contains("--- stdout ---\nhello-from-shell\n"));
    let spec = &specs.lock().unwrap()[0];
    assert!(spec.system.contains("<tool_def_sep>"));
    let names: Vec<_> = spec.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["exec", "read_file", "grep"]);
    assert!(
        ui.events
            .iter()
            .any(|e| e.starts_with("tool exec [SAFE] SAFE · read-only"))
    );
}

#[test]
fn compaction_failure_preserves_executed_results_for_the_next_task() {
    use nosh_llm::{
        CancelHandle, ChatEngine, Event, LlmError, SessionId, SessionSpec, StepOutcome,
    };

    struct Engine {
        inner: MockChatEngine,
        steps: usize,
        fail_compaction: bool,
        force_context_reset: bool,
    }

    impl ChatEngine for Engine {
        fn open(&mut self, spec: SessionSpec) -> Result<SessionId, LlmError> {
            self.inner.open(spec)
        }

        fn step(
            &mut self,
            sid: SessionId,
            append: Vec<Message>,
            sink: &mut dyn FnMut(Event),
        ) -> Result<StepOutcome, LlmError> {
            self.steps += 1;
            if self.steps == 2 {
                // Fail after the append, without a completed assistant turn.
                let after_append = self.inner.message_count(sid) + append.len();
                self.inner.step(sid, append, &mut |_| {})?;
                self.inner.rewind(sid, after_append)?;
                return Err(LlmError::ContextFull {
                    used: 8193,
                    max: 8192,
                });
            }
            self.inner.step(sid, append, sink)
        }

        fn rewind(&mut self, sid: SessionId, keep: usize) -> Result<(), LlmError> {
            self.inner.rewind(sid, keep)
        }

        fn message_count(&self, sid: SessionId) -> usize {
            self.inner.message_count(sid)
        }

        fn compact_tool_results(
            &mut self,
            sid: SessionId,
            keep_recent: usize,
        ) -> Result<usize, LlmError> {
            if std::mem::take(&mut self.fail_compaction) {
                Err(LlmError::Tokenizer("one-time compaction failure".into()))
            } else {
                self.inner.compact_tool_results(sid, keep_recent)
            }
        }

        fn context_usage(&self, sid: SessionId) -> (usize, usize) {
            if self.force_context_reset && self.steps == 2 {
                (7500, 8192)
            } else {
                self.inner.context_usage(sid)
            }
        }

        fn cancel_handle(&self) -> CancelHandle {
            self.inner.cancel_handle()
        }

        fn close(&mut self, sid: SessionId) {
            self.inner.close(sid);
        }
    }

    let _g = setup();
    let default_idle = AgentConfig::default().idle_reset;
    for (idle_reset, force_context_reset, expected_sessions) in [
        (default_idle, false, 1),
        (std::time::Duration::ZERO, false, 2),
        (default_idle, true, 2),
    ] {
        let root = tmpdir(&format!(
            "guidance-recovery-{expected_sessions}-{force_context_reset}"
        ));
        std::fs::create_dir(root.join(".git")).unwrap();
        let guidance = root.join("AGENTS.md");
        std::fs::write(&guidance, "temporary scoped instruction").unwrap();
        let mut sh = shell();
        sh.run_user_line(&format!("cd {}", root.display()));
        let engine = MockChatEngine::with_responder(|history| {
            let has_result = history.iter().any(|message| match message {
                Message::Tool(result) => result.contains("printed-once"),
                _ => false,
            });
            if has_result {
                vec![text("The command printed printed-once.")]
            } else {
                vec![call("exec", json!({"command": "printf printed-once"}))]
            }
        });
        let received = engine.received();
        let specs = engine.specs();
        let mut agent = Agent::new(
            Box::new(Engine {
                inner: engine,
                steps: 0,
                fail_compaction: true,
                force_context_reset,
            }),
            AgentConfig {
                idle_reset,
                ..AgentConfig::default()
            },
            env(),
            ToolSet::Full,
        );
        let first = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, "print the result"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
        assert_eq!(first.status, TaskStatus::Failed);
        assert_eq!(first.commands_run, 1);
        assert!(
            first
                .error
                .as_deref()
                .unwrap()
                .contains("one-time compaction failure")
        );
        assert_eq!(agent.last_output_id(), Some(1));
        std::fs::remove_file(guidance).unwrap();

        if idle_reset.is_zero() {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        let second = agent.run_task(
            &mut sh,
            TaskInput::new(Trigger::Hash, "what did the command print?"),
            &mut Scripted::new([]),
            &mut RecordUi::default(),
        );
        assert_eq!(second.status, TaskStatus::Completed);
        assert_eq!(second.commands_run, 0, "the command must not be run again");
        assert_eq!(second.steps, 1);
        assert_eq!(second.answer, "The command printed printed-once.");
        assert_eq!(agent.last_output_id(), Some(1));
        assert_eq!(specs.lock().unwrap().len(), expected_sessions);

        let received = received.lock().unwrap();
        assert_eq!(received.len(), 3);
        let [Message::Tool(original)] = &received[1][..] else {
            panic!("the failed append must contain the command result");
        };
        let [
            Message::Tool(restored),
            Message::System(background),
            Message::User(followup),
        ] = &received[2][..]
        else {
            panic!("the next task must receive the result before its own input");
        };
        assert_eq!(restored, original);
        assert_eq!(followup, "what did the command print?");
        assert_eq!(
            background.contains("[project documents cleared]"),
            expected_sessions == 1,
            "an errored conversation must not retain removed guidance"
        );
        drop(received);
        std::fs::remove_dir_all(root).unwrap();
    }
}

#[test]
fn malformed_calls_are_fed_back_then_give_up() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![
        vec![bad_call(
            CallErrorKind::MissingParam,
            "missing parameter 'command'",
        )],
        vec![call("exec", json!({"command": "echo fixed"}))],
        vec![text("Fixed.")],
    ]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "x"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(out.status, TaskStatus::Completed);
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].starts_with("error: missing parameter 'command'"));
    assert!(results[1].contains("fixed"));

    let bad = || vec![bad_call(CallErrorKind::Malformed, "unclosed <function>")];
    let engine = MockChatEngine::new(vec![bad(), bad(), bad(), bad()]);
    let mut a = agent(engine, AgentConfig::default());
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "x"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(out.status, TaskStatus::Failed);
    assert_eq!(out.steps, 3, "same error is retried at most twice");
}

#[test]
fn long_output_is_truncated_and_saved() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![
        vec![call("exec", json!({"command": "seq 1 20000"}))],
        vec![text("Many numbers.")],
    ]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "count"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    let results = tool_results(&received.lock().unwrap());
    let r = &results[0];
    assert!(r.contains("truncated=yes"));
    assert!(r.contains("characters omitted"));
    assert!(r.contains("\n1\n2\n3\n"), "head kept");
    assert!(r.contains("\n20000\n"), "tail kept");
    assert!(r.len() < 7000, "{}", r.len());
    let log = r
        .lines()
        .find_map(|l| l.strip_prefix("[full output: "))
        .map(|l| l.trim_end_matches(']').to_string())
        .expect("full output path");
    let full = std::fs::read_to_string(log).unwrap();
    assert!(full.contains("\n10000\n"));
    assert_eq!(a.output(1).map(|o| o.text.lines().count()), Some(20000));
}

#[test]
fn step_limit_asks_for_a_summary() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::with_responder(|history| match history.last() {
        Some(Message::System(u)) if u.contains("Step limit reached") => vec![text("Summary.")],
        Some(Message::User(u)) if u == "next task" => {
            vec![call("exec", json!({"command": "printf fresh-task"}))]
        }
        Some(Message::Tool(result)) if result.contains("fresh-task") => {
            vec![text("Next task done.")]
        }
        _ => vec![call("exec", json!({"command": "true"}))],
    });
    let received = engine.received();
    let specs = engine.specs();
    let mut a = agent(
        engine,
        AgentConfig {
            max_steps: 3,
            ..AgentConfig::default()
        },
    );
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "loop"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(out.status, TaskStatus::Incomplete);
    assert_eq!(out.steps, 4);
    assert_eq!(out.answer, "Summary.");
    assert_eq!(out.status.exit_code(), 1);
    let next = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "next task"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(next.status, TaskStatus::Completed);
    assert_eq!(next.steps, 2);
    assert_eq!(next.commands_run, 1);
    assert_eq!(specs.lock().unwrap().len(), 1);
    let records = received.lock().unwrap();
    let control = records
        .iter()
        .flatten()
        .find_map(|message| match message {
            Message::System(text) if text.contains("Step limit reached") => Some(text),
            _ => None,
        })
        .unwrap();
    assert!(control.contains("preceding user request only"));
    assert!(control.contains("Later user requests may use tools normally"));
}

#[test]
fn advice_in_final_text_does_not_prefill_or_execute() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![vec![text("Try `sudo apt install jq`.")]]);
    let mut a = agent(engine, AgentConfig::default());
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "install jq"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(out.status, TaskStatus::Completed);
    assert_eq!(out.steps, 1);
    assert_eq!(out.proposed, None);
    assert_eq!(out.commands_run, 0);
}

#[test]
fn terminal_handoff_stops_without_another_model_turn_or_later_calls() {
    let _g = setup();
    let mut sh = shell();
    // A real stop signal, without depending on the test runner having a PTY.
    let command = "python3 -c 'import os, signal; os.kill(os.getpid(), signal.SIGTTIN)'";
    let engine = MockChatEngine::new(vec![vec![
        call("exec", json!({"command": command})),
        call("exec", json!({"command": "echo must-not-run"})),
    ]]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    let mut ui = RecordUi::default();
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "terminal task"),
        &mut Scripted::new([ApprovalResponse::Approve]),
        &mut ui,
    );
    assert_eq!(out.status, TaskStatus::Completed, "{out:?} {:?}", ui.events);
    assert_eq!(out.proposed.as_deref(), Some(command));
    assert_eq!(out.steps, 1);
    assert_eq!(out.commands_run, 1);
    assert_eq!(received.lock().unwrap().len(), 1);
    assert!(
        ui.events
            .iter()
            .any(|e| e.contains(command) && e.contains("terminal")),
        "{:?}",
        ui.events
    );
    assert!(a.output(2).is_none());
}

#[test]
fn compound_terminal_handoff_warns_about_partial_execution_without_replaying() {
    let _g = setup();
    let dir = tmpdir("compound-handoff");
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let command = "printf 'charged\\n' >> marker; python3 -c 'import os, signal; os.kill(os.getpid(), signal.SIGTTIN)'";
    let engine = MockChatEngine::new(vec![vec![
        call("exec", json!({"command": command})),
        call("exec", json!({"command": "touch must-not-run"})),
    ]]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    let mut ui = RecordUi::default();
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "compound terminal task"),
        &mut Scripted::new([ApprovalResponse::Approve]),
        &mut ui,
    );
    assert_eq!(out.status, TaskStatus::Completed, "{out:?} {:?}", ui.events);
    assert_eq!(out.proposed.as_deref(), Some(command));
    assert_eq!(
        std::fs::read_to_string(dir.join("marker")).unwrap(),
        "charged\n"
    );
    assert!(!dir.join("must-not-run").exists());
    assert!(a.output(2).is_none());
    assert_eq!(out.steps, 1);
    assert_eq!(out.commands_run, 1);
    assert_eq!(received.lock().unwrap().len(), 1);
    assert!(
        ui.events.iter().any(|e| {
            e.contains(command)
                && e.contains("earlier parts of this shell program may already have run")
                && e.contains("Check the current state and the entire command")
        }),
        "{:?}",
        ui.events
    );
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn grep_cancellation_stops_the_task_without_counting_a_command() {
    struct InterruptApproval(std::sync::Arc<nosh_shell::Interrupts>);
    impl nosh_core::ApprovalChannel for InterruptApproval {
        fn request(&mut self, request: &nosh_core::ApprovalRequest) -> ApprovalResponse {
            assert_eq!(request.tool, "grep");
            self.0.fire();
            ApprovalResponse::Approve
        }
    }

    let _g = setup();
    let root = tmpdir("grep-cancelled");
    std::fs::create_dir(root.join(".git")).unwrap();
    std::fs::write(
        root.join("AGENTS.md"),
        "scoped instruction after cancelled grep",
    )
    .unwrap();
    std::fs::write(root.join("secret.txt"), "needle\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", root.display()));
    let engine = MockChatEngine::new(vec![
        vec![
            call("grep", json!({"pattern": "needle"})),
            call("exec", json!({"command": "touch should-not-exist"})),
        ],
        vec![text("done")],
    ]);
    let received = engine.received();
    let mut a = agent(
        engine,
        AgentConfig {
            protected: vec![root.join("secret.txt")],
            ..Default::default()
        },
    );
    let mut approval = InterruptApproval(sh.interrupts());
    let mut ui = RecordUi::default();
    let outcome = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "find text"),
        &mut approval,
        &mut ui,
    );
    assert_eq!(outcome.status, TaskStatus::Cancelled);
    assert_eq!(outcome.status.exit_code(), 130);
    assert_eq!(outcome.commands_run, 0);
    assert_eq!(outcome.denied, 0);
    assert_eq!(
        outcome.steps, 1,
        "cancellation must not reach another model step"
    );
    assert!(!root.join("should-not-exist").exists());

    let next = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "describe"),
        &mut Scripted::new([]),
        &mut ui,
    );
    assert_eq!(next.status, TaskStatus::Completed);
    let records = received.lock().unwrap();
    assert_eq!(records.len(), 2);
    let results = tool_results(&records);
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], "[cancelled by the user]");
    assert!(results[1].starts_with("[skipped]"), "{results:?}");
    assert!(records[1].iter().any(|message| matches!(
        message,
        Message::System(text) if text.contains("scoped instruction after cancelled grep")
    )));
    drop(records);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn grep_uses_the_existing_timeout_without_cancelling_the_task() {
    let _g = setup();
    let root = tmpdir("grep-timeout");
    std::fs::write(root.join("a"), "needle\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", root.display()));
    let engine = MockChatEngine::new(vec![
        vec![call("grep", json!({"pattern": "needle"}))],
        vec![text("Search incomplete.")],
    ]);
    let received = engine.received();
    let mut a = Agent::new(
        Box::new(engine),
        AgentConfig {
            command_timeout: std::time::Duration::ZERO,
            ..Default::default()
        },
        env(),
        ToolSet::ReadOnly,
    );
    let outcome = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Pipe, "find text"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(outcome.status, TaskStatus::Completed);
    assert_eq!(outcome.commands_run, 0);
    assert_eq!(outcome.steps, 2);
    let results = tool_results(&received.lock().unwrap());
    assert_eq!(results.len(), 1);
    assert!(results[0].starts_with("[0 matching lines; truncated=yes]"));
    assert!(results[0].contains("time limit reached"), "{results:?}");
    assert!(results[0].contains("search incomplete"), "{results:?}");
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn timeout_is_reported() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![
        vec![call(
            "exec",
            json!({"command": "sleep 30", "timeout_sec": 1}),
        )],
        vec![text("It hung.")],
    ]);
    let received = engine.received();
    let mut a = agent(engine, AgentConfig::default());
    let start = std::time::Instant::now();
    a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "wait"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert!(start.elapsed().as_secs() < 10);
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].contains("timed_out=yes"), "{}", results[0]);
}

/// Shell + REPL pipeline + ShellAi + MockChatEngine: the T4 acceptance flows.
#[test]
fn repl_pipeline_with_mock_engine() {
    let _g = setup();
    let dir = tmpdir("repl");
    let mut sh = shell();
    let engine = MockChatEngine::with_responder(move |history| match history.last() {
        Some(Message::User(u)) if u.contains("go to tmp") => {
            vec![call("exec", json!({"command": "cd /tmp"}))]
        }
        Some(Message::Tool(t)) if t.contains("[state] cwd:") => vec![text("Now in /tmp.")],
        _ => vec![text("ok")],
    });
    let received = engine.received();
    let mut engine = Some(engine);
    let mut ai = ShellAi::new(
        Box::new(move |_| {
            Ok(nosh_core::LoadedEngine {
                engine: Box::new(engine.take().expect("loaded once")),
                description: "mock".into(),
            })
        }),
        AgentConfig::default(),
        Box::new(Scripted::new([])),
    );
    let mut p = Pipeline::new(ReplConfig {
        command_assist: false,
        ..Default::default()
    });

    // `#` goes to the AI; the agent's cd persists for the user's next command.
    assert_eq!(
        p.process(&mut sh, &mut ai, &mut Ui, "# go to tmp"),
        LineOutcome::Continue(None)
    );
    let out = dir.join("pwd.txt");
    p.process(
        &mut sh,
        &mut ai,
        &mut Ui,
        &format!("pwd > {}", out.display()),
    );
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "/tmp\n");

    // A typo is corrected locally and not run; the model is not involved.
    let steps_before = received.lock().unwrap().len();
    if std::process::Command::new("git")
        .arg("--version")
        .output()
        .is_ok()
    {
        assert_eq!(
            p.process(&mut sh, &mut ai, &mut Ui, "gti status"),
            LineOutcome::Continue(Some("git status".into()))
        );
    }
    assert_eq!(received.lock().unwrap().len(), steps_before);

    for (line, chinese) in [
        ("帮我看看磁盘空间", true),
        ("编译", true),
        ("# 编译", true),
        ("xqzvw_nosuch --help", false),
    ] {
        p.process(&mut sh, &mut ai, &mut Ui, line);
        let rec = received.lock().unwrap();
        let Some(append) = rec.last() else {
            panic!("expected a task message");
        };
        let [Message::System(background), Message::User(request)] = append.as_slice() else {
            panic!("expected separate background and request");
        };
        assert!(background.starts_with("[context]\n"), "{background}");
        assert!(!background.contains("trigger="), "{background}");
        assert!(context_field(background, "exit").is_none());
        assert_eq!(context_field(background, "lang") == Some("zh"), chinese);
        assert!(background.contains("[recent] pwd >"), "{background}");
        assert_eq!(request, line.trim_start_matches("# "));
    }
    let _ = std::fs::remove_dir_all(dir);
}
