//! End-to-end task flows against MockChatEngine (no model): the REPL
//! pipeline, the agent loop, permissions and the shared shell session.

use std::sync::{Mutex, MutexGuard, Once};

use nosh_core::{
    Agent, AgentConfig, ApprovalResponse, Environment, NoTerminal, RecordUi, Scripted, ShellAi,
    TaskInput, TaskStatus, ToolSet,
};
use nosh_llm::mock::{bad_call, call, text};
use nosh_llm::{CallErrorKind, Message, MockChatEngine};
use nosh_permissions::{ApprovalMode, Risk};
use nosh_shell::repl::{GuardChoice, LineOutcome, Pipeline, ReplUi};
use nosh_shell::{EmbeddedShell, ReplConfig, ShellOptions, Trigger};
use serde_json::json;

static SERIAL: Mutex<()> = Mutex::new(());
static HOME: Once = Once::new();

fn setup() -> MutexGuard<'static, ()> {
    HOME.call_once(|| {
        let home = std::env::temp_dir().join(format!("nosh-flow-home-{}", std::process::id()));
        std::fs::create_dir_all(&home).unwrap();
        // SAFETY: set once before any test reads it; tests are serialized.
        unsafe { std::env::set_var("NOSH_HOME", home) };
    });
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn shell() -> EmbeddedShell {
    EmbeddedShell::new(ShellOptions::default()).unwrap()
}

fn env() -> Environment {
    Environment {
        os: "Linux".into(),
        arch: "x86_64".into(),
        user: "test".into(),
        available: vec![],
    }
}

fn agent(engine: MockChatEngine, cfg: AgentConfig) -> Agent {
    Agent::new(Box::new(engine), cfg, env(), ToolSet::Full)
}

fn tool_results(received: &[Vec<Message>]) -> Vec<String> {
    received
        .iter()
        .flatten()
        .filter_map(|m| match m {
            Message::Tool(t) => Some(t.clone()),
            _ => None,
        })
        .collect()
}

fn context_field<'a>(message: &'a str, name: &str) -> Option<&'a str> {
    let context = message.split_once("[context]\n")?.1;
    let prefix = format!("{name}: ");
    context.lines().find_map(|line| line.strip_prefix(&prefix))
}

fn tmpdir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("nosh-flow-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    std::fs::canonicalize(&d).unwrap()
}

#[test]
fn multi_step_task_uses_tool_results() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::with_responder(|history| {
        let last = history.last().unwrap();
        match last {
            Message::User(_) => vec![
                text("Let me check."),
                call("run_command", json!({"command": "echo hello-from-shell"})),
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
    assert_eq!(names, ["run_command", "read_file", "grep"]);
    assert!(
        ui.events
            .iter()
            .any(|e| e.starts_with("tool run_command [SAFE] SAFE · auto"))
    );
}

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
        Message::User(text) => text,
        other => panic!("expected user message, got {other:?}"),
    };
    assert!(task(0).contains("[user_output "));
    assert!(!task(1).contains("[user_output "));
    assert!(task(2).contains("[user_output "));
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
                vec![call(
                    "run_command",
                    json!({"command": "printf printed-once"}),
                )]
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
    let mut a = agent(engine, AgentConfig::default());
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
    let mut a = agent(engine, AgentConfig::default());
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
fn malformed_calls_are_fed_back_then_give_up() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![
        vec![bad_call(
            CallErrorKind::MissingParam,
            "missing parameter 'command'",
        )],
        vec![call("run_command", json!({"command": "echo fixed"}))],
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
        vec![call("run_command", json!({"command": "seq 1 20000"}))],
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
        _ => vec![call("run_command", json!({"command": "true"}))],
    });
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
    let mut a = agent(MockChatEngine::new(turns()), AgentConfig::default());
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
        call("run_command", json!({"command": command})),
        call("run_command", json!({"command": "echo must-not-run"})),
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
        call("run_command", json!({"command": command})),
        call("run_command", json!({"command": "touch must-not-run"})),
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
fn suggest_and_ctrl_g_use_text_without_tools_or_execution() {
    use nosh_shell::AiHandler;
    let _g = setup();
    let mut sh = shell();
    let dir = tmpdir("suggest");
    sh.run_user_line(&format!("cd {}", dir.display()));
    for response in [
        "touch suggested",
        "```bash\ntouch suggested\n```",
        "for f in *.txt; do\n  echo \"$f\"\ndone",
    ] {
        let mut engine = MockChatEngine::new(vec![vec![text(response)]]);
        let specs = engine.specs();
        let result = nosh_core::suggest::suggest(
            &mut engine,
            &env(),
            &sh,
            "suggest",
            Trigger::Cli,
            nosh_llm::SamplingParams::default(),
        )
        .unwrap()
        .unwrap();
        assert!(!result.command.contains("```"));
        assert!(!dir.join("suggested").exists());
        assert!(specs.lock().unwrap()[0].tools.is_empty());
        assert_eq!(specs.lock().unwrap()[0].sampling.temperature, 1.0);
    }
    let mut engine = Some(MockChatEngine::new(vec![vec![text("touch suggested")]]));
    let mut ai = ShellAi::new(
        Box::new(move || {
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
                "run_command",
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
    let suggestion = nosh_core::suggest::suggest_with_context(
        &mut engine,
        &env(),
        &sh,
        "suggest",
        Trigger::Cli,
        cfg.sampling,
        &cfg.permission_context(&sh),
    )
    .unwrap();
    assert_eq!(suggestion.unwrap().command, "echo ok");
    check(&received.lock().unwrap());

    let engine = MockChatEngine::new(vec![vec![text("echo ok")]]);
    let received = engine.received();
    let mut engine = Some(engine);
    let mut ai = ShellAi::new(
        Box::new(move || {
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
    let result = nosh_core::suggest::suggest_with_context(
        &mut engine,
        &env(),
        &sh,
        "suggest",
        Trigger::Cli,
        cfg.sampling,
        &cfg.permission_context(&sh),
    );
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
        let result = nosh_core::suggest::suggest_with_context(
            &mut engine,
            &env(),
            &sh,
            "suggest",
            Trigger::Cli,
            cfg.sampling,
            &cfg.permission_context(&sh),
        )
        .unwrap();
        assert_eq!(result.unwrap().command, "echo ok");
        let received = received.lock().unwrap();
        let Message::System(message) = &received[0][0] else {
            panic!("expected reference context in the task");
        };
        assert_eq!(message.contains("reference unavailable"), blocked);
        assert_eq!(message.contains("Updated reference."), !blocked);
    }
    std::fs::remove_dir_all(root).unwrap();
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
    assert_eq!(approval.seen.len(), 2, "both reads reach /etc");
    assert_eq!(approval.seen[0].command, "read_file /etc/hostname");
    let results = tool_results(&received.lock().unwrap());
    assert!(results[0].contains("[denied by user]"), "{}", results[0]);
    assert!(results[1].contains("[denied by user]"), "{}", results[1]);
    assert!(
        results[2].starts_with("error: the command contains"),
        "{}",
        results[2]
    );
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
        (ToolSet::Suggest, ""),
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

#[test]
fn timeout_is_reported() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![
        vec![call(
            "run_command",
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

struct Ui;

impl ReplUi for Ui {
    fn guard(&mut self, _: &str) -> GuardChoice {
        GuardChoice::Ai
    }
    fn notice(&mut self, _: &str) {}
}

/// Shell + REPL pipeline + ShellAi + MockChatEngine: the T4 acceptance flows.
#[test]
fn repl_pipeline_with_mock_engine() {
    let _g = setup();
    let dir = tmpdir("repl");
    let mut sh = shell();
    let engine = MockChatEngine::with_responder(move |history| match history.last() {
        Some(Message::User(u)) if u.contains("go to tmp") => {
            vec![call("run_command", json!({"command": "cd /tmp"}))]
        }
        Some(Message::Tool(t)) if t.contains("[state] cwd:") => vec![text("Now in /tmp.")],
        _ => vec![text("ok")],
    });
    let received = engine.received();
    let mut engine = Some(engine);
    let mut ai = ShellAi::new(
        Box::new(move || {
            Ok(nosh_core::LoadedEngine {
                engine: Box::new(engine.take().expect("loaded once")),
                description: "mock".into(),
            })
        }),
        AgentConfig::default(),
        Box::new(Scripted::new([])),
    );
    let mut p = Pipeline::new(ReplConfig::default());

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
