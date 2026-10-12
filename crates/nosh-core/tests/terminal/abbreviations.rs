use super::support::*;
use nosh_shell::{AiHandler, AiOutcome, AiRequest, Badge, EmbeddedShell};
use std::os::unix::fs::PermissionsExt;

#[derive(Default)]
struct AbbreviationAi {
    suggestions: usize,
    requests: usize,
}

impl AiHandler for AbbreviationAi {
    fn handle(&mut self, _: &mut EmbeddedShell, _: AiRequest) -> AiOutcome {
        self.requests += 1;
        AiOutcome::default()
    }
    fn command(&mut self, _: &mut EmbeddedShell, _: nosh_shell::ManagementCommand) -> AiOutcome {
        self.requests += 1;
        AiOutcome::default()
    }
    fn suggest(&mut self, _: &mut EmbeddedShell, _: &str) -> Option<String> {
        self.suggestions += 1;
        Some("echo EXPLICIT_AI".into())
    }
    fn badge(&self) -> Badge {
        Badge::default()
    }
}

pub(super) fn probe(mode: &str) {
    let home = std::path::PathBuf::from(std::env::var_os("NOSH_HOME").unwrap());
    let bin = home.join("abbr-bin");
    std::fs::create_dir(&bin).unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        interactive: true,
        working_dir: Some(home.clone()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(
        shell
            .run_user_line(&format!(
                "PATH='{}'; PS1='abbr> '; PROMPT_COMMAND='PROMPTS=$(( ${{PROMPTS:-0}} + 1 ))'",
                bin.display()
            ))
            .exit_code,
        0
    );
    let mut config = nosh_shell::ReplConfig {
        abbreviations: nosh_shell::abbreviations::Config {
            builtins: false,
            rules: [
                (
                    "zzq".into(),
                    nosh_shell::abbreviations::Rule {
                        expansion: "echo ABBR_DONE".into(),
                        override_existing: false,
                    },
                ),
                (
                    "zzr".into(),
                    nosh_shell::abbreviations::Rule {
                        expansion: "echo SECOND_DONE".into(),
                        override_existing: false,
                    },
                ),
            ]
            .into_iter()
            .collect(),
            ..Default::default()
        },
        abbreviation_worker: Some(input_worker_command()),
        input_assist: nosh_shell::input_assist::Config {
            enabled: false,
            worker: None,
        },
        completion: nosh_shell::completion::Config {
            worker: Some(input_worker_command()),
            ..Default::default()
        },
        editing: nosh_shell::editing::Config {
            mode: if mode.ends_with("-vi") {
                nosh_shell::editing::Mode::Vi
            } else {
                nosh_shell::editing::Mode::Emacs
            },
            ..Default::default()
        },
        on_failure: nosh_shell::OnFailure::Auto,
        command_assist: false,
        status_bar: nosh_shell::status::Config {
            enabled: false,
            ..Default::default()
        },
        ..Default::default()
    };
    if mode.contains("-paste") {
        config.trigger.trigger_on_error = false;
        config.on_failure = nosh_shell::OnFailure::Off;
    }
    if mode.contains("-feedback") {
        config.input_assist = nosh_shell::input_assist::Config {
            enabled: true,
            worker: Some(input_worker_command()),
        };
    }
    if mode.contains("-slow") {
        config.abbreviations.builtins = true;
        let path = bin.join("git");
        std::fs::write(
            &path,
            "#!/bin/sh\nprintf '%s\\n' $$ > abbr-bin/started\nexec /bin/sleep 60\n",
        )
        .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    shell.add_history("zzq");
    let mut ai = AbbreviationAi::default();
    let code = nosh_shell::repl::run(&mut shell, &mut ai, config);
    assert_eq!(code, 0);
    println!(
        "abbreviation-result:{}",
        serde_json::json!({
            "suggestions": ai.suggestions, "requests": ai.requests,
            "commands": shell.recent_commands().iter().map(|c| c.line.as_str()).collect::<Vec<_>>(),
            "prompts": shell.var("PROMPTS"),
        })
    );
}

fn run(mode: &str, steps: &[KeyStep<'_>]) -> (serde_json::Value, String, ProbeTimings) {
    let (out, err, timings) = Probe {
        mode,
        stdout_tty: true,
        stderr_tty: true,
        input_assist: false,
        no_color: "1",
        steps,
        ..Default::default()
    }
    .run_with_timings();
    let text = style::strip_ansi(&(out + &err));
    let result = text
        .split_once("abbreviation-result:")
        .unwrap_or_else(|| panic!("{text}"))
        .1
        .lines()
        .next()
        .unwrap();
    (serde_json::from_str(result).unwrap(), text, timings)
}

#[test]
fn abbreviation_space_undo_and_literal_submit_do_not_call_ai() {
    for mode in ["repl-abbreviation-emacs", "repl-abbreviation-vi"] {
        let (result, _, _) = run(
            mode,
            &[
                ("@delay:1800", b"zzq "),
                ("echo ABBR_DONE ", b"\x1a"),
                ("Input kept literal", b" \r"),
                ("no automatic AI", b"exit 0\r"),
            ],
        );
        assert_eq!(result["requests"], 0);
        assert_eq!(result["suggestions"], 0);
        assert!(
            result["commands"]
                .as_array()
                .unwrap()
                .iter()
                .any(|c| c == "zzq")
        );
    }
}

#[test]
fn abbreviation_enter_and_multiple_command_positions_expand_once() {
    let (result, _, _) = run(
        "repl-abbreviation-enter",
        &[
            ("@delay:1800", b"zzq\r"),
            ("ABBR_DONE", b"zzq;zzr\r"),
            ("SECOND_DONE", b"exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 0);
    assert!(
        result["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "echo ABBR_DONE;echo SECOND_DONE")
    );
}

#[test]
fn abbreviation_ctrl_space_uses_real_nul_and_preserves_explicit_ai() {
    let (result, _, _) = run(
        "repl-abbreviation-literal",
        &[
            ("@delay:1800", b"zzq\0"),
            ("Input kept literal", b"\x1bOQ"),
            ("echo EXPLICIT_AI", b"\r"),
            ("EXPLICIT_AI", b"zzq\0"),
            ("Input kept literal", b"\r"),
            ("no automatic AI", b"exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 0);
    assert_eq!(result["suggestions"], 1);
}

#[test]
fn abbreviation_paste_is_protected_but_tab_can_explicitly_expand() {
    let (result, _, _) = run(
        "repl-abbreviation-paste",
        &[
            ("@delay:1800", b"\x1b[200~zzq\x1b[201~ \r"),
            ("command not found: zzq", b"\x1b[200~zzq\x1b[201~\t"),
            ("echo ABBR_DONE ", b"\r"),
            ("ABBR_DONE", b"exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 0);
    assert!(
        result["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "zzq")
    );
    assert!(
        result["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "echo ABBR_DONE")
    );
}

#[test]
fn abbreviation_slow_probe_does_not_block_input_or_exit() {
    let (result, _, timings) = run(
        "repl-abbreviation-slow",
        &[
            ("@file:abbr-bin/started", b"echo RESPONSIVE\r"),
            ("RESPONSIVE", b"exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 0);
    assert!(timings.exit < Duration::from_secs(2), "{:?}", timings.exit);
}

#[test]
fn abbreviation_input_feedback_completion_and_literal_undo_share_one_rule() {
    let (result, _, _) = run(
        "repl-abbreviation-feedback",
        &[
            ("@delay:1800", b"zzq"),
            ("echo ABBR_DONE", b"\t"),
            ("echo ABBR_DONE ", b"\x1a"),
            ("Input kept literal", b"\r"),
            ("no automatic AI", b"exit 0\r"),
        ],
    );
    assert_eq!(result["requests"], 0);
    assert_eq!(result["suggestions"], 0);
    assert!(
        result["commands"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == "zzq")
    );
}
