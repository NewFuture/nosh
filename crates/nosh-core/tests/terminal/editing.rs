use super::support::*;
use nosh_shell::{AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell};

const F2: &[u8] = b"\x1bOQ";
const F3: &[u8] = b"\x1bOR";

struct DraftAi {
    requests: usize,
    agents: usize,
    answer: Option<String>,
}

impl AiHandler for DraftAi {
    fn handle(&mut self, _: &mut EmbeddedShell, _: AiRequest) -> AiOutcome {
        self.agents += 1;
        AiOutcome::default()
    }

    fn builtin(&mut self, _: &mut EmbeddedShell, _: &[String]) -> AiOutcome {
        self.agents += 1;
        AiOutcome::default()
    }

    fn suggest(&mut self, _: &mut EmbeddedShell, _: &str) -> Option<String> {
        self.requests += 1;
        eprintln!("editing-suggest-request-{}", self.requests);
        self.answer.clone()
    }

    fn badge(&self) -> Badge {
        Badge::default()
    }
}

pub(super) fn probe(mode: &str) {
    let directory = tempfile::tempdir().unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        interactive: true,
        working_dir: Some(directory.path().into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        shell.run_user_line(
            "PATH=/usr/bin:/bin; PS1='editing> '; PROMPT_COMMAND='PROMPTS=$(( ${PROMPTS:-0} + 1 ))'; candidate_one() { :; }; candidate_two() { :; }"
        ).exit_code,
        0,
    );
    shell.add_history("touch history_executed");
    let mut ai = DraftAi {
        requests: 0,
        agents: 0,
        answer: (!mode.ends_with("-none")).then(|| "touch generated".into()),
    };
    let mut config = nosh_shell::ReplConfig {
        editing: nosh_shell::editing::Config {
            mode: if mode.contains("-vi") {
                nosh_shell::editing::Mode::Vi
            } else {
                nosh_shell::editing::Mode::Emacs
            },
            ..Default::default()
        },
        input_assist: nosh_shell::input_assist::Config {
            enabled: false,
            worker: None,
        },
        status_bar: nosh_shell::status::Config {
            enabled: false,
            ..Default::default()
        },
        command_assist: false,
        ..Default::default()
    };
    if mode.ends_with("-remap") {
        config
            .editing
            .keybindings
            .actions
            .insert("ai_suggest".into(), vec!["F3".into()]);
    }
    if mode.ends_with("-enhanced") {
        config
            .editing
            .keybindings
            .actions
            .insert("redo".into(), vec!["Ctrl+Shift+Z".into()]);
    }
    let code = nosh_shell::repl::run(&mut shell, &mut ai, config);
    assert_eq!(code, 0);
    let snapshot = shell.snapshot();
    println!(
        "editing-result:{}",
        serde_json::json!({
            "requests": ai.requests,
            "agents": ai.agents,
            "generated": directory.path().join("generated").exists(),
            "history_executed": directory.path().join("history_executed").exists(),
            "prompts": snapshot.var("PROMPTS").unwrap().parse::<usize>().unwrap(),
            "commands": shell.recent_commands().iter().map(|command| command.line.as_str()).collect::<Vec<_>>(),
        })
    );
}

fn run(mode: &str, terminal: &str, steps: &[KeyStep<'_>]) -> serde_json::Value {
    run_with_keyboard(mode, terminal, false, steps)
}

fn run_with_keyboard(
    mode: &str,
    terminal: &str,
    enhanced_keyboard: bool,
    steps: &[KeyStep<'_>],
) -> serde_json::Value {
    let (out, _) = Probe {
        mode,
        terminal: Some(terminal),
        stdout_tty: true,
        stderr_tty: true,
        input_assist: false,
        no_color: "1",
        enhanced_keyboard,
        steps,
        ..Default::default()
    }
    .run();
    let out = style::strip_ansi(&out);
    let result = out
        .split_once("editing-result:")
        .unwrap_or_else(|| panic!("{out:?}"))
        .1;
    serde_json::from_str(result.lines().next().unwrap()).unwrap()
}

#[test]
fn real_enhanced_events_work_only_after_capability_negotiation() {
    let result = run_with_keyboard(
        "repl-editing-enhanced",
        "xterm-256color",
        true,
        &[
            ("editing> ", b"rewrite_this_fixture"),
            ("rewrite_this_fixture", F2),
            ("touch generated", b"\x1a"),
            ("rewrite_this_fixture", b"\x1b[122;6u"),
            ("touch generated", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 1);
    assert_eq!(result["generated"], false);
    let result = run_with_keyboard(
        "repl-editing-enhanced",
        "xterm-256color",
        false,
        &[
            ("using defaults", b"rewrite_this_fixture"),
            ("rewrite_this_fixture", F2),
            ("touch generated", b"\x1a"),
            ("rewrite_this_fixture", b"\x19"),
            ("touch generated", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 1);
    assert_eq!(result["generated"], false);
}

#[test]
fn f2_prefill_undo_redo_never_executes_or_repeats_prompt_hooks() {
    let result = run(
        "repl-editing",
        "xterm-256color",
        &[
            ("editing> ", b"rewrite_this_fixture"),
            ("rewrite_this_fixture", F2),
            ("touch generated", b"\x1f"),
            ("rewrite_this_fixture", b"\x19"),
            ("touch generated", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 1);
    assert_eq!(result["agents"], 0);
    assert_eq!(result["generated"], false);
    assert_eq!(result["prompts"], 1);
}

#[test]
fn no_suggestion_retains_a_mid_buffer_cursor_and_original_draft() {
    let result = run(
        "repl-editing-none",
        "xterm-256color",
        &[
            ("editing> ", b"printf original_ab"),
            ("original_ab", b"\x1b[D"),
            ("original_a", F2),
            ("draft unchanged", b"X\x05\r"),
            ("editing> ", b"exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 1);
    assert_eq!(result["generated"], false);
    assert_eq!(result["agents"], 0);
    assert_eq!(result["prompts"], 2);
    assert!(
        result["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|command| command == "printf original_aXb")
    );
}

#[test]
fn tab_only_requests_ai_when_completion_is_definitively_exhausted() {
    let result = run(
        "repl-editing",
        "xterm-256color",
        &[
            ("editing> ", b"nosh_no_completion_match_fixture\t"),
            ("touch generated", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 1);
    assert_eq!(result["generated"], false);
    let result = run(
        "repl-editing",
        "xterm-256color",
        &[
            ("editing> ", b"candidate_\t"),
            ("candidate_two", F2),
            ("exit or cancel", b"\x1b"),
            ("editing> ", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 0);
    assert_eq!(result["agents"], 0);
}

#[test]
fn history_enter_submits_but_escape_and_cancel_do_not() {
    let result = run(
        "repl-editing",
        "xterm-256color",
        &[
            ("editing> ", b"\x12history_executed"),
            ("touch history_executed", b"\r"),
            ("editing> ", b"exit 0\r"),
        ],
    );
    assert_eq!(result["history_executed"], true);
    assert_eq!(result["requests"], 0);
    let result = run(
        "repl-editing",
        "xterm-256color",
        &[
            ("editing> ", b"printf original"),
            ("original", b"\x12history_executed"),
            ("touch history_executed", F2),
            ("exit or cancel", b"\x1b"),
            ("touch history_executed", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["history_executed"], false);
    assert_eq!(result["requests"], 0);
    assert_eq!(result["prompts"], 1);
    let result = run(
        "repl-editing",
        "xterm-256color",
        &[
            ("editing> ", b"printf original"),
            ("original", b"\x12no_history_match_fixture"),
            ("failing reverse-i-search", b"\r"),
            ("failing reverse-i-search", b"\x07"),
            ("original", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["history_executed"], false);
    assert_eq!(result["requests"], 0);
    assert_eq!(result["prompts"], 1);
}

#[test]
fn configured_ai_key_and_basic_terminal_share_the_same_no_execution_boundary() {
    let result = run(
        "repl-editing-remap",
        "xterm-256color",
        &[
            ("editing> ", b"rewrite_this_fixture"),
            ("rewrite_this_fixture", b"\x1bOQq"),
            ("rewrite_this_fixtureq", F3),
            ("touch generated", b"\x15exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 1);
    assert_eq!(result["generated"], false);
    for terminal in ["xterm-256color", "dumb"] {
        let result = run(
            "repl-editing",
            terminal,
            &[("editing> ", F2), ("no ready suggestion", b"exit 0\r")],
        );
        assert_eq!(result["requests"], 0);
        assert_eq!(result["agents"], 0);
    }
}

#[test]
fn vi_insert_normal_search_and_ai_resumption_keep_the_real_mode() {
    let result = run(
        "repl-editing-vi",
        "xterm-256color",
        &[
            ("[I]", b"printf vi_a"),
            ("vi_a", b"\x1b"),
            ("[N]", b"x"),
            ("[N]", b"u"),
            ("[N]", b"\x12history_executed"),
            ("touch history_executed", b"\x07"),
            ("[N]", F2),
            ("touch generate", b"\x1a"),
            ("[N]", b"\x19"),
            ("touch generate", b"\x03"),
            ("[N]", b"i"),
            ("[I]", b"exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 1);
    assert_eq!(result["generated"], false);
    assert_eq!(result["history_executed"], false);
    assert_eq!(result["agents"], 0);
    assert_eq!(result["prompts"], 2);
}
