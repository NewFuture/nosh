use super::*;

fn assert_connected_powerline(fragment: &Fragment) {
    assert_eq!(
        fragment.text().matches('\u{e0b6}').count(),
        1,
        "{}",
        fragment.text()
    );
    assert_eq!(
        fragment.text().matches('\u{e0b4}').count(),
        1,
        "{}",
        fragment.text()
    );
    let mut previous = None;
    for (text, paint) in fragment.runs() {
        for character in text.chars() {
            match character {
                '\u{e0b6}' => assert!(paint.background.is_none()),
                '\u{e0b4}' => {
                    assert!(paint.background.is_none());
                    assert_eq!(paint.foreground, previous);
                }
                '\u{e0b0}' => {
                    assert_eq!(paint.foreground, previous);
                    assert!(paint.background.is_some());
                    previous = paint.background;
                }
                _ => {
                    assert!(
                        paint.background.is_some(),
                        "unstyled gap in {:?}",
                        fragment.text()
                    );
                    previous = paint.background;
                }
            }
        }
    }
}

#[test]
fn automatic_powerline_does_not_claim_arbitrary_nerd_font_glyphs() {
    for glyphs in [Glyphs::Plain, Glyphs::Powerline, Glyphs::NerdFont] {
        for character in ['a', ' ', '\u{4e2d}', '\u{1f4bb}'] {
            assert!(glyphs.allows(character));
        }
        for character in ['\u{e0b0}', '\u{e0b2}', '\u{e0b4}', '\u{e0b6}', '\u{e0b7}'] {
            assert_eq!(glyphs.allows(character), glyphs != Glyphs::Plain);
        }
        for character in [
            '\u{e000}',
            '\u{e0af}',
            '\u{e0b8}',
            '\u{f07b}',
            '\u{f8ff}',
            '\u{f0000}',
            '\u{100000}',
            '\u{10fffd}',
        ] {
            assert_eq!(glyphs.allows(character), glyphs == Glyphs::NerdFont);
        }
    }
}

#[test]
fn glyph_filter_covers_symbol_maps_without_rewriting_detection_paths() {
    let original: toml::Table = r#"
format = "\ue0b6$directory\ue0b4"
[os.symbols]
Linux = "\uf17c"
[directory]
symbol = "\uf07b"
detect_files = ["\uf07b"]
[directory.substitutions]
"\uf07b" = "\uf07b work"
[status]
success_symbol = "\uf00c"
symbol_failure = "\uf00d"
[[battery.display]]
threshold = 30
charging_symbol = "\uf00c"
discharging_symbol = "\uf00d"
[[kubernetes.contexts]]
context_pattern = "\uf0c2-cluster"
symbol = "\uf0c2"
"#
    .parse()
    .unwrap();
    let original = toml::Value::Table(original);
    let mut filtered = original.clone();
    filter_glyphs(&mut filtered, Glyphs::Powerline, false);
    assert_eq!(filtered["format"], original["format"]);
    assert_eq!(filtered["os"]["symbols"]["Linux"].as_str(), Some(""));
    assert_eq!(filtered["directory"]["symbol"].as_str(), Some(""));
    assert_eq!(
        filtered["directory"]["substitutions"]["\u{f07b}"].as_str(),
        Some(" work")
    );
    assert_eq!(
        filtered["directory"]["detect_files"],
        original["directory"]["detect_files"]
    );
    assert_eq!(filtered["status"]["success_symbol"].as_str(), Some(""));
    assert_eq!(filtered["status"]["symbol_failure"].as_str(), Some(""));
    assert_eq!(
        filtered["battery"]["display"][0]["charging_symbol"].as_str(),
        Some("")
    );
    assert_eq!(
        filtered["battery"]["display"][0]["discharging_symbol"].as_str(),
        Some("")
    );
    assert_eq!(
        filtered["kubernetes"]["contexts"][0]["symbol"].as_str(),
        Some("")
    );
    assert_eq!(
        filtered["kubernetes"]["contexts"][0]["context_pattern"],
        original["kubernetes"]["contexts"][0]["context_pattern"]
    );
    let mut filtered = original.clone();
    filter_glyphs(&mut filtered, Glyphs::NerdFont, false);
    assert_eq!(filtered, original);
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn native_environment_preserves_authored_outer_padding() {
    let directory = tempfile::tempdir().unwrap();
    let shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(directory.path().into()),
        ..Default::default()
    })
    .unwrap();
    for format in ["  native  ", "  [native](fg:blue)  "] {
        let source =
            toml::to_string(&toml::Table::from_iter([("format".into(), format.into())])).unwrap();
        for glyphs in [Glyphs::Plain, Glyphs::Powerline, Glyphs::NerdFont] {
            assert_eq!(
                environment(&source, &shell, glyphs).unwrap().text(),
                "  native  ",
                "{glyphs:?}/{format}"
            );
        }
    }
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn native_powerline_only_policy_preserves_caps_without_enabling_other_icons() {
    let directory = tempfile::tempdir().unwrap();
    let shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(directory.path().into()),
        ..Default::default()
    })
    .unwrap();
    let mut table: toml::Table = export(Builtin::Dark, true).parse().unwrap();
    table["directory"]["format"] =
        "([\u{e0b6}](fg:nosh_environment_bg)[ \u{f07b} $path ]($style))".into();
    let source = toml::to_string(&table).unwrap();
    for glyphs in [Glyphs::Plain, Glyphs::Powerline, Glyphs::NerdFont] {
        let fragment = environment(&source, &shell, glyphs).unwrap();
        assert_eq!(
            fragment.text().contains('\u{f07b}'),
            glyphs == Glyphs::NerdFont
        );
        if glyphs.powerline() {
            assert_connected_powerline(&fragment);
        } else {
            assert!(
                !fragment
                    .text()
                    .contains(['\u{e0b0}', '\u{e0b4}', '\u{e0b6}'])
            );
        }
        assert!(
            fragment
                .text()
                .contains(directory.path().file_name().unwrap().to_str().unwrap())
        );
    }
}

#[test]
fn native_exports_have_no_commands_or_proprietary_color_indices() {
    for builtin in Builtin::ALL {
        let source = export(builtin, false);
        let table: toml::Table = source.parse().unwrap();
        assert_eq!(
            table["$schema"].as_str(),
            Some("https://starship.rs/config-schema.json")
        );
        assert!(!source.contains("foreground_index") && !source.contains("background_index"));
        for module in [
            "git_status",
            "rust",
            "nodejs",
            "golang",
            "python",
            "conda",
            "status",
            "cmd_duration",
        ] {
            assert!(
                table["format"]
                    .as_str()
                    .unwrap()
                    .contains(&format!("${module}"))
            );
        }
        assert_eq!(table["status"]["disabled"].as_bool(), Some(false));
        assert_eq!(table["cmd_duration"]["min_time"].as_integer(), Some(2000));
        validate_custom(&table).unwrap();
    }
}

#[test]
fn importing_a_theme_never_executes_custom_scripts() {
    for value in [
        "[custom.danger]\ncommand = 'touch should_not_exist'\nwhen = true",
        "[custom.danger]\nwhen = 'touch should_not_exist'",
        "[custom.nosh_typo]\nstyle = 'blue'",
    ] {
        assert!(compile(value).is_err());
    }
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn official_starship_resolves_native_export_styles() {
    for builtin in Builtin::ALL {
        for nerd_font in [false, true] {
            let actual = compile(&export(builtin, nerd_font)).unwrap();
            for (role, pair) in builtin.palette().pairs() {
                let actual = actual.pairs().find(|(name, _)| *name == role).unwrap().1;
                assert_eq!(actual.foreground_rgb(), pair.foreground_rgb(), "{role}");
                assert_eq!(actual.background_rgb(), pair.background_rgb(), "{role}");
            }
        }
    }
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn native_directory_and_git_styles_map_to_separate_segments() {
    let theme =
        compile("[directory]\nstyle='fg:#11111B bg:#FAB387'\n[git_branch]\nstyle='bg:#F9E2AF'\n")
            .unwrap();
    assert_eq!(theme.environment.background_rgb(), [250, 179, 135]);
    assert_eq!(theme.git_branch.background_rgb(), [249, 226, 175]);
    assert_eq!(theme.git_branch.foreground_rgb(), [17, 17, 27]);
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn native_style_errors_are_not_silently_replaced() {
    assert!(compile("[custom.nosh_operation]\nstyle = 'not_a_color'").is_err());
    assert!(compile("[custom.nosh_operation]\nstyle = 'hidden'").is_err());
    assert!(
        compile("[custom.nosh_operation]\nstyle = 'fg:#11111B bg:#FAB387'")
            .unwrap_err()
            .contains("host reference background")
    );
    assert_eq!(
        compile("[custom.nosh_operation]\nstyle = 'fg:#FFFFFF bg:#FFFFFF'").unwrap_err(),
        "operation: RGB text contrast must be at least 4.5:1"
    );
}

#[test]
#[ignore = "requires official Starship on PATH"]
fn official_modules_use_real_directory_git_and_rust_context() {
    let directory = tempfile::tempdir().unwrap();
    let status = Command::new("git")
        .args(["init", "--quiet", "--initial-branch=theme-fixture"])
        .arg(directory.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::write(
        directory.path().join("Cargo.toml"),
        "[package]\nname='fixture'\nversion='0.1.0'\n",
    )
    .unwrap();
    let shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(directory.path().into()),
        ..Default::default()
    })
    .unwrap();
    let fragment = environment(&export(Builtin::Dark, false), &shell, Glyphs::Plain).unwrap();
    assert!(
        fragment.text().contains("theme-fixture"),
        "{}",
        fragment.text()
    );
    assert!(
        fragment
            .text()
            .contains(directory.path().file_name().unwrap().to_str().unwrap())
    );
    let version = Command::new("rustc").arg("--version").output().unwrap();
    let version = String::from_utf8(version.stdout).unwrap();
    assert!(
        fragment
            .text()
            .contains(version.split_whitespace().nth(1).unwrap())
    );
    let mut table: toml::Table = export(Builtin::Dark, false).parse().unwrap();
    table.insert(
        "format".into(),
        "[\u{e0b6}](fg:nosh_environment_bg)$directory[\u{e0b0}](fg:nosh_environment_bg bg:nosh_git_branch_bg)$git_branch[\u{e0b4}](fg:nosh_git_branch_bg)".into(),
    );
    let source = toml::to_string(&table).unwrap();
    let native = environment(&source, &shell, Glyphs::NerdFont).unwrap();
    assert!(native.text().starts_with('\u{e0b6}') && native.text().ends_with('\u{e0b4}'));
    assert_eq!(native.text().matches('\u{e0b0}').count(), 1);
    let plain = environment(&source, &shell, Glyphs::Plain).unwrap();
    assert!(!plain.text().contains(['\u{e0b6}', '\u{e0b0}', '\u{e0b4}']));
    assert!(plain.text().contains("theme-fixture"));
}

#[test]
#[cfg(unix)]
#[ignore = "requires official Starship on PATH"]
fn official_modules_render_git_changes_runtime_environment_and_command_metrics() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(directory.path())
            .status()
            .unwrap()
            .success()
    );
    let bin = directory.path().join("bin");
    std::fs::create_dir(&bin).unwrap();
    for (name, version) in [
        ("node", "v22.9.0"),
        ("go", "go version go1.25.0 linux/amd64"),
        ("python", "Python 3.12.4"),
    ] {
        let file = bin.join(name);
        std::fs::write(&file, format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n")).unwrap();
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    for name in ["package.json", "go.mod", "app.py"] {
        std::fs::write(directory.path().join(name), "").unwrap();
    }
    let mut shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(directory.path().into()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line(&format!(
        "export PATH='{}':\"$PATH\" VIRTUAL_ENV='{}/dev-env' CONDA_DEFAULT_ENV='data-env'",
        bin.display(),
        directory.path().display()
    ));
    let fragment = environment(&export(Builtin::Dark, true), &shell, Glyphs::NerdFont).unwrap();
    assert_connected_powerline(&fragment);
    for value in ["22.9.0", "1.25.0", "3.12.4", "dev-env", "data-env", "?"] {
        assert!(
            fragment.text().contains(value),
            "missing {value}: {}",
            fragment.text()
        );
    }
    let empty = tempfile::tempdir().unwrap();
    let mut empty_shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(empty.path().into()),
        ..Default::default()
    })
    .unwrap();
    empty_shell.run_user_line("unset VIRTUAL_ENV CONDA_DEFAULT_ENV");
    for nerd_font in [false, true] {
        let glyphs = if nerd_font {
            Glyphs::NerdFont
        } else {
            Glyphs::Plain
        };
        let fragment =
            environment(&export(Builtin::Dark, nerd_font), &empty_shell, glyphs).unwrap();
        assert!(!fragment.text().contains("exit 0"), "{}", fragment.text());
        if nerd_font {
            assert_connected_powerline(&fragment);
            assert_eq!(
                fragment.text().matches('\u{e0b0}').count(),
                0,
                "{}",
                fragment.text()
            );
        }
    }
    assert!(
        Command::new("git")
            .args(["init", "--quiet"])
            .arg(empty.path())
            .status()
            .unwrap()
            .success()
    );
    let clean = environment(&export(Builtin::Dark, true), &empty_shell, Glyphs::NerdFont).unwrap();
    assert_connected_powerline(&clean);
    assert_eq!(
        clean.text().matches('\u{e0b0}').count(),
        1,
        "clean repository left an empty Git status segment: {}",
        clean.text()
    );
    let previous = nosh_shell::UserCommand {
        id: 1,
        line: "fixture".into(),
        cwd: directory.path().into(),
        exit: 7,
        duration: Duration::from_millis(2400),
    };
    let table = export(Builtin::Dark, false).parse().unwrap();
    let variables: Vec<_> = shell
        .scalar_vars()
        .into_iter()
        .filter(|(_, _, exported)| *exported)
        .map(|(name, value, _)| (name, value))
        .collect();
    let output = run(
        &table,
        directory.path(),
        Some(&variables),
        Some(&previous),
        &|| false,
    )
    .unwrap();
    let text = Fragment::parse(&output);
    for version in ["22.9.0", "1.25.0", "3.12.4"] {
        assert!(text.text().contains(version), "{}", text.text());
    }
    assert!(
        text.text().contains("exit 7") && text.text().contains("2s"),
        "{}",
        text.text()
    );
}

#[test]
#[cfg(unix)]
fn native_runs_are_bounded() {
    let start = Instant::now();
    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 10"]);
    let error = collect(&mut command, &|| false, Duration::from_millis(50)).unwrap_err();
    assert!(error.contains("budget"), "{error}");
    assert!(start.elapsed() < Duration::from_secs(2));

    let mut command = Command::new("/bin/sh");
    command.args(["-c", "yes x"]);
    assert!(
        collect(&mut command, &|| false, TIMEOUT)
            .unwrap_err()
            .contains("64 KiB")
    );

    let mut command = Command::new("/bin/sh");
    command.args(["-c", "sleep 10"]);
    assert!(
        collect(&mut command, &|| true, TIMEOUT)
            .unwrap_err()
            .contains("cancelled")
    );
}
