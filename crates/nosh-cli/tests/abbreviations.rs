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

fn stubbed_probe_path(home: &Path) -> String {
    let bin = home.join("bin");
    fs::create_dir(&bin).unwrap();
    fs::set_permissions(&bin, fs::Permissions::from_mode(0o777)).unwrap();
    for (name, version) in [
        ("git", "git version 2.40.0"),
        ("ls", "ls (GNU coreutils) 9.0"),
    ] {
        let program = bin.join(name);
        fs::write(
            &program,
            format!("#!/bin/sh\nprintf x >> {name}-probed\nprintf '{version}\\n'\n"),
        )
        .unwrap();
        fs::set_permissions(program, fs::Permissions::from_mode(0o700)).unwrap();
    }
    format!("{}:/usr/bin:/bin", bin.display())
}

#[test]
fn abbreviation_management_is_persistent_and_works_without_ai() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    let listing = success(
        command(home.path())
            .current_dir(home.path())
            .env("PATH", stubbed_probe_path(home.path()))
            .args(["abbr", "list"])
            .output()
            .unwrap(),
    );
    assert!(listing.contains("parent-shell aliases/functions are not visible"));
    assert!(
        listing.contains("gst -> git status [builtin] available"),
        "{listing}"
    );
    assert!(
        listing.contains("ll -> ls -lh [builtin] available"),
        "{listing}"
    );
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
        "# keep\n[shell]\ntheme='soft-dark'\nstatus_bar=false\n\
         [shell.abbreviations]\nprobe_commands = false\n",
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
    assert!(text.contains("probe_commands = false"));
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
fn malformed_shell_text_in_toml_is_rejected_without_rewriting_the_file() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    let source = "[shell.abbreviations.rules]\nzzq = { expansion = \"echo '\" }\n";
    fs::write(&path, source).unwrap();
    for arguments in [vec!["list"], vec!["set", "another", "echo valid"]] {
        let result = abbr(home.path(), &arguments);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("invalid expansion"));
        assert_eq!(fs::read_to_string(&path).unwrap(), source);
    }
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
fn first_management_inspection_allows_bounded_cold_tool_startup() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let git = bin.join("git");
    fs::write(
        &git,
        "#!/bin/sh\n/bin/sleep 2.5\nprintf 'git version 2.40.0\\n'\n",
    )
    .unwrap();
    fs::set_permissions(&git, fs::Permissions::from_mode(0o700)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    let listing = success(
        command(home.path())
            .env("PATH", &path)
            .args(["abbr", "list"])
            .output()
            .unwrap(),
    );
    assert!(
        listing.contains("gst -> git status [builtin] available"),
        "{listing}"
    );
    interactive_config(
        home.path(),
        "[shell.abbreviations]\nprobe_commands = true\n",
    );
    let mut terminal = launch_with_path(home.path(), true, &path);
    wait_rule(
        &mut terminal,
        "gst",
        "gst -> git status [builtin] available",
    );
    finish(&mut terminal);
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
    launch_with_path(home, ai_enabled, "/usr/bin:/bin")
}

fn launch_with_path(home: &Path, ai_enabled: bool, path: &str) -> Terminal {
    let mut command = command(home);
    command
        .env("PATH", path)
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
    // Match the management inspection bound, including the first cold probe.
    let deadline = Instant::now() + Duration::from_millis(4500);
    while Instant::now() < deadline {
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
fn default_probes_run_and_explicit_disable_blocks_inspection_and_startup() {
    let home = tempfile::tempdir().unwrap();
    let path = stubbed_probe_path(home.path());
    let inspect = || {
        success(
            command(home.path())
                .current_dir(home.path())
                .env("PATH", &path)
                .args(["abbr", "list"])
                .output()
                .unwrap(),
        )
    };
    let listing = inspect();
    for definition in [
        "gst -> git status [builtin] available",
        "ll -> ls -lh [builtin] available",
    ] {
        assert!(listing.contains(definition), "{listing}");
    }
    assert!(!home.path().join("config.toml").exists());
    for name in ["git", "ls"] {
        let marker = home.path().join(format!("{name}-probed"));
        assert_eq!(fs::read_to_string(&marker).unwrap(), "x");
        fs::remove_file(marker).unwrap();
    }
    fs::write(
        home.path().join("config.toml"),
        "[shell.abbreviations]\nprobe_commands = false\n",
    )
    .unwrap();
    let listing = inspect();
    assert!(
        listing.contains("external capability probes disabled"),
        "{listing}"
    );
    for name in ["git", "ls"] {
        assert!(!home.path().join(format!("{name}-probed")).exists());
    }
    interactive_config(
        home.path(),
        "[shell.abbreviations]\nprobe_commands = false\n",
    );
    let mut terminal = launch_with_path(home.path(), true, &path);
    wait_rule(&mut terminal, "gst", "external capability probes disabled");
    wait_rule(&mut terminal, "..", ".. -> cd .. [builtin] available");
    let start = terminal.output.len();
    terminal.send(b"gst ");
    pump(&mut terminal, Duration::from_millis(100));
    assert!(
        !nosh_shell::style::strip_ansi(&String::from_utf8_lossy(&terminal.output[start..]))
            .contains("git status ")
    );
    terminal.send(b"\x15");
    finish(&mut terminal);
    for name in ["git", "ls"] {
        assert!(!home.path().join(format!("{name}-probed")).exists());
    }
    interactive_config(home.path(), "");
    let mut terminal = launch_with_path(home.path(), true, &path);
    wait_rule(
        &mut terminal,
        "gst",
        "gst -> git status [builtin] available",
    );
    wait_rule(&mut terminal, "ll", "ll -> ls -lh [builtin] available");
    for name in ["git", "ls"] {
        assert!(home.path().join(format!("{name}-probed")).exists());
    }
    finish(&mut terminal);
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
    terminal.send(b"\x1b[A");
    terminal.wait_for("History [all]");
    terminal.send(b"\r");
    terminal.wait_for("abbr-real> ");
    assert!(!home.path().join("must-not-run").exists());
    terminal.send(b"\r");
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

#[test]
fn actual_binary_pending_rule_preserves_an_unhashed_path_executable() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for (name, source) in [
        ("git", "#!/bin/sh\n: > probe-started\nexec /bin/sleep 60\n"),
        ("zzq", "#!/bin/sh\nprintf 'REAL_UNHASHED_EXECUTABLE\\n'\n"),
    ] {
        let path = bin.join(name);
        fs::write(&path, source).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    interactive_config(
        home.path(),
        "[shell.abbreviations]\nprobe_commands = true\n\
         [shell.abbreviations.rules]\nzzq = { expansion = 'touch must-not-run' }\n",
    );
    let mut launch = command(home.path());
    launch
        .current_dir(home.path())
        .env("PS1", "abbr-real> ")
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .args(["--safe", "-i"]);
    let mut terminal = Terminal::new(launch, 24, 120);
    terminal.wait_for("abbr-real> ");
    let deadline = Instant::now() + Duration::from_secs(5);
    while !home.path().join("probe-started").exists() {
        assert!(Instant::now() < deadline);
        assert!(terminal.poll().is_none());
    }
    terminal.send(b"zzq\r");
    terminal.wait_for("REAL_UNHASHED_EXECUTABLE\r\n");
    terminal.wait_for("abbr-real> ");
    assert!(!home.path().join("must-not-run").exists());
    finish(&mut terminal);
}
