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
    assert_eq!(
        names,
        ["run_command", "read_file", "list_dir", "propose_command"]
    );
    assert!(
        ui.events
            .iter()
            .any(|e| e.starts_with("tool run_command [SAFE] SAFE · auto"))
    );
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
fn propose_command_ends_the_task_with_a_prefill() {
    let _g = setup();
    let mut sh = shell();
    let engine = MockChatEngine::new(vec![vec![call(
        "propose_command",
        json!({"command": "sudo apt install jq", "explanation": "needs a password"}),
    )]]);
    let mut a = agent(engine, AgentConfig::default());
    let out = a.run_task(
        &mut sh,
        TaskInput::new(Trigger::Hash, "install jq"),
        &mut Scripted::new([]),
        &mut RecordUi::default(),
    );
    assert_eq!(out.status, TaskStatus::Completed);
    assert_eq!(out.steps, 1);
    assert_eq!(out.proposed.as_deref(), Some("sudo apt install jq"));
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
            "propose_command",
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
