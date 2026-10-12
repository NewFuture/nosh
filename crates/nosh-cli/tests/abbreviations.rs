#![cfg(unix)]

#[path = "support/terminal.rs"]
mod terminal;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};
use std::time::{Duration, Instant};
use terminal::Terminal;

fn command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .env("NOSH_HOME", home)
        .env("HOME", home)
        .env("NOSH_DISABLE_AI", "1")
        .env("NOSH_LANG", "en")
        .env("PATH", "/usr/bin:/bin")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "dumb");
    command
}

fn abbr(home: &Path, args: &[&str]) -> Output {
    command(home).arg("abbr").args(args).output().unwrap()
}

fn success(result: Output) -> String {
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}

#[test]
fn abbreviation_management_is_persistent_and_works_without_ai() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    let listing = success(abbr(home.path(), &["list"]));
    assert!(listing.contains("parent-shell aliases/functions are not visible"));
    for name in [
        "ll", "la", "..", "...", "gst", "gd", "gds", "gb", "glo", "glog",
    ] {
        assert!(listing.contains(&format!("{name} ->")), "{listing}");
    }
    assert!(!path.exists(), "inspection must not create configuration");
    success(abbr(home.path(), &["set", "zzq", "echo 'hello world'"]));
    assert!(
        fs::read_to_string(&path)
            .unwrap()
            .contains("echo 'hello world'")
    );
    success(abbr(home.path(), &["disable", "--builtins"]));
    let listing = success(abbr(home.path(), &["list"]));
    assert!(listing.contains("zzq -> echo 'hello world' [user] available"));
    success(abbr(home.path(), &["disable", "zzq"]));
    assert!(success(abbr(home.path(), &["list"])).contains("[user] disabled"));
    success(abbr(home.path(), &["enable", "zzq"]));
    success(abbr(home.path(), &["set", "zzq", "echo updated"]));
    success(abbr(home.path(), &["remove", "zzq"]));
    assert!(!success(abbr(home.path(), &["list"])).contains("zzq ->"));
}

#[test]
fn abbreviation_overrides_and_invalid_updates_are_explicit() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    fs::write(
        &path,
        "# keep\n[shell]\ntheme='soft-dark'\nstatus_bar=false\n",
    )
    .unwrap();
    let original = fs::read(&path).unwrap();
    for args in [
        vec!["set", "gst", "git status -s"],
        vec!["set", "echo", "printf replacement"],
        vec!["set", "bad name", "echo invalid"],
        vec!["set", "zzq", "echo '"],
        vec!["set", "zzq", "echo x\necho y"],
        vec!["remove", "ll"],
        vec!["disable", "unknown-rule"],
    ] {
        assert!(!abbr(home.path(), &args).status.success(), "{args:?}");
        assert_eq!(fs::read(&path).unwrap(), original, "{args:?}");
    }
    success(abbr(
        home.path(),
        &["set", "gst", "git status -s", "--override"],
    ));
    let text = fs::read_to_string(&path).unwrap();
    assert!(text.contains("# keep"));
    assert!(text.contains("theme='soft-dark'"));
    success(
        command(home.path())
            .args(["theme", "set", "soft-light"])
            .output()
            .unwrap(),
    );
    assert!(fs::read_to_string(&path).unwrap().contains("git status -s"));
}

#[test]
fn abbreviation_definitions_are_not_evaluated_or_used_by_dash_c() {
    let home = tempfile::tempdir().unwrap();
    let marker = home.path().join("must-not-exist");
    let expansion = format!("echo $(touch '{}')", marker.display());
    success(abbr(home.path(), &["set", "zzq", &expansion]));
    assert!(!marker.exists());
    let result = command(home.path())
        .args(["--norc", "-c", "zzq"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!marker.exists());
}

#[test]
fn abbreviation_malformed_configuration_is_not_overwritten() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    fs::write(&path, "[shell.abbreviations]\nenabled='wrong'\n").unwrap();
    let original = fs::read(&path).unwrap();
    assert!(
        !abbr(home.path(), &["set", "zzq", "echo hello"])
            .status
            .success()
    );
    assert_eq!(fs::read(&path).unwrap(), original);
    assert!(!abbr(home.path(), &["list"]).status.success());
}

#[test]
fn setting_a_user_rule_does_not_probe_unrelated_tools() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    fs::write(&git, "#!/bin/sh\nexec /bin/sleep 10\n").unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
    let result = command(home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["abbr", "set", "zzq", "echo independent"])
        .output()
        .unwrap();
    success(result);
    assert!(
        fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("echo independent")
    );
}

#[test]
fn nonexecutable_path_entries_do_not_reserve_abbreviation_names() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let file = bin.join("zzq");
    fs::write(&file, "not a command").unwrap();
    fs::set_permissions(file, fs::Permissions::from_mode(0o600)).unwrap();
    let result = command(home.path())
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["abbr", "set", "zzq", "echo independent"])
        .output()
        .unwrap();
    success(result);
}

fn interactive_config(home: &Path, rules: &str) {
    fs::write(
        home.join("config.toml"),
        format!(
            "[shell]\ncapture_output = 'off'\nedit_mode = 'emacs'\ninput_assist = false\n\
         status_bar = false\ncommand_assist = false\ncompletion_scripts = false\n\
         trigger_on_error = false\non_failure = 'off'\n{rules}"
        ),
    )
    .unwrap();
}

fn launch(home: &Path, ai_enabled: bool) -> Terminal {
    let mut command = command(home);
    command
        .env("PS1", "abbr-real> ")
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "color.ui")
        .env("GIT_CONFIG_VALUE_0", "never")
        .env_remove("VISUAL")
        .env_remove("EDITOR")
        .current_dir(home)
        .args(["--offline", "--no-download", "--norc", "-i"]);
    if ai_enabled {
        command.env_remove("NOSH_DISABLE_AI");
    }
    let mut terminal = Terminal::new(command, 24, 120);
    terminal.wait_for("abbr-real> ");
    terminal
}

fn pump(terminal: &mut Terminal, duration: Duration) {
    let until = Instant::now() + duration;
    while Instant::now() < until {
        assert!(
            terminal.poll().is_none(),
            "shell exited: {:?}",
            String::from_utf8_lossy(&terminal.output)
        );
    }
}

fn wait_rule(terminal: &mut Terminal, name: &str, expected: &str) {
    for _ in 0..25 {
        let start = terminal.output.len();
        terminal.send(b"#abbr\r");
        terminal.wait_for(&format!("{name} ->"));
        terminal.wait_for("abbr-real> ");
        let output = String::from_utf8_lossy(&terminal.output[start..]);
        if output.contains(expected) {
            return;
        }
        pump(terminal, Duration::from_millis(80));
    }
    panic!(
        "rule {name:?} did not reach {expected:?}: {:?}",
        String::from_utf8_lossy(&terminal.output)
    );
}

fn finish(terminal: &mut Terminal) -> String {
    terminal.send(b"exit 0\r");
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(status) = terminal.poll() {
            assert!(
                status.success(),
                "{:?}",
                String::from_utf8_lossy(&terminal.output)
            );
            return nosh_shell::style::strip_ansi(&String::from_utf8_lossy(&terminal.output));
        }

        assert!(
            Instant::now() < deadline,
            "real shell failed to exit: {:?}",
            String::from_utf8_lossy(&terminal.output)
        );
    }
}

fn barrier(terminal: &mut Terminal) {
    terminal.send(b"printf 'REAL_BARRIER\\n'\r");
    terminal.wait_for("REAL_BARRIER\r\n");
    terminal.wait_for("abbr-real> ");
}

#[test]
fn actual_binary_expands_builtin_git_status_and_preserves_aliases() {
    let home = tempfile::tempdir().unwrap();
    interactive_config(home.path(), "");
    let init = Command::new("/usr/bin/git")
        .args(["init", "--quiet"])
        .arg(home.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", home.path())
        .output()
        .unwrap();
    assert!(init.status.success(), "{:?}", init.stderr);
    fs::write(home.path().join("review-real.txt"), "real test\n").unwrap();
    let mut terminal = launch(home.path(), true);
    wait_rule(
        &mut terminal,
        "gst",
        "gst -> git status [builtin] available",
    );
    terminal.send(b"gst ");
    terminal.wait_for("git status ");
    terminal.send(b"--short\r");
    terminal.wait_for("?? review-real.txt\r\n");
    terminal.wait_for("abbr-real> ");
    terminal.send(b"echo for;gst --short\r");
    terminal.wait_for("for\r\n");
    terminal.wait_for("?? review-real.txt\r\n");
    terminal.wait_for("abbr-real> ");
    terminal.send(b"alias gst='printf \"REAL_ALIAS_USED\\n\"'\r");
    terminal.wait_for("abbr-real> ");
    wait_rule(
        &mut terminal,
        "gst",
        "name conflicts with an existing alias",
    );
    terminal.send(b"gst \r");
    terminal.wait_for("REAL_ALIAS_USED\r\n");
    terminal.wait_for("abbr-real> ");
    let output = finish(&mut terminal);
    assert!(
        !output.contains("Downloading") && !output.contains("not installed"),
        "{output}"
    );
}

#[test]
fn actual_binary_undo_literal_paste_and_history_do_not_execute_expansions() {
    let home = tempfile::tempdir().unwrap();
    interactive_config(
        home.path(),
        "[shell.abbreviations]\nbuiltins = false\n\
         [shell.abbreviations.rules]\nzzq = { expansion = 'touch must-not-run' }\n",
    );
    let mut terminal = launch(home.path(), true);
    wait_rule(
        &mut terminal,
        "zzq",
        "zzq -> touch must-not-run [user] available",
    );
    terminal.send(b"zzq ");
    terminal.wait_for("touch must-not-run ");
    terminal.send(b"\x1a");
    terminal.wait_for("Input kept literal");
    terminal.send(b"\r");
    terminal.wait_for("no automatic AI");
    terminal.wait_for("abbr-real> ");
    assert!(!home.path().join("must-not-run").exists());
    terminal.send(b"zzq\0\r");
    terminal.wait_for("no automatic AI");
    terminal.wait_for("abbr-real> ");
    terminal.send(b"\x1b[200~zzq\x1b[201~ \r");
    terminal.wait_for("command not found: zzq");
    terminal.wait_for("abbr-real> ");
    terminal.send(b"\x1b[A\r");
    terminal.wait_for("command not found: zzq");
    terminal.wait_for("abbr-real> ");
    assert!(!home.path().join("must-not-run").exists());
    let output = finish(&mut terminal);
    assert!(
        !output.contains("Downloading") && !output.contains("not installed"),
        "{output}"
    );
}

#[test]
fn actual_binary_explicit_completion_and_rule_refresh_insert_the_displayed_value() {
    let home = tempfile::tempdir().unwrap();
    interactive_config(
        home.path(),
        "[shell.abbreviations]\nbuiltins = false\n\
         [shell.abbreviations.rules]\nzzq_unique = { expansion = 'touch old-target' }\n",
    );
    let mut terminal = launch(home.path(), true);
    wait_rule(
        &mut terminal,
        "zzq_unique",
        "zzq_unique -> touch old-target [user] available",
    );
    terminal.send(b"\x1b[200~zzq_unique\x1b[201~\t");
    terminal.wait_for("touch old-target ");
    assert!(
        !home.path().join("old-target").exists(),
        "completion must not execute"
    );
    terminal.send(b"\x15");
    terminal.wait_for("abbr-real> ");
    terminal.send(b"zzq_unique");
    terminal.wait_for("zzq_unique");
    interactive_config(
        home.path(),
        "[shell.abbreviations]\nbuiltins = false\n\
         [shell.abbreviations.rules]\nzzq_unique = { expansion = 'touch new-target' }\n",
    );
    terminal.send(b" ");
    terminal.wait_for("touch old-target ");
    terminal.send(b"\r");
    terminal.wait_for("abbr-real> ");
    barrier(&mut terminal);
    assert!(home.path().join("old-target").exists());
    assert!(!home.path().join("new-target").exists());
    wait_rule(
        &mut terminal,
        "zzq_unique",
        "zzq_unique -> touch new-target [user] available",
    );
    terminal.send(b"zzq_unique\r");
    terminal.wait_for("abbr-real> ");
    barrier(&mut terminal);
    assert!(home.path().join("new-target").exists());
    let output = finish(&mut terminal);
    assert!(
        !output.contains("Downloading") && !output.contains("not installed"),
        "{output}"
    );
}

#[test]
fn actual_binary_prefix_management_persists_add_disable_and_remove() {
    let home = tempfile::tempdir().unwrap();
    interactive_config(home.path(), "[shell.abbreviations]\nbuiltins = false\n");
    let mut terminal = launch(home.path(), true);
    terminal.send(b"#abbr set zzq 'printf \"REAL_CUSTOM\\n\"'\r");
    terminal.wait_for("abbreviations saved");
    terminal.wait_for("abbr-real> ");
    wait_rule(&mut terminal, "zzq", "[user] available");
    terminal.send(b"zzq\r");
    terminal.wait_for("REAL_CUSTOM\r\n");
    terminal.wait_for("abbr-real> ");
    terminal.send(b"#abbr disable zzq\r");
    terminal.wait_for("abbreviations saved");
    terminal.wait_for("abbr-real> ");
    wait_rule(&mut terminal, "zzq", "[user] disabled");
    terminal.send(b"zzq \r");
    terminal.wait_for("command not found: zzq");
    terminal.wait_for("abbr-real> ");
    terminal.send(b"#abbr remove zzq\r");
    terminal.wait_for("user rule removed");
    terminal.wait_for("abbr-real> ");
    assert!(
        !fs::read_to_string(home.path().join("config.toml"))
            .unwrap()
            .contains("REAL_CUSTOM")
    );
    let output = finish(&mut terminal);
    assert!(
        !output.contains("Downloading") && !output.contains("not installed"),
        "{output}"
    );
}

#[test]
fn actual_binary_ll_and_la_use_confirmed_host_ls_flags() {
    let home = tempfile::tempdir().unwrap();
    interactive_config(home.path(), "");
    let mut terminal = launch(home.path(), true);
    wait_rule(&mut terminal, "ll", "ll -> ls -lh [builtin] available");
    wait_rule(&mut terminal, "la", "la -> ls -lAh [builtin] available");
    for (name, expansion) in [("ll", "ls -lh "), ("la", "ls -lAh ")] {
        terminal.send(format!("{name} ").as_bytes());
        terminal.wait_for(expansion);
        terminal.send(b"-d .\r");
        terminal.wait_for(" .\r\n");
        terminal.wait_for("abbr-real> ");
        terminal.send(b"printf 'REAL_LS_EXIT:%s\\n' \"$?\"\r");
        terminal.wait_for("REAL_LS_EXIT:0\r\n");
        terminal.wait_for("abbr-real> ");
    }
    finish(&mut terminal);
}

#[test]
fn actual_binary_abbreviations_work_with_ai_disabled() {
    let home = tempfile::tempdir().unwrap();
    interactive_config(
        home.path(),
        "[shell.abbreviations]\nbuiltins = false\n\
         [shell.abbreviations.rules]\nzzq = { expansion = 'printf \"REAL_AI_OFF\\n\"' }\n",
    );
    let mut terminal = launch(home.path(), false);
    pump(&mut terminal, Duration::from_millis(750));
    terminal.send(b"zzq\r");
    terminal.wait_for("REAL_AI_OFF\r\n");
    terminal.wait_for("abbr-real> ");
    let output = finish(&mut terminal);
    assert!(
        !output.contains("Downloading") && !output.contains("not installed"),
        "{output}"
    );
}
