#![cfg(unix)]

#[path = "support/terminal.rs"]
mod terminal;

use std::ffi::OsStr;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use terminal::Terminal;

fn nosh(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .env("NOSH_HOME", home)
        .env("HOME", home)
        .env("NOSH_LANG", "en")
        .env_remove("NOSH_MODEL_PATH")
        .env_remove("NOSH_DISABLE_AI")
        .env_remove("NOSH_EVAL_WORKER")
        .env_remove("NOSH_EVAL_TRACE")
        .env_remove("NOSH_INTERNAL_PTY_FD");
    command
}

fn init(home: &Path) -> Vec<u8> {
    let output = nosh(home).args(["init", "bash"]).output().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    output.stdout
}

fn source_bash(program: impl AsRef<OsStr>, script: &[u8]) -> Output {
    let mut child = Command::new(program)
        .args(["--noprofile", "--norc"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(script).unwrap();
    child.wait_with_output().unwrap()
}

#[test]
fn init_bash_is_script_only_without_model_or_startup_side_effects() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join(".bashrc"), "touch rc-was-loaded").unwrap();
    fs::write(
        home.path().join("config.toml"),
        "[model]\npath = 'missing.gguf'\ndevice = 'cuda'\n",
    )
    .unwrap();
    let before = fs::read(home.path().join("config.toml")).unwrap();
    let output = nosh(home.path())
        .env(
            "NOSH_EVAL_WORKER",
            home.path().join("must-not-connect.sock"),
        )
        .env("NOSH_EVAL_TRACE", home.path().join("must-not-create.jsonl"))
        .args(["--safe", "--offline", "init", "bash"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
    let script = String::from_utf8(output.stdout).unwrap();
    assert!(script.starts_with("# nosh:"));
    assert!(script.contains("__nosh_bash_suggest()"));
    assert!(!script.contains("# NOSH_COMMAND"));
    assert!(!script.contains("# NOSH_BINDINGS"));
    assert_eq!(fs::read_dir(home.path()).unwrap().count(), 2);
    assert_eq!(fs::read(home.path().join("config.toml")).unwrap(), before);

    let output = nosh(home.path()).args(["init", "zsh"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("bash"));
}

#[test]
fn init_bash_shared_startup_script_does_not_install_readline_hooks_in_nosh() {
    let home = tempfile::tempdir().unwrap();
    let script = home.path().join("integration.sh");
    fs::write(&script, init(home.path())).unwrap();
    let output = nosh(home.path())
        .args([
            "--safe",
            "-c",
            "source \"$1\"; printf native; if declare -F __nosh_bash_suggest; then exit 99; fi",
            "check",
        ])
        .arg(script)
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"native");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn shell_flags_execute_init_named_script_instead_of_dispatching_init() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("init"), "printf script-ran\n").unwrap();
    for flag in ["-e", "-u", "-x", "-i", "-eux"] {
        let output = nosh(home.path())
            .current_dir(home.path())
            .args(["--safe", flag, "init"])
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(output.status.success(), "{flag}: {:?}", output.stderr);
        assert_eq!(output.stdout, b"script-ran", "{flag}");
    }
}

struct Bash {
    terminal: Terminal,
    home: tempfile::TempDir,
}

const SNAPSHOT: &str = r#"
__test_snapshot() {
    printf '%s\0%s\0' "$READLINE_LINE" "$READLINE_POINT" > snapshot
    printf '%s\0%s\0%s\0%s\0%s\0' "${BUSINESS-unset}" "${private_value-unset}" "${reply-unset}" "$PWD" "$-" > host-snapshot
    trap -p INT > trap-after
    printf '\n\123NAPSHOT_DONE\n'
}
bind -m emacs-standard -x '"\e[21~":__test_snapshot'
bind -m vi-insert -x '"\e[21~":__test_snapshot'
bind -m vi-command -x '"\e[21~":__test_snapshot'
"#;

impl Bash {
    fn new(config: &str, before: &str, after: &str, stub: bool) -> Self {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("config.toml"), config).unwrap();
        fs::write(home.path().join("integration.sh"), init(home.path())).unwrap();
        let stub = if stub {
            r#"
__nosh_bash_run() {
    printf '%s' "$1" > request
    printf 'call\n' >> calls
    case ${TEST_RESULT-success} in
        error) printf 'test diagnostic\n' >&2; return 2 ;;
        cancel) "$BASH" --noprofile --norc -c 'trap "exit 130" INT; printf "\nWORKING\n" >&2; sleep 30' ;;
        pending) printf '\nWORKING\n' >&2; sleep 1; cat response ;;
        empty) return 0 ;;
        *) cat response ;;
    esac
}
"#
        } else {
            ""
        };
        fs::write(
            home.path().join("rc"),
            format!(
                "PS1='BASH> '\nPS2='MORE> '\n{before}\nsource ./integration.sh\n{stub}\n{SNAPSHOT}\n{after}\n\
                 trap -p INT > trap-before\nprintf '\\n\\111NIT_DONE\\n'\n",
            ),
        )
        .unwrap();
        let mut command =
            Command::new(std::env::var_os("NOSH_TEST_BASH").unwrap_or_else(|| "bash".into()));
        command
            .args(["--noprofile", "--rcfile", "rc", "-i"])
            .current_dir(home.path())
            .env("HOME", home.path())
            .env("NOSH_HOME", home.path())
            .env("NOSH_LANG", "en")
            .env("INPUTRC", "/dev/null")
            .env("TERM", "xterm-256color")
            .env("PS1", "BASH> ")
            .env("PS2", "MORE> ")
            .env(
                "LC_ALL",
                if cfg!(target_os = "macos") {
                    "en_US.UTF-8"
                } else {
                    "C.UTF-8"
                },
            );
        for name in [
            "BASH_ENV",
            "BRUSH_VERSION",
            "ENV",
            "SHELLOPTS",
            "BASHOPTS",
            "NOSH_MODEL_PATH",
            "NOSH_DISABLE_AI",
            "NOSH_EVAL_WORKER",
            "NOSH_EVAL_TRACE",
            "NOSH_INTERNAL_PTY_FD",
        ] {
            command.env_remove(name);
        }
        let mut terminal = Terminal::new(command, 30, 160);
        terminal.wait_for("\nINIT_DONE");
        terminal.wait_for("BASH> ");
        assert!(
            !String::from_utf8_lossy(&terminal.output).contains("requires Bash"),
            "set NOSH_TEST_BASH to Bash 5.1+",
        );
        Self { terminal, home }
    }

    fn response(&self, text: &str) {
        fs::write(self.path("response"), text).unwrap();
    }

    fn path(&self, name: &str) -> PathBuf {
        self.home.path().join(name)
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.path(name)).unwrap()
    }

    fn command(&mut self, command: &str) {
        self.terminal
            .send(format!("{command}; printf '\\n\\103OMMAND_DONE\\n'\r").as_bytes());
        self.terminal.wait_for("\nCOMMAND_DONE");
        self.terminal.wait_for("BASH> ");
    }

    fn reload(&mut self, config: &str) {
        fs::write(self.path("config.toml"), config).unwrap();
        fs::write(self.path("integration.sh"), init(self.home.path())).unwrap();
        self.command("source ./integration.sh");
    }

    fn type_text(&mut self, text: &str) {
        self.terminal.send(
            text.replace('\t', "\x16\t")
                .replace('\n', "\x16\n")
                .as_bytes(),
        );
    }

    fn snapshot(&mut self) -> (String, usize) {
        self.terminal.send(b"\x1b[21~");
        self.terminal.wait_for("\nSNAPSHOT_DONE");
        self.terminal.wait_for("BASH> ");
        let text = self.read("snapshot");
        let mut fields = text.split('\0');
        (
            fields.next().unwrap().into(),
            fields.next().unwrap().parse().unwrap(),
        )
    }

    fn suggest(&mut self) {
        self.terminal.send(b"\x1bOQ");
    }

    fn clear(&mut self) {
        self.terminal.send(b"\x03");
        self.terminal.wait_for("BASH> ");
    }
}

#[test]
fn init_bash_suggestion_is_editable_and_not_executed_until_enter() {
    let mut bash = Bash::new("", "", "", true);
    let response = "printf '%s\\n' 'done' > created";
    bash.response(response);
    bash.type_text("create a file");
    bash.suggest();
    bash.terminal.wait_for(response);
    assert_eq!(bash.snapshot().0, response);
    assert!(!bash.path("created").exists());
    bash.type_text(" # checked");
    assert_eq!(bash.snapshot().0, format!("{response} # checked"));
    assert!(!bash.path("created").exists());
    bash.terminal.send(b"\r");
    bash.terminal.wait_for("BASH> ");
    assert_eq!(bash.read("created"), "done\n");
    assert_eq!(bash.read("calls"), "call\n");
}

#[test]
fn init_bash_quotes_unicode_and_multiline_descriptions_are_not_evaluated() {
    let mut bash = Bash::new("", "readonly LC_ALL", "", true);
    let draft = "-n \u{4e2d}\u{6587} 'quoted' \"double\" $(touch bad)\n--second";
    let response =
        "printf '%s\\n' '\u{4e2d}\u{6587} e\u{301} \u{1f469}\u{200d}\u{1f4bb}'\nprintf done";
    bash.response(response);
    bash.type_text(draft);
    assert_eq!(bash.snapshot().0, draft);
    bash.suggest();
    bash.terminal.wait_for("printf done");
    assert_eq!(bash.read("request"), draft);
    assert_eq!(bash.snapshot().0, response);
    bash.type_text("X");
    assert_eq!(
        bash.snapshot().0,
        format!("{response}X"),
        "cursor must be at the end"
    );
    assert!(!bash.path("bad").exists());
}

#[test]
fn init_bash_failure_empty_and_cancellation_restore_draft_and_cursor() {
    for result in ["error", "empty", "cancel"] {
        let mut bash = Bash::new(
            "",
            "set -eu\ntrap 'printf original-int > original-trap-ran' INT",
            &format!("TEST_RESULT={result}"),
            true,
        );
        bash.type_text("\u{4e2d}\u{6587} original");
        bash.terminal.send(b"\x02\x02");
        let before = bash.snapshot();
        bash.suggest();
        if result == "cancel" {
            bash.terminal.wait_for("\nWORKING");
            bash.terminal.send(b"\x03");
        }
        bash.terminal.wait_for("input unchanged");
        assert_eq!(bash.snapshot(), before, "{result}");
        assert_eq!(bash.read("trap-before"), bash.read("trap-after"));
        assert!(!bash.path("original-trap-ran").exists());
        bash.type_text("X");
        assert_eq!(bash.snapshot().0, "\u{4e2d}\u{6587} originXal");
    }
}

#[test]
fn init_bash_blank_input_does_not_request_suggestions() {
    let mut bash = Bash::new("", "", "", true);
    bash.type_text(" \t ");
    let before = bash.snapshot();
    bash.suggest();
    bash.terminal.wait_for("enter a description first");
    assert_eq!(bash.snapshot(), before);
    assert!(!bash.path("calls").exists());
}

#[test]
fn init_bash_queued_enter_cannot_execute_an_unseen_suggestion() {
    for coalesced in [false, true] {
        let mut bash = Bash::new("", "", "TEST_RESULT=pending", true);
        bash.response("touch generated");
        bash.type_text("printf original > accepted-original");
        if coalesced {
            bash.terminal.send(b"\x1bOQ\r");
        } else {
            bash.suggest();
            bash.terminal.wait_for("\nWORKING");
            bash.terminal.send(b"\r");
        }
        bash.terminal.wait_for("suggestion discarded");
        bash.snapshot();
        assert!(!bash.path("generated").exists());
        assert_eq!(bash.read("accepted-original"), "original");
    }
}

#[test]
fn init_bash_missing_model_uses_real_cli_without_destroying_input() {
    let mut bash = Bash::new("[download]\nauto = 'never'\n", "", "", false);
    bash.type_text("-find \u{4e2d}\u{6587}");
    bash.terminal.send(b"\x02");
    let before = bash.snapshot();
    bash.suggest();
    bash.terminal.wait_for("input unchanged");
    assert_eq!(bash.snapshot(), before);
    assert!(String::from_utf8_lossy(&bash.terminal.output).contains("not installed"));
}

#[test]
fn init_bash_default_preserves_existing_bindings() {
    for binding in [
        r#"bind -x '"\eOQ":printf host > host-ran'"#,
        r#"bind '"\eOQ":beginning-of-line'"#,
        r#"bind '"\eOQ":"HOST"'"#,
    ] {
        let mut bash = Bash::new("", binding, "", true);
        bash.type_text("draft");
        bash.suggest();
        let draft = bash.snapshot();
        assert!(!bash.path("calls").exists());
        assert!(
            String::from_utf8_lossy(&bash.terminal.output).contains("keeping existing binding")
        );
        assert!(draft.0.contains("draft"));
    }
}

#[test]
fn init_bash_does_not_hide_existing_key_sequences() {
    for (binding, keys) in [
        (r#"bind '"\eOQx":end-of-line'"#, b"\x1bOQx".as_slice()),
        (r#"bind '"\eO":"HOST"'"#, b"\x1bOQ".as_slice()),
    ] {
        let mut bash = Bash::new("", binding, "", true);
        bash.type_text("draft");
        bash.terminal.send(keys);
        assert!(bash.snapshot().0.contains("draft"));
        assert!(
            !bash.path("calls").exists(),
            "{binding}: {:?}",
            String::from_utf8_lossy(&bash.terminal.output),
        );
        assert!(String::from_utf8_lossy(&bash.terminal.output).contains("overlaps"));
    }
}

#[test]
fn init_bash_uses_existing_remapped_suggestion_action() {
    let mut bash = Bash::new("[shell.keybindings]\nai_suggest = ['F3']\n", "", "", true);
    bash.response("printf remapped");
    bash.type_text("draft");
    bash.suggest();
    assert_eq!(bash.snapshot().0, "draft");
    assert!(!bash.path("calls").exists());
    bash.terminal.send(b"\x1bOR");
    bash.terminal.wait_for("printf remapped");
    assert_eq!(bash.snapshot().0, "printf remapped");
}

#[test]
fn init_bash_missing_terminal_key_capability_is_explicit() {
    let mut bash = Bash::new("", "PATH=/nosh-test-missing-path", "", true);
    assert!(
        String::from_utf8_lossy(&bash.terminal.output).contains("terminal key kf2 unavailable")
    );
    bash.type_text("draft");
    bash.suggest();
    assert_eq!(bash.snapshot().0, "draft");
    assert!(!bash.path("calls").exists());
}

#[test]
fn init_bash_explicit_overrides_restore_on_unbind_and_respect_later_host_rebinds() {
    let config = "[shell.keybindings]\nai_suggest = ['F2']\n";
    let before = r#"bind -x '"\eOQ":printf "%s\n" "host quoted" > host-ran'"#;
    let mut bash = Bash::new(config, before, "", true);
    bash.response("printf suggested");
    bash.type_text("draft");
    bash.suggest();
    bash.terminal.wait_for("printf suggested");
    assert_eq!(bash.snapshot().0, "printf suggested");
    assert!(!bash.path("host-ran").exists());
    assert!(String::from_utf8_lossy(&bash.terminal.output).contains("ai_suggest replaces"));

    bash.clear();
    bash.reload("[shell.keybindings]\nai_suggest = []\n");
    bash.suggest();
    bash.snapshot();
    assert!(
        bash.path("host-ran").exists(),
        "{:?}",
        String::from_utf8_lossy(&bash.terminal.output),
    );
    assert_eq!(bash.read("host-ran"), "host quoted\n");

    let mut bash = Bash::new(config, "", "", true);
    bash.command(r#"bind -x '"\eOQ":printf later > later-ran'"#);
    bash.reload("[shell.keybindings]\nai_suggest = []\n");
    bash.suggest();
    bash.snapshot();
    assert!(bash.path("later-ran").exists());
}

#[test]
fn init_bash_restores_displaced_readline_functions_and_macros() {
    for (binding, expected_line, expected_point) in [
        (r#"bind '"\eOQ":beginning-of-line'"#, "draft", 0),
        (r#"bind '"\eOQ":"HOST"'"#, "draftHOST", 9),
    ] {
        let mut bash = Bash::new(
            "[shell.keybindings]\nai_suggest = ['F2']\n",
            binding,
            "",
            true,
        );
        bash.reload("[shell.keybindings]\nai_suggest = []\n");
        bash.type_text("draft");
        bash.suggest();
        assert_eq!(bash.snapshot(), (expected_line.into(), expected_point));
        assert!(!bash.path("calls").exists());
    }
}

#[test]
fn init_bash_equivalent_keys_keep_one_original_binding() {
    let mut bash = Bash::new(
        "[shell.keybindings]\nai_suggest = ['Ctrl+I', 'Tab']\n",
        r#"bind '"\C-i":"HOST"'"#,
        "",
        true,
    );
    bash.response("printf aliased");
    bash.type_text("draft");
    bash.terminal.send(b"\t");
    bash.terminal.wait_for("printf aliased");
    assert_eq!(bash.snapshot().0, "printf aliased");
    assert_eq!(bash.read("calls"), "call\n");
    bash.clear();
    bash.reload("[shell.keybindings]\nai_suggest = []\n");
    bash.type_text("draft");
    bash.terminal.send(b"\t");
    assert_eq!(bash.snapshot().0, "draftHOST");
}

#[test]
fn init_bash_reloading_and_vi_mode_preserve_host_state() {
    for mode in ["emacs", "vi"] {
        let mut bash = Bash::new(
            "",
            &format!(
                "set -o {mode}\nexport BUSINESS=exported\nprivate_value=private\nreply=untouched\nPROMPT_COMMAND='printf hook >> hooks'"
            ),
            "source ./integration.sh\nprintf '%s\\n' \"$BUSINESS|$private_value|$reply|$PWD\" > state\nset -o > options",
            true,
        );
        // Reload reinstalls the production command, then the test overrides only
        // that subprocess boundary, not Readline or the generated binding.
        bash.command("__nosh_bash_run() { printf 'call\\n' >> calls; printf 'printf ready'; }");
        let hooks = bash.read("hooks");
        bash.type_text("draft");
        if mode == "vi" {
            bash.terminal.send(b"\x1b");
        }
        bash.snapshot();
        let host_state = bash.read("host-snapshot");
        bash.suggest();
        bash.terminal.wait_for("printf ready");
        assert_eq!(bash.snapshot().0, "printf ready");
        assert_eq!(bash.read("host-snapshot"), host_state);
        assert_eq!(bash.read("hooks"), hooks);
        assert_eq!(bash.read("calls"), "call\n");
        assert!(
            bash.read("state")
                .starts_with("exported|private|untouched|")
        );
    }
}

#[test]
fn init_bash_rejects_noninteractive_and_basic_terminals() {
    let home = tempfile::tempdir().unwrap();
    let output = source_bash(
        std::env::var_os("NOSH_TEST_BASH").unwrap_or_else(|| "bash".into()),
        &init(home.path()),
    );
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interactive Readline terminal"));
    let mut bash = Bash::new("", "TERM=dumb", "", true);
    assert!(String::from_utf8_lossy(&bash.terminal.output).contains("usable TERM"));
    bash.type_text("draft");
    assert_eq!(bash.snapshot().0, "draft");
}

#[test]
fn init_bash_rejects_legacy_bash_without_installing_helpers() {
    let old = std::env::var_os("NOSH_TEST_OLD_BASH")
        .or_else(|| cfg!(target_os = "macos").then(|| "/bin/bash".into()));
    let Some(old) = old else { return };
    let home = tempfile::tempdir().unwrap();
    let mut script = init(home.path());
    script.extend_from_slice(b"\ndeclare -F __nosh_bash_suggest\n");
    let output = source_bash(old, &script);
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("requires Bash 5.1+"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("syntax error"));
}
