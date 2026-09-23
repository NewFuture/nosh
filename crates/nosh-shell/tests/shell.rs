//! EmbeddedShell, trigger classification and the line pipeline (no model).

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use nosh_shell::repl::{GuardChoice, LineOutcome, Pipeline, ReplUi};
use nosh_shell::trigger::{Action, Trigger, TriggerConfig, classify};
use nosh_shell::{
    AgentExecOpts, AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell, NullSink, ReplConfig,
    ShellOptions,
};

/// Agent commands signal new process groups of this process, so tests that
/// spawn processes must not overlap.
static SERIAL: Mutex<()> = Mutex::new(());

fn serial() -> MutexGuard<'static, ()> {
    SERIAL.lock().unwrap_or_else(|e| e.into_inner())
}

fn shell() -> EmbeddedShell {
    EmbeddedShell::new(ShellOptions::default()).unwrap()
}

fn tmpdir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("nosh-shell-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn agent(sh: &mut EmbeddedShell, cmd: &str) -> nosh_shell::CommandResult {
    sh.run_agent_command(cmd, &AgentExecOpts::default(), &mut NullSink)
        .unwrap()
}

fn which(name: &str) -> bool {
    std::env::var("PATH")
        .unwrap_or_default()
        .split(':')
        .any(|d| std::path::Path::new(d).join(name).exists())
}

#[test]
fn classify_lines() {
    let _g = serial();
    let mut sh = shell();
    let cfg = TriggerConfig::default();
    let c = |sh: &mut EmbeddedShell, l: &str| classify(l, sh, &cfg);
    assert_eq!(c(&mut sh, "   "), Action::Empty);
    assert_eq!(c(&mut sh, "ls -la"), Action::Execute);
    assert_eq!(
        c(&mut sh, "# find big files"),
        Action::Ai {
            trigger: Trigger::Hash,
            text: "find big files".into()
        }
    );
    assert_eq!(
        c(&mut sh, "what's using port 8080"),
        Action::Ai {
            trigger: Trigger::ParseError,
            text: "what's using port 8080".into()
        }
    );
    assert!(matches!(
        c(&mut sh, "帮我看看 8080 端口被谁占用"),
        Action::Ai {
            trigger: Trigger::NotFound,
            ..
        }
    ));
    assert!(matches!(
        c(&mut sh, "xqzvw_nosuch --help"),
        Action::Ai {
            trigger: Trigger::NotFound,
            ..
        }
    ));
    assert_eq!(c(&mut sh, "f() { echo hi; }; f"), Action::Execute);
    assert_eq!(c(&mut sh, "$CMD arg"), Action::Execute);
    assert_eq!(c(&mut sh, "rm all temp files please"), Action::Guard);
    assert_eq!(
        c(&mut sh, "ai \"find files\""),
        Action::AiBuiltin("\"find files\"".into())
    );
    // Unterminated quote without word-internal apostrophes is just incomplete.
    assert_eq!(c(&mut sh, "echo 'abc"), Action::Execute);
    let off = TriggerConfig {
        ai_enabled: false,
        ..TriggerConfig::default()
    };
    assert_eq!(classify("# comment", &mut sh, &off), Action::Execute);
}

#[test]
fn spelling_is_corrected_locally() {
    let _g = serial();
    if !which("git") {
        return;
    }
    let mut sh = shell();
    let cfg = TriggerConfig::default();
    match classify("gti status", &mut sh, &cfg) {
        Action::Correct {
            corrected,
            from,
            to,
        } => {
            assert_eq!(corrected, "git status");
            assert_eq!((from.as_str(), to.as_str()), ("gti", "git"));
        }
        other => panic!("unexpected {other:?}"),
    }
    match classify("ls && gti push", &mut sh, &cfg) {
        Action::Correct { corrected, .. } => assert_eq!(corrected, "ls && git push"),
        other => panic!("unexpected {other:?}"),
    }
}

#[test]
fn agent_output_and_exit_codes() {
    let _g = serial();
    let mut sh = shell();
    let r = agent(
        &mut sh,
        "echo out; echo err >&2; exit_code_test() { return 3; }; exit_code_test",
    );
    assert_eq!(r.stdout, "out\n");
    assert_eq!(r.stderr, "err\n");
    assert_eq!(r.exit_code, 3);
    assert!(!r.timed_out);
    assert!(r.diff.funcs_added.contains(&"exit_code_test".to_string()));
    let r = agent(&mut sh, "printf 'a\\nb\\n' | wc -l");
    assert_eq!(r.stdout.trim(), "2");
    let r = agent(&mut sh, "read x; echo \"[$x]\"");
    assert_eq!(r.stdout, "[]\n");
}

#[test]
fn agent_state_persists_in_session() {
    let _g = serial();
    let dir = tmpdir("cd");
    let mut sh = shell();
    let r = agent(&mut sh, &format!("cd {}", dir.display()));
    assert_eq!(r.exit_code, 0);
    let cwd = std::fs::canonicalize(&dir).unwrap();
    assert_eq!(r.diff.cwd.as_ref().map(|c| c.1.clone()), Some(cwd.clone()));
    assert!(r.diff.describe().starts_with("cwd: "));
    // The user's next command sees the agent's cwd and variables.
    agent(&mut sh, "export NOSH_T=42");
    let out = dir.join("pwd.txt");
    let run = sh.run_user_line(&format!("pwd > {0}; echo $NOSH_T >> {0}", out.display()));
    assert_eq!(run.exit_code, 0);
    let text = std::fs::read_to_string(&out).unwrap();
    assert_eq!(text, format!("{}\n42\n", cwd.display()));
    assert_eq!(sh.snapshot().cwd, cwd);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn anti_hang_env_is_scoped_to_one_command() {
    let _g = serial();
    let mut sh = shell();
    let before = sh.var("GIT_PAGER");
    let r = agent(&mut sh, "echo \"$GIT_PAGER|$GIT_TERMINAL_PROMPT|$PAGER\"");
    assert_eq!(r.stdout, "cat|0|cat\n");
    assert_eq!(sh.var("GIT_PAGER"), before);
    assert!(r.diff.is_empty(), "{}", r.diff.describe());
    // A deliberate change by the command survives.
    agent(&mut sh, "export PAGER=less");
    assert_eq!(sh.var("PAGER").as_deref(), Some("less"));
}

#[test]
fn agent_timeout_interrupts() {
    let _g = serial();
    let mut sh = shell();
    let start = Instant::now();
    let r = sh
        .run_agent_command(
            "echo start; sleep 20; echo never",
            &AgentExecOpts {
                timeout: Duration::from_millis(500),
                ..AgentExecOpts::default()
            },
            &mut NullSink,
        )
        .unwrap();
    assert!(r.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(r.stdout.starts_with("start"));
    assert!(!r.stdout.contains("never"));
    // The session is still usable.
    assert_eq!(agent(&mut sh, "echo ok").stdout, "ok\n");
}

#[test]
fn capture_is_bounded() {
    let _g = serial();
    let mut sh = shell();
    let r = sh
        .run_agent_command(
            "head -c 100000 /dev/zero | tr '\\0' x",
            &AgentExecOpts {
                capture_limit: 1000,
                ..AgentExecOpts::default()
            },
            &mut NullSink,
        )
        .unwrap();
    assert!(r.truncated);
    assert_eq!(r.stdout.len(), 1000);
}

#[derive(Default)]
struct RecordingAi {
    requests: Vec<AiRequest>,
    reply_prefill: Option<String>,
}

impl AiHandler for RecordingAi {
    fn handle(&mut self, _: &mut EmbeddedShell, req: AiRequest) -> AiOutcome {
        self.requests.push(req);
        AiOutcome {
            prefill: self.reply_prefill.clone(),
            exit_code: 0,
        }
    }
    fn builtin(&mut self, _: &mut EmbeddedShell, args: &[String]) -> AiOutcome {
        self.requests.push(AiRequest {
            trigger: Trigger::Builtin,
            text: format!("builtin:{}", args.join(" ")),
            failed: None,
        });
        AiOutcome::default()
    }
    fn suggest(&mut self, _: &mut EmbeddedShell, _: &str) -> Option<String> {
        None
    }
    fn badge(&self) -> Badge {
        Badge::default()
    }
}

struct ScriptUi {
    guard: GuardChoice,
    notices: Vec<String>,
}

impl ReplUi for ScriptUi {
    fn guard(&mut self, _: &str) -> GuardChoice {
        self.guard
    }
    fn notice(&mut self, msg: &str) {
        self.notices.push(msg.to_string());
    }
}

#[test]
fn pipeline_routes_lines() {
    let _g = serial();
    let mut sh = shell();
    let mut ai = RecordingAi::default();
    let mut ui = ScriptUi {
        guard: GuardChoice::Cancel,
        notices: Vec::new(),
    };
    let mut p = Pipeline::new(ReplConfig::default());

    assert_eq!(
        p.process(&mut sh, &mut ai, &mut ui, "# list big files"),
        LineOutcome::Continue(None)
    );
    assert_eq!(ai.requests.len(), 1);
    assert_eq!(ai.requests[0].trigger, Trigger::Hash);
    assert_eq!(ai.requests[0].text, "list big files");

    // Guard cancel puts the line back for editing, nothing runs.
    assert_eq!(
        p.process(&mut sh, &mut ai, &mut ui, "rm all the temp files"),
        LineOutcome::Continue(Some("rm all the temp files".into()))
    );
    assert_eq!(ai.requests.len(), 1);

    // A failing command gets a hint, not an AI call.
    let r = p.process(
        &mut sh,
        &mut ai,
        &mut ui,
        "ls /definitely/not/here 2>/dev/null",
    );
    assert_eq!(r, LineOutcome::Continue(None));
    assert_eq!(ai.requests.len(), 1);
    assert!(
        ui.notices.last().unwrap().contains("exit 2"),
        "{:?}",
        ui.notices
    );
    assert_eq!(p.last_failure().map(|c| c.exit), Some(2));

    // Bare `#` asks about the failure.
    p.process(&mut sh, &mut ai, &mut ui, "#");
    assert_eq!(ai.requests.len(), 2);
    assert_eq!(ai.requests[1].trigger, Trigger::Failed { exit: 2 });
    assert!(ai.requests[1].failed.is_some());

    // Chinese in a failing line goes straight to the AI.
    p.process(&mut sh, &mut ai, &mut ui, "ls /不存在的目录 2>/dev/null");
    assert_eq!(ai.requests.len(), 3);
    assert_eq!(ai.requests[2].trigger, Trigger::Failed { exit: 2 });

    // grep's "no match" is not a failure.
    let n = ui.notices.len();
    p.process(&mut sh, &mut ai, &mut ui, "echo a | grep -q zzz");
    assert_eq!(ui.notices.len(), n);

    // `ai` builtin: management subcommands and tasks.
    p.process(&mut sh, &mut ai, &mut ui, "ai mode auto");
    assert_eq!(ai.requests[3].text, "builtin:mode auto");
    p.process(&mut sh, &mut ai, &mut ui, "ai \"count lines of code\"");
    assert_eq!(ai.requests[4].trigger, Trigger::Builtin);
    assert_eq!(ai.requests[4].text, "count lines of code");

    // `exit` ends the session with its status.
    assert_eq!(
        p.process(&mut sh, &mut ai, &mut ui, "exit 7"),
        LineOutcome::Exit(7)
    );
}

#[test]
fn pipeline_corrects_without_running() {
    let _g = serial();
    if !which("git") {
        return;
    }
    let mut sh = shell();
    let mut ai = RecordingAi::default();
    let mut ui = ScriptUi {
        guard: GuardChoice::Cancel,
        notices: Vec::new(),
    };
    let mut p = Pipeline::new(ReplConfig::default());
    let r = p.process(&mut sh, &mut ai, &mut ui, "gti status");
    assert_eq!(r, LineOutcome::Continue(Some("git status".into())));
    assert!(ai.requests.is_empty());
    assert!(sh.recent_commands().is_empty(), "nothing may run");
}

/// A sink as slow as a terminal display: it must not hold up the timeout.
struct SlowSink(usize);

impl nosh_shell::OutputSink for SlowSink {
    fn stdout(&mut self, chunk: &str) {
        self.0 += chunk.len();
        std::thread::sleep(Duration::from_micros(300));
    }
    fn stderr(&mut self, _: &str) {}
}

#[test]
fn fast_output_does_not_block_the_timeout() {
    let _g = serial();
    let mut sh = shell();
    let start = Instant::now();
    let mut sink = SlowSink(0);
    let r = sh
        .run_agent_command(
            "yes",
            &AgentExecOpts {
                timeout: Duration::from_secs(1),
                capture_limit: 1 << 20,
            },
            &mut sink,
        )
        .unwrap();
    assert!(r.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(5),
        "{:?}",
        start.elapsed()
    );
    assert!(r.truncated);
    assert!(r.stdout.len() <= 1 << 20);
    assert!(
        sink.0 <= 1 << 20,
        "the display gets no more than the capture"
    );
}

/// Processes whose command line contains `needle` (from /proc).
fn processes_with(needle: &str) -> usize {
    std::fs::read_dir("/proc")
        .unwrap()
        .flatten()
        .filter(|e| {
            std::fs::read(e.path().join("cmdline"))
                .map(|c| {
                    String::from_utf8_lossy(&c)
                        .replace('\0', " ")
                        .contains(needle)
                })
                .unwrap_or(false)
        })
        .count()
}

#[test]
fn timeout_stops_substitutions_and_pipeline_stages() {
    let _g = serial();
    let mut sh = shell();
    for (cmd, needle) in [
        ("x=$(sleep 31.25); echo done", "sleep 31.25"),
        ("echo hi | sleep 32.25", "sleep 32.25"),
    ] {
        let r = sh
            .run_agent_command(
                cmd,
                &AgentExecOpts {
                    timeout: Duration::from_millis(500),
                    ..AgentExecOpts::default()
                },
                &mut NullSink,
            )
            .unwrap();
        assert!(r.timed_out, "{cmd}");
        let deadline = Instant::now() + Duration::from_secs(4);
        while processes_with(needle) > 0 && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(processes_with(needle), 0, "{cmd} left a process behind");
    }
}

#[test]
fn builtin_only_loops_time_out() {
    let _g = serial();
    // Agent shells (interactive or `nosh -a`) catch SIGINT and make builtins
    // interruptible.
    let mut sh = EmbeddedShell::new(ShellOptions {
        catch_sigint: true,
        ..ShellOptions::default()
    })
    .unwrap();
    let start = Instant::now();
    let r = sh
        .run_agent_command(
            "while :; do :; done",
            &AgentExecOpts {
                timeout: Duration::from_millis(500),
                ..AgentExecOpts::default()
            },
            &mut NullSink,
        )
        .unwrap();
    assert!(r.timed_out);
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(agent(&mut sh, "echo ok").stdout, "ok\n");
}

#[test]
fn abandoned_functions_leave_no_variables_behind() {
    let _g = serial();
    let mut sh = shell();
    let path = sh.var("PATH");
    agent(
        &mut sh,
        "f() { sleep 30; }; outer() { local PATH=/nonexistent; inner; }; inner() { sleep 30; }",
    );
    let opts = AgentExecOpts {
        timeout: Duration::from_millis(400),
        ..AgentExecOpts::default()
    };
    assert!(
        sh.run_agent_command("NOSH_LEAK=bar f", &opts, &mut NullSink)
            .unwrap()
            .timed_out
    );
    assert_eq!(sh.var("NOSH_LEAK"), None);
    assert!(
        sh.run_agent_command("outer", &opts, &mut NullSink)
            .unwrap()
            .timed_out
    );
    assert_eq!(sh.var("PATH"), path);
    let r = agent(
        &mut sh,
        "echo ${NOSH_LEAK:-unset}; ls / >/dev/null && echo ls-ok",
    );
    assert_eq!(r.stdout, "unset\nls-ok\n");
}

#[test]
fn background_jobs_keep_running_after_the_command_returns() {
    let _g = serial();
    let dir = tmpdir("bg");
    let marker = dir.join("marker");
    let mut sh = shell();
    let cmd = format!(
        "sh -c 'for i in 1 2 3 4 5 6; do echo tick; sleep 0.25; done; touch {}' &",
        marker.display()
    );
    let r = agent(&mut sh, &cmd);
    assert_eq!(r.exit_code, 0);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(marker.exists(), "the background job was killed by SIGPIPE");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ctrl_c_ends_a_builtin_loop_typed_at_the_prompt() {
    let _g = serial();
    let mut sh = EmbeddedShell::new(ShellOptions {
        interactive: true,
        ..ShellOptions::default()
    })
    .unwrap();
    sh.run_user_line("f() { local NOSH_LOCAL=1; while :; do :; done; }");
    let ints = sh.interrupts();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        ints.fire();
    });
    let start = Instant::now();
    let run = sh.run_user_line("f");
    assert_eq!(run.exit_code, 130);
    assert!(start.elapsed() < Duration::from_secs(3));
    assert_eq!(sh.var("NOSH_LOCAL"), None);
    assert_eq!(sh.run_user_line("true").exit_code, 0);
}
