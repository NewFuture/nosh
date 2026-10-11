use super::*;

#[test]
fn starship_export_does_not_require_an_installed_engine() {
    let home = tempfile::tempdir().unwrap();
    let output = theme(home.path(), &["export", "soft-dark"]);
    assert!(output.status.success(), "{:?}", output.stderr);
    let source = String::from_utf8(output.stdout).unwrap();
    let table: toml::Table = source.parse().unwrap();
    assert_eq!(
        table["$schema"].as_str(),
        Some("https://starship.rs/config-schema.json")
    );
    assert!(!source.contains("foreground_index"));
    assert!(!home.path().join("config.toml").exists());
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn inline_native_theme_paths_follow_the_embedded_shell_directory() {
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    fs::write(&config, "[shell]\ncapture_output='off'\ninput_assist=false\ncompletion=true\ncommand_assist=false\n").unwrap();
    let project = home.path().join("project");
    fs::create_dir(&project).unwrap();
    let native = project.join("palette with spaces.toml");
    let exported = theme(home.path(), &["export", "soft-light"]);
    assert!(exported.status.success());
    fs::write(&native, exported.stdout).unwrap();

    let mut shell = Shell::inline(home.path());
    shell.wait(0, DARK);
    let start = shell.bytes.len();
    shell.send(b"cd project\r");
    shell.wait(start, DARK);
    let start = shell.bytes.len();
    shell.send(b"#theme ");
    shell.wait(start, "Light theme");
    let start = shell.bytes.len();
    shell.send(b"\x1b");
    shell.wait(start, "Tab");
    let start = shell.bytes.len();
    shell.send(b"'./palette with spaces.toml'\r");
    shell.wait(start, ".toml saved");
    shell.wait(start, LIGHT);
    let table: toml::Table = fs::read_to_string(&config).unwrap().parse().unwrap();
    let path = native.canonicalize().unwrap();
    assert_eq!(table["shell"]["theme"].as_str(), path.to_str());

    let start = shell.bytes.len();
    shell.send(b"#theme\r");
    shell.wait(start, &format!("theme: {}", path.display()));
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn native_powerline_respects_ascii_and_no_color_terminal_capabilities() {
    let home = tempfile::tempdir().unwrap();
    let exported = theme(home.path(), &["export", "soft-dark", "--nerd-font"]);
    assert!(exported.status.success());
    fs::write(home.path().join("starship.toml"), exported.stdout).unwrap();
    fs::write(
        home.path().join("config.toml"),
        "[shell]\ntheme='starship.toml'\nstarship=true\nnerd_font=true\ncapture_output='off'\ninput_assist=false\ncompletion=false\ncommand_assist=false\n",
    ).unwrap();
    for (name, value, prompt) in [
        ("LC_ALL", "C", "> "),
        ("NO_COLOR", "1", "\u{276f} "),
        ("COLORTERM", "", "\u{276f}"),
    ] {
        let mut command = command(home.path());
        command.env(name, value).arg("--safe");
        if name == "COLORTERM" {
            command.env("TERM", "xterm");
        }
        let mut shell = Shell::start(home.path(), command);
        shell.wait(0, prompt);
        let output = String::from_utf8_lossy(&shell.bytes);
        assert!(
            !output.contains(['\u{e0b6}', '\u{e0b4}', '\u{e0b0}']),
            "{output}"
        );
        assert!(
            !output.contains("Starship environment unavailable"),
            "{output}"
        );
    }
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn native_auto_mode_allows_powerline_without_assuming_full_nerd_font_support() {
    let home = tempfile::tempdir().unwrap();
    let exported = theme(home.path(), &["export", "soft-dark", "--nerd-font"]);
    assert!(exported.status.success());
    let mut table: toml::Table = String::from_utf8(exported.stdout).unwrap().parse().unwrap();
    table["directory"]["format"] =
        "([\u{e0b6}](fg:nosh_environment_bg)[ \u{f07b} $path ]($style))".into();
    fs::write(
        home.path().join("starship.toml"),
        toml::to_string(&table).unwrap(),
    )
    .unwrap();
    for setting in ["'auto'", "true", "false"] {
        fs::write(
            home.path().join("config.toml"),
            format!("[shell]\ntheme='starship.toml'\nstarship=true\nnerd_font={setting}\ncapture_output='off'\ninput_assist=false\ncompletion=false\ncommand_assist=false\n"),
        ).unwrap();
        let mut command = command(home.path());
        command
            .env("TERM", "xterm-kitty")
            .env("KITTY_WINDOW_ID", "1")
            .arg("--safe");
        let mut shell = Shell::start(home.path(), command);
        shell.wait(0, "\u{276f}");
        let output = String::from_utf8_lossy(&shell.bytes);
        assert_eq!(output.contains('\u{e0b6}'), setting != "false", "{output}");
        assert_eq!(output.contains('\u{e0b4}'), setting != "false", "{output}");
        assert_eq!(output.contains('\u{f07b}'), setting == "true", "{output}");
        assert!(
            !output.contains("Starship environment unavailable"),
            "{output}"
        );
    }
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn native_theme_files_are_saved_and_engine_calls_stop_while_editing() {
    use std::os::unix::fs::PermissionsExt;
    let home = tempfile::tempdir().unwrap();
    let config = home.path().join("config.toml");
    fs::write(&config, "[shell]\ncapture_output='off'\ninput_assist=false\ncompletion=false\ncommand_assist=false\nstarship=true\n").unwrap();
    let exported = theme(home.path(), &["export", "soft-dark"]);
    assert!(exported.status.success());
    let mut table: toml::Table = String::from_utf8(exported.stdout).unwrap().parse().unwrap();
    table.insert("format".into(), "ENV_SENTINEL".into());
    let native = home.path().join("starship.toml");
    fs::write(&native, toml::to_string(&table).unwrap()).unwrap();
    let selected = theme(home.path(), &["set", native.to_str().unwrap()]);
    assert!(
        selected.status.success(),
        "{}",
        String::from_utf8_lossy(&selected.stderr)
    );

    let original_path = std::env::var_os("PATH").unwrap();
    let real = std::env::split_paths(&original_path)
        .map(|p| p.join("starship"))
        .find(|p| p.is_file())
        .expect("official Starship on PATH");
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let counter = home.path().join("starship-calls");
    let quote = |path: &Path| path.to_string_lossy().replace('\'', "'\\''");
    let wrapper = bin.join("starship");
    fs::write(
        &wrapper,
        format!(
            "#!/bin/sh\nprintf 'call\\n' >> '{}'\nexec '{}' \"$@\"\n",
            quote(&counter),
            quote(&real)
        ),
    )
    .unwrap();
    fs::set_permissions(&wrapper, fs::Permissions::from_mode(0o755)).unwrap();
    let path =
        std::env::join_paths(std::iter::once(bin).chain(std::env::split_paths(&original_path)))
            .unwrap();
    let mut shell = Shell::with_path(home.path(), Some(&path));
    shell.wait(0, "ENV_SENTINEL");
    let calls = || fs::read_to_string(&counter).unwrap().lines().count();
    let initial = calls();
    assert_eq!(
        initial, 2,
        "startup should parse native styles once and render the environment once"
    );
    shell.send(b"echo draft_only");
    shell.wait(0, "echo draft_only");
    for _ in 0..4 {
        shell.pump(50);
    }
    assert_eq!(calls(), initial, "editing re-ran Starship");
    let start = shell.bytes.len();
    shell.send(b"\x03");
    shell.wait(start, "ENV_SENTINEL");
    assert_eq!(calls(), initial + 1, "unchanged config recompiled styles");

    let initial = calls();
    let saved = fs::read_to_string(&config).unwrap();
    fs::write(
        &config,
        format!("{saved}\n# unrelated configuration edit\n"),
    )
    .unwrap();
    let start = shell.bytes.len();
    shell.send(b"\x03");
    shell.wait(start, "ENV_SENTINEL");
    assert_eq!(
        calls(),
        initial + 1,
        "unrelated config edit recompiled styles"
    );

    let initial = calls();
    let exported = theme(home.path(), &["export", "soft-light"]);
    assert!(exported.status.success());
    let mut table: toml::Table = String::from_utf8(exported.stdout).unwrap().parse().unwrap();
    table.insert("format".into(), "ENV_SENTINEL".into());
    fs::write(&native, toml::to_string(&table).unwrap()).unwrap();
    let start = shell.bytes.len();
    shell.send(b"\x03");
    shell.wait(start, "\x1b[38;2;76;79;105m");
    shell.wait(start, "ENV_SENTINEL");
    assert_eq!(
        calls(),
        initial + 2,
        "changed native theme was not recompiled"
    );
}
