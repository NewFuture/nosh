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
    let Message::User(task) = &rec[0][0] else {
        panic!("first message must be the task");
    };
    assert!(task.starts_with("[task trigger=hash cwd="), "{task}");
    assert!(task.ends_with("\nsay hello"));
    let results = tool_results(&rec);
    assert!(results[0].starts_with("[exit_code=0 "), "{}", results[0]);
    assert!(results[0].contains("--- stdout ---\nhello-from-shell\n"));
    let spec = &specs.lock().unwrap()[0];
    assert!(spec.system.contains("<tool_def_sep>"));
    let names: Vec<_> = spec.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, ["run_command", "read_file", "list_dir"]);
    assert!(
        ui.events
            .iter()
            .any(|e| e.starts_with("tool run_command [SAFE] SAFE · auto"))
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
            self.inner.context_usage(sid)
        }

        fn cancel_handle(&self) -> CancelHandle {
            self.inner.cancel_handle()
        }

        fn close(&mut self, sid: SessionId) {
            self.inner.close(sid);
        }
    }

    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::with_responder(|history| {
        if history.iter().any(
            |message| matches!(message, Message::Tool(result) if result.contains("printed-once")),
        ) {
            vec![text("The command printed printed-once.")]
        } else {
            vec![call(
                "run_command",
                json!({"command": "printf printed-once"}),
            )]
        }
    });
    let received = engine.received();
    let mut agent = Agent::new(
        Box::new(Engine {
            inner: engine,
            steps: 0,
            fail_compaction: true,
        }),
        AgentConfig::default(),
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

    let received = received.lock().unwrap();
    assert_eq!(received.len(), 3);
    let [Message::Tool(original)] = &received[1][..] else {
        panic!("the failed append must contain the command result");
    };
    let [Message::Tool(restored), Message::User(followup)] = &received[2][..] else {
        panic!("the next task must receive the result before its own input");
    };
    assert_eq!(restored, original);
    assert!(followup.ends_with("\nwhat did the command print?"));
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
        Some(Message::User(u)) if u.contains("Step limit reached") => vec![text("Summary.")],
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
fn read_only_tools_and_protected_paths() {
    let _g = setup();
    let dir = tmpdir("read");
    std::fs::write(dir.join("notes.txt"), "alpha\nbeta\n").unwrap();
    let mut sh = shell();
    sh.run_user_line(&format!("cd {}", dir.display()));
    let engine = MockChatEngine::new(vec![
        vec![
            call("list_dir", json!({})),
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
        (ToolSet::Full, "run_command, read_file, list_dir"),
        (ToolSet::ReadOnly, "read_file, list_dir"),
        (ToolSet::Suggest, ""),
    ] {
        let name = if set == ToolSet::Full {
            "unknown_tool"
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
        Some(Message::User(u)) if u.contains("trigger=not_found") => vec![text("Not a command.")],
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

    // Unknown command words (e.g. Chinese) trigger the AI with not_found.
    p.process(&mut sh, &mut ai, &mut Ui, "帮我看看磁盘空间");
    let rec = received.lock().unwrap();
    let Some(Message::User(u)) = rec.last().and_then(|m| m.last()) else {
        panic!("expected a task message");
    };
    assert!(u.contains("trigger=not_found"), "{u}");
    assert!(u.contains(" lang=zh]"), "{u}");
    assert!(u.contains("[recent] pwd >"), "{u}");
    drop(rec);
    let _ = std::fs::remove_dir_all(dir);
}
