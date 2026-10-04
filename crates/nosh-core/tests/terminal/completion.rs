use super::support::*;
use nosh_shell::{AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell};

const F3: &[u8] = b"\x1bOR";
const F4: &[u8] = b"\x1bOS";
const F5: &[u8] = b"\x1b[15~";
const F6: &[u8] = b"\x1b[17~";

#[derive(Default)]
struct CompletionAi {
    requests: usize,
    agents: usize,
}

impl AiHandler for CompletionAi {
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
        eprintln!("completion-ai-request-{}", self.requests);
        Some("echo generated".into())
    }

    fn badge(&self) -> Badge {
        Badge::default()
    }
}

pub(super) fn probe(mode: &str) {
    let home = std::path::PathBuf::from(std::env::var_os("NOSH_HOME").unwrap());
    let directory = home.join("completion");
    std::fs::create_dir(&directory).unwrap();
    for name in ["my-project", "main-playground"] {
        std::fs::create_dir(directory.join(name)).unwrap();
    }
    for name in [
        "résumé 中 space.txt",
        "résumé 中 spare.txt",
        "tab\t中 name",
        "tab\t中 note",
        "line\n中 name",
        "line\n中 note",
        "native_candidate",
    ] {
        std::fs::write(directory.join(name), "").unwrap();
    }
    std::fs::write(
        directory.join("Makefile"),
        "OnlyTarget:\n\t@touch make_executed\n$(unknown):\n",
    )
    .unwrap();
    if mode.contains("-package") {
        use std::os::unix::fs::PermissionsExt;
        let bin = directory.join("bin");
        std::fs::create_dir(&bin).unwrap();
        for name in ["npm", "yarn"] {
            let executable = bin.join(name);
            std::fs::write(&executable, "#!/bin/sh\n: > manager_executed\n").unwrap();
            std::fs::set_permissions(executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            directory.join("package.json"),
            r#"{"scripts":{"task space":"touch script_executed"}}"#,
        )
        .unwrap();
    }
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        interactive: true,
        working_dir: Some(directory.clone()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        shell
            .run_user_line(
                r#"
PATH=/nonexistent; PS1='completion> '; PROMPT_COMMAND='PROMPTS=$(( ${PROMPTS:-0} + 1 ))'
nosh_Completion_Alpha() { : > executed; }
nosh_Completion_Alpine() { : > executed; }
capture() { CAPTURED=("$@"); CAPTURED_FIRST=$1; CAPTURED_SECOND=$2; : > executed; }
fixture_one() { : > executed; }
fixture_many() { : > executed; }
fixture_background() { : > executed; }
fixture_loop() { : > executed; }
fixture_sleep() { : > executed; }
fixture_failed() { : > executed; }
fixture_empty() { : > executed; }
_one() {
    printf 'call\n' >> calls_one; : > provider_started
    while [[ ! -f release ]]; do :; done
    COMPREPLY=(UniQueValue); compopt -o noquote -o nospace
}
_many() {
    printf 'call\n' >> calls_many
    COMPREPLY=(Zebra Alpha Delta); compopt -o nosort -o noquote -o nospace
}
_background() {
    printf 'call\n' >> calls_background
    if [[ ${COMP_WORDS[1]} == oldx ]]; then
        : > provider_started
        while [[ ! -f release ]]; do :; done
        COMPREPLY=(oldx-only)
    else
        COMPREPLY=(Zebra Alpha)
    fi
    compopt -o noquote -o nospace
}
_loop() {
    printf 'call\n' >> calls_slow; : > provider_started
    while :; do :; done
    COMPREPLY=(late-only)
}
_sleep() {
    printf 'call\n' >> calls_slow
    /bin/sh -c 'echo $$ > sleep_pid; : > provider_started; exec /bin/sleep 10'
    : > late_result; COMPREPLY=(late-only)
}
_empty() { printf 'call\n' >> calls_empty; COMPREPLY=(); }
complete -F _one fixture_one
complete -F _many fixture_many
complete -F _background fixture_background
complete -F _loop fixture_loop
complete -F _sleep fixture_sleep
complete -F missing_fixture_provider fixture_failed
complete -F _empty fixture_empty
"#
            )
            .exit_code,
        0,
    );
    if mode.contains("-partial") {
        assert_eq!(shell.run_user_line("PATH=/usr/bin:/bin").exit_code, 0);
    }
    if mode.contains("-package") {
        assert_eq!(
            shell
                .run_user_line(&format!("PATH='{}'", directory.join("bin").display()))
                .exit_code,
            0
        );
    }
    let mut config = nosh_shell::ReplConfig {
        completion: nosh_shell::completion::Config {
            worker: Some(input_worker_command()),
            ..Default::default()
        },
        input_assist: nosh_shell::input_assist::Config {
            enabled: false,
            worker: None,
        },
        editing: nosh_shell::editing::Config {
            mode: if mode.contains("-vi") {
                nosh_shell::editing::Mode::Vi
            } else {
                nosh_shell::editing::Mode::Emacs
            },
            ..Default::default()
        },
        status_bar: nosh_shell::status::Config {
            enabled: mode.contains("-status"),
            ..Default::default()
        },
        command_assist: false,
        ..Default::default()
    };
    let abbreviation_selections = Arc::new(Mutex::new(Vec::new()));
    if mode.contains("-abbreviation") {
        config.input_abbreviations = nosh_shell::input_assist::Abbreviations {
            revision: 7,
            applicable: ["gco", "gcp"].into_iter().map(str::to_owned).collect(),
            definitions: [
                (
                    "gco".into(),
                    nosh_shell::input_assist::Abbreviation {
                        expansion: "echo gc".into(),
                        source: "fixture".into(),
                    },
                ),
                (
                    "gcp".into(),
                    nosh_shell::input_assist::Abbreviation {
                        expansion: "echo gs".into(),
                        source: "fixture".into(),
                    },
                ),
            ]
            .into_iter()
            .collect(),
        };
        let selections = abbreviation_selections.clone();
        config.completion.selection_observer = Some(Arc::new(move |selection| {
            let nosh_shell::completion::Selection::Abbreviation(selection) = selection;
            selections
                .lock()
                .unwrap()
                .push((selection.name.clone(), selection.revision));
        }));
    }
    if mode.contains("-custom") {
        config
            .editing
            .keybindings
            .actions
            .insert("cut_to_start".into(), vec!["Ctrl+U".into()]);
        config
            .editing
            .keybindings
            .actions
            .insert("complete_or_ai".into(), vec!["F3".into()]);
        config.editing.keybindings.contexts.insert(
            "menu".into(),
            std::collections::BTreeMap::from([
                ("accept".into(), vec!["F4".into()]),
                ("cancel".into(), vec!["F5".into()]),
                ("complete".into(), vec!["F6".into()]),
            ]),
        );
    }
    let mut ai = CompletionAi::default();
    let code = nosh_shell::repl::run(&mut shell, &mut ai, config);
    assert_eq!(code, 0);
    if let Ok(pid) = std::fs::read_to_string(directory.join("sleep_pid")) {
        let pid: libc::pid_t = pid.trim().parse().unwrap();
        let deadline = Instant::now() + Duration::from_millis(300);
        while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(
            unsafe { libc::kill(pid, 0) },
            -1,
            "completion's external sleep survived cancellation",
        );
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
    }
    let calls = |name: &str| {
        std::fs::read_to_string(directory.join(name))
            .unwrap_or_default()
            .lines()
            .count()
    };
    println!(
        "completion-result:{}",
        serde_json::json!({
            "requests": ai.requests,
            "agents": ai.agents,
            "executed": directory.join("executed").exists(),
            "make_executed": directory.join("make_executed").exists(),
            "manager_executed": directory.join("manager_executed").exists(),
            "script_executed": directory.join("script_executed").exists(),
            "late_result": directory.join("late_result").exists(),
            "captured_first": shell.var("CAPTURED_FIRST"),
            "captured_second": shell.var("CAPTURED_SECOND"),
            "prompts": shell.var("PROMPTS").unwrap().parse::<usize>().unwrap(),
            "calls_one": calls("calls_one"),
            "calls_many": calls("calls_many"),
            "calls_background": calls("calls_background"),
            "calls_slow": calls("calls_slow"),
            "calls_empty": calls("calls_empty"),
            "abbreviation_selections": *abbreviation_selections.lock().unwrap(),
        })
    );
}

fn run(probe: Probe<'_>) -> (serde_json::Value, String, ProbeTimings) {
    let (out, err, timings) = probe.run_with_timings();
    let output = style::strip_ansi(&(out + &err));
    let (output, result) = output
        .split_once("completion-result:")
        .unwrap_or_else(|| panic!("{output:?}"));
    let result = serde_json::from_str(result.lines().next().unwrap()).unwrap();
    (result, output.to_owned(), timings)
}

fn fixture(mode: &str, steps: &[KeyStep<'_>]) -> (serde_json::Value, String, ProbeTimings) {
    run(Probe {
        mode,
        stdout_tty: true,
        stderr_tty: true,
        input_assist: false,
        no_color: "1",
        columns: 120,
        steps,
        ..Default::default()
    })
}

fn unexecuted(result: &serde_json::Value) {
    assert_eq!(result["executed"], false);
    assert_eq!(result["agents"], 0);
    assert_eq!(result["prompts"], 1);
}

#[test]
fn completion_command_and_path_acceptance() {
    let (result, _, _) = fixture(
        "repl-completion",
        &[
            ("completion> ", b"nca\t"),
            ("nosh_Completion_Alpine", b"\r"),
            ("completion> nosh_Completion_Alpha", b"\x1a"),
            ("completion> nca", b"\x15cd mp\t"),
            ("main-playground/", b"\r"),
            ("completion> cd my-project/", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["requests"], 0);
}

#[test]
fn completion_package_script_acceptance() {
    for command in ["npm run", "yarn run", "yarn"] {
        let draft = format!("{command} 'task s'\t");
        let accepted = format!("completion> {command} 'task space'");
        let (result, _, _) = fixture(
            "repl-completion-package",
            &[
                ("completion> ", draft.as_bytes()),
                (&accepted, b"\x15exit 0\r"),
            ],
        );
        unexecuted(&result);
        assert_eq!(result["manager_executed"], false);
        assert_eq!(result["script_executed"], false);
        assert_eq!(result["requests"], 0);
    }
}

#[test]
fn completion_line_middle_unicode_and_undo() {
    let (result, _, _) = fixture(
        "repl-completion",
        &[
            (
                "completion> ",
                "capture before 'rés中' after\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\t".as_bytes(),
            ),
            ("résumé 中 spare.txt", b"\r"),
            ("capture before 'résumé 中 space.txt' after", b"\x1a"),
            ("capture before 'rés中' after", b"\x19"),
            (
                "capture before 'résumé 中 space.txt' after",
                b"\x05\x15exit 0\r",
            ),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["requests"], 0);
}

#[test]
fn completion_real_tab_and_newline_filenames_round_trip_after_protected_acceptance() {
    for (query, display, inserted, filename) in [
        ("tb中", "tab\\t中 note", "$'tab\\t中 name'", "tab\t中 name"),
        (
            "ln中",
            "line\\n中 note",
            "$'line\\n中 name'",
            "line\n中 name",
        ),
    ] {
        let draft = format!("capture {query} tail\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\t");
        let accepted = format!("capture {inserted} tail");
        let (result, output, _) = fixture(
            "repl-completion",
            &[
                ("completion> ", draft.as_bytes()),
                (display, b"\r"),
                (&accepted, b"\r"),
                ("completion> ", b"exit 0\r"),
            ],
        );
        assert_eq!(result["captured_first"], filename);
        assert_eq!(result["captured_second"], "tail");
        assert_eq!(result["executed"], true);
        assert_eq!(result["prompts"], 2);
        assert_eq!(result["requests"], 0);
        assert_eq!(result["agents"], 0);
        assert!(output.contains(&accepted));
    }
}

#[test]
fn completion_abbreviation_acceptance() {
    for (query, selected) in [("gc\t", "gcp"), ("gco\t", "completion> echo gc")] {
        let (result, output, _) = fixture(
            "repl-completion-abbreviation",
            &[
                ("completion> ", query.as_bytes()),
                (selected, if query == "gc\t" { b"\r" } else { b"" }),
                ("@delay:50", b"\x15exit 0\r"),
            ],
        );
        unexecuted(&result);
        assert_eq!(result["requests"], 0);
        assert_eq!(
            result["abbreviation_selections"],
            serde_json::json!([["gco", 7]])
        );
        assert!(output.contains("completion> echo gc"));
    }
}

#[test]
fn completion_async_explicit_unique_is_authorized_but_navigation_revokes_it() {
    let (result, _, _) = fixture(
        "repl-completion",
        &[
            ("completion> ", b"fixture_one q\t"),
            (
                "@file:completion/provider_started",
                b"@file:completion/release",
            ),
            ("completion> fixture_one UniQueValue", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["calls_one"], 1);

    let (result, output, _) = fixture(
        "repl-completion",
        &[
            ("completion> ", b"fixture_one q\t"),
            ("@file:completion/provider_started", b"\x1b[C"),
            ("@delay:40", b"@file:completion/release"),
            ("UniQueValue", b"\x1b"),
            ("completion> fixture_one q", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["calls_one"], 1);
    assert!(!output.contains("fixture_one UniQueValue"));
}

#[test]
fn completion_idle_refresh_preserves_draft() {
    let (result, output, timings) = fixture(
        "repl-completion",
        &[
            ("completion> ", b"fixture_background old\t"),
            ("Alpha", b"x"),
            (
                "@file:completion/provider_started",
                b"@file:completion/release",
            ),
            ("oldx-only", b"\x1b[B\x1b[A\t"),
            ("@delay:450", b"\x1b"),
            ("completion> fixture_background oldx", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["calls_background"], 2);
    assert!(!output.contains("fixture_background oldx-only"));
    println!(
        "completion_idle_refresh_us={}",
        timings.input[2].as_micros()
    );
}

#[test]
fn completion_navigation_and_resize_repaint_do_not_rerun_or_resort_provider() {
    let (result, output, _) = run(Probe {
        mode: "repl-completion-status",
        stdout_tty: true,
        stderr_tty: true,
        input_assist: false,
        no_color: "1",
        columns: 120,
        track_frames: true,
        resizes: &[(2, 70), (3, 120)],
        steps: &[
            ("completion> ", b"fixture_many z\t"),
            ("Delta", b"\x1b[C"),
            (">Alpha", b""),
            ("@delay:80", b"\x1b[D\x1b[B\x1b[A"),
            ("@delay:400", b"\r"),
            ("completion> fixture_many Zebra", b"\x15exit 0\r"),
        ],
        ..Default::default()
    });
    unexecuted(&result);
    assert_eq!(result["calls_many"], 1);
    assert!(output.contains(">Zebra"));
}

#[test]
fn completion_partial_unique_has_visible_reason_with_status_off_and_explicit_accept() {
    let (result, output, _) = fixture(
        "repl-completion-partial",
        &[
            ("completion> ", b"make On\t"),
            (">OnlyTarget", b"\r"),
            ("completion> make OnlyTarget", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["make_executed"], false);
    assert_eq!(result["requests"], 0);
    assert!(output.contains("Partial completions"));
}

#[test]
fn completion_failure_and_empty_results() {
    let (result, _, _) = fixture(
        "repl-completion",
        &[
            ("completion> ", b"fixture_failed q\t"),
            ("Completion unavailable", b"\t\r"),
            ("completion> fixture_failed q", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["requests"], 0);

    let (result, _, _) = fixture(
        "repl-completion",
        &[
            ("completion> ", b"fixture_empty q\t"),
            ("NO RECORDS FOUND", b"\t"),
            ("completion> echo generated", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["requests"], 1);
    assert_eq!(result["calls_empty"], 1);
}

#[test]
fn completion_slow_builtin_and_external_provider_do_not_block_edit_cancel_or_exit() {
    for command in ["fixture_loop", "fixture_sleep"] {
        let initial = format!("{command} \t");
        let changed = format!("completion> {command} x");
        let (result, output, timings) = fixture(
            "repl-completion",
            &[
                ("completion> ", initial.as_bytes()),
                ("@file:completion/provider_started", b"x\x1b[B\x1b"),
                (&changed, b"\x15exit 0\r"),
            ],
        );
        unexecuted(&result);
        assert_eq!(result["calls_slow"], 1);
        assert_eq!(result["late_result"], false);
        assert_eq!(result["requests"], 0);
        assert!(!output.contains("native_candidate"));
        assert!(
            timings.input[2] < Duration::from_millis(500),
            "{command}: {:?}",
            timings.input[2]
        );
        assert!(
            timings.exit < Duration::from_millis(500),
            "{command} exit: {:?}",
            timings.exit,
        );
        println!(
            "completion_{command}_edit_cancel_us={} exit_us={}",
            timings.input[2].as_micros(),
            timings.exit.as_micros(),
        );
    }
}

#[test]
fn completion_cancel_suppresses_late_result_and_keeps_draft_after_deadline() {
    let (result, output, _) = fixture(
        "repl-completion",
        &[
            ("completion> ", b"fixture_sleep \t"),
            ("@file:completion/provider_started", b"\x1b"),
            ("completion> fixture_sleep", b""),
            ("@delay:1650", b"x"),
            ("completion> fixture_sleep x", b"\x15exit 0\r"),
        ],
    );
    unexecuted(&result);
    assert_eq!(result["calls_slow"], 1);
    assert_eq!(result["late_result"], false);
    assert!(!output.contains("late-only"));
    assert!(!output.contains("Completion unavailable"));
}

#[test]
fn completion_remapped_actions_in_emacs_and_vi() {
    for mode in ["repl-completion-custom", "repl-completion-vi-custom"] {
        let indicator = if mode.contains("-vi") { "[I] " } else { "" };
        let draft = format!("completion> {indicator}nca");
        let accepted = format!("completion> {indicator}nosh_Completion_Alpine");
        let (result, _, _) = fixture(
            mode,
            &[
                ("completion> ", b"nca"),
                (&draft, F3),
                ("nosh_Completion_Alpine", b"\r"),
                ("@delay:40", F6),
                (">nosh_Completion_Alpine", F4),
                (&accepted, b"\x1a"),
                (&draft, F3),
                ("nosh_Completion_Alpha", F5),
                (&draft, b"\x15exit 0\r"),
            ],
        );
        unexecuted(&result);
        assert_eq!(result["requests"], 0);
    }
}
