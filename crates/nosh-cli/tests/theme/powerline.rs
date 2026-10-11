use super::*;

const KITTY: &[(&str, &str)] = &[("TERM", "xterm-kitty"), ("KITTY_WINDOW_ID", "1")];
const GHOSTTY: &[(&str, &str)] = &[("TERM", "xterm-ghostty"), ("TERM_PROGRAM", "ghostty")];

fn preview(home: &Path, environment: &[(&str, &str)], force: bool) -> String {
    let mut command = command(home);
    command.envs(environment.iter().copied()).args([
        "theme",
        "preview",
        "soft-dark",
        "--case",
        "idle",
        "--columns",
        "80",
        "--mode",
        "off",
    ]);
    if force {
        command.arg("--nerd-font");
    }
    let mut shell = Shell::start(home, command);
    shell.wait(0, "not applicable");
    String::from_utf8_lossy(&shell.bytes).into_owned()
}

fn assert_caps(output: &str, expected: bool) {
    assert_eq!(output.contains('\u{e0b6}'), expected, "{output:?}");
    assert_eq!(output.contains('\u{e0b4}'), expected, "{output:?}");
    if !expected {
        assert!(!output.contains('\u{e0b0}'), "{output:?}");
    }
}

#[test]
fn omitted_and_explicit_auto_modes_follow_direct_renderer_capabilities() {
    let cases: &[(&[(&str, &str)], bool)] = &[
        (KITTY, true),
        (GHOSTTY, true),
        (&[], false),
        (&[("TERM", "xterm-kitty")], false),
        (&[("KITTY_WINDOW_ID", "1")], false),
        (&[("TERM", "xterm-ghostty")], false),
        (&[("TERM_PROGRAM", "vscode")], false),
        (&[("WT_SESSION", "test-host")], false),
    ];
    for configuration in [None, Some("[shell]\nnerd_font = 'auto'\n")] {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("config.toml");
        if let Some(configuration) = configuration {
            fs::write(&path, configuration).unwrap();
        }
        for (environment, expected) in cases {
            let output = preview(home.path(), environment, false);
            assert_caps(&output, *expected);
        }
        assert_eq!(fs::read_to_string(&path).ok().as_deref(), configuration);
    }
}

#[test]
fn auto_rejects_inherited_hints_and_explicit_modes_override_detection() {
    let home = tempfile::tempdir().unwrap();
    for name in [
        "TMUX",
        "STY",
        "ZELLIJ",
        "SSH_TTY",
        "SSH_CONNECTION",
        "SSH_CLIENT",
    ] {
        for direct in [KITTY, GHOSTTY] {
            let mut environment = direct.to_vec();
            environment.push((name, "inherited"));
            assert_caps(&preview(home.path(), &environment, false), false);
        }
    }
    let path = home.path().join("config.toml");
    for (setting, expected) in [("true", true), ("false", false)] {
        let saved = format!("[shell]\nnerd_font = {setting}\n");
        fs::write(&path, &saved).unwrap();
        for environment in [KITTY, &[("TERM_PROGRAM", "unknown"), ("TMUX", "nested")]] {
            assert_caps(&preview(home.path(), environment, false), expected);
        }
        assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    }
    assert_caps(&preview(home.path(), &[], true), true);
}

#[test]
fn automatic_and_forced_modes_respect_output_capability_fallbacks() {
    let home = tempfile::tempdir().unwrap();
    for limitation in [
        ("LC_ALL", "C"),
        ("NO_COLOR", "1"),
        ("CLICOLOR", "0"),
        ("COLORTERM", ""),
        ("TERM", "dumb"),
    ] {
        let mut environment = KITTY.to_vec();
        environment.push(limitation);
        for force in [false, true] {
            assert_caps(&preview(home.path(), &environment, force), false);
        }
    }
    let output = command(home.path())
        .envs(KITTY.iter().copied())
        .args(["theme", "preview", "soft-dark", "--case", "idle"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(!output.stdout.contains(&0x1b));
    assert_caps(&String::from_utf8_lossy(&output.stdout), false);
    assert!(!home.path().join("config.toml").exists());
}

#[test]
fn default_startup_and_prompt_refresh_apply_auto_without_interrupting_a_draft() {
    let home = tempfile::tempdir().unwrap();
    let mut command = command(home.path());
    command.envs(KITTY.iter().copied()).arg("--safe");
    let mut shell = Shell::start(home.path(), command);
    shell.wait(0, "\u{276f}");
    assert_caps(&String::from_utf8_lossy(&shell.bytes), true);
    shell.send(b"touch must_not_execute");
    shell.wait(0, "touch must_not_execute");
    let path = home.path().join("config.toml");
    fs::write(&path, "[shell]\nnerd_font = false\n").unwrap();
    let start = shell.bytes.len();
    for _ in 0..3 {
        shell.pump(50);
    }
    assert!(
        !String::from_utf8_lossy(&shell.bytes[start..]).contains("Ctrl+R history"),
        "changing the display setting interrupted the draft"
    );
    shell.send(b"\x03");
    shell.wait(start, "Ctrl+R history");
    let start = shell.bytes.len();
    shell.send(b"printf 'PLAIN_DONE\\n'\r");
    let start = shell.wait(start, "PLAIN_DONE\r\n");
    shell.wait(start, "\u{276f}");
    assert_caps(&String::from_utf8_lossy(&shell.bytes[start..]), false);
    assert!(!home.path().join("must_not_execute").exists());

    fs::write(&path, "[shell]\nnerd_font = 'auto'\n").unwrap();
    let start = shell.bytes.len();
    shell.send(b"printf 'AUTO_DONE\\n'\r");
    let start = shell.wait(start, "AUTO_DONE\r\n");
    shell.wait(start, "\u{276f}");
    assert_caps(&String::from_utf8_lossy(&shell.bytes[start..]), true);
}
