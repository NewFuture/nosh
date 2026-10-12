use super::*;
use crate::{EmbeddedShell, ShellOptions, TriggerConfig};
use std::os::unix::fs::PermissionsExt;

#[test]
fn abbreviation_names_limits_and_explicit_overrides() {
    let mut config = Config::default();
    assert_eq!(BUILTINS.len(), 10);
    assert!(config.validate("#").is_ok());
    for name in ["", "a b", "if", "#help", "x=y", "a/b", "'gst'"] {
        assert!(validate_name(name, "#").is_err(), "{name:?}");
    }
    assert!(validate_name("目录", "#").is_ok());
    config.rules.insert(
        "gst".into(),
        Rule {
            expansion: "git status -s".into(),
            override_existing: false,
        },
    );
    assert!(config.validate("#").is_err());
    config.rules.get_mut("gst").unwrap().override_existing = true;
    assert!(config.validate("#").is_ok());
    for expansion in ["", "# comment", "echo '", "echo x\npwd", "echo \0"] {
        assert!(validate_expansion(expansion).is_err(), "{expansion:?}");
    }
    assert!(validate_expansion("printf '%s' \"hello world\" | cat").is_ok());
}

#[test]
fn file_and_command_configuration_share_shell_syntax_validation() {
    for expansion in ["echo '", "# only a comment", "echo value |"] {
        let mut config = Config::default();
        config.rules.insert(
            "custom".into(),
            Rule {
                expansion: expansion.into(),
                override_existing: false,
            },
        );
        assert!(config.validate("#").is_err(), "{expansion}");
        assert!(
            Operation::Set {
                name: "custom".into(),
                expansion: expansion.into(),
                override_existing: false,
            }
            .apply(&mut Config::default(), "#")
            .is_err()
        );
    }
}

#[test]
fn abbreviation_management_does_not_silently_replace_builtins() {
    let mut config = Config::default();
    let operation =
        Operation::parse(&["set".into(), "gst".into(), "git status -s".into()]).unwrap();
    assert!(operation.apply(&mut config, "#").is_err());
    let operation = Operation::parse(&[
        "set".into(),
        "gst".into(),
        "git status -s".into(),
        "--override".into(),
    ])
    .unwrap();
    operation.apply(&mut config, "#").unwrap();
    assert!(config.rules["gst"].override_existing);
    Operation::Toggle {
        target: Target::Builtins,
        enabled: false,
    }
    .apply(&mut config, "#")
    .unwrap();
    assert!(!config.builtins);
    assert!(config.rules.contains_key("gst"));
    assert!(
        Operation::Remove("gst".into())
            .apply(&mut config, "#")
            .unwrap()
            .contains("built-in")
    );
    assert!(
        Operation::Remove("ll".into())
            .apply(&mut config, "#")
            .is_err()
    );
}

#[test]
fn abbreviation_positions_preserve_quotes_arguments_comments_and_scope() {
    let words = |text: &str| {
        command_ranges(text, Some("#"))
            .unwrap()
            .into_iter()
            .map(|range| text[range].to_owned())
            .collect::<Vec<_>>()
    };
    assert_eq!(words("gst;gd | gds && gb"), ["gst", "gd", "gds", "gb"]);
    assert_eq!(words("echo for; gst"), ["echo", "gst"]);
    assert_eq!(words("< for gst"), ["gst"]);
    assert_eq!(words("echo case function coproc; gd"), ["echo", "gd"]);
    assert_eq!(words("A=1 2>out gst 中文\n  gd 'x y'"), ["gst", "gd"]);
    assert_eq!(words("echo gst 'gd' \"gb\" # glog\n gst"), ["echo", "gst"]);
    assert!(words("# explain gst").is_empty());
    assert!(words("'gst'").is_empty());
    assert!(words("\\gst").is_empty());
    assert!(words("PATH=/other gst").is_empty());
    assert_eq!(words("source file; gst"), ["source"]);
    assert_eq!(words("cat <<EOF\ngst\nEOF"), ["cat"]);
    assert!(words("gst() { :; }; gst").is_empty());
    assert!(command_ranges(&"x".repeat(MAX_BYTES + 1), None).is_err());
}

pub(super) fn fixture() -> (tempfile::TempDir, EmbeddedShell) {
    let directory = tempfile::tempdir().unwrap();
    for (name, text) in [
        ("git", "#!/bin/sh\nprintf 'git version 2.40.0\\n'\n"),
        ("ls", "#!/bin/sh\nprintf 'ls (GNU coreutils) 9.0\\n'\n"),
    ] {
        let path = directory.path().join(name);
        std::fs::write(&path, text).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(directory.path().to_owned()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line(&format!("PATH='{}'", directory.path().display()));
    shell.run_user_line("shopt -s expand_aliases");
    (directory, shell)
}

pub(super) fn probe_config() -> Config {
    Config {
        probe_commands: true,
        ..Default::default()
    }
}

#[test]
fn external_capability_probes_require_explicit_opt_in_even_with_cached_results() {
    let (directory, mut shell) = fixture();
    std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
    for (name, version) in [
        ("git", "git version 2.40.0"),
        ("ls", "ls (GNU coreutils) 9.0"),
    ] {
        std::fs::write(
            directory.path().join(name),
            format!("#!/bin/sh\nprintf x >> {name}-probed\nprintf '{version}\\n'\n"),
        )
        .unwrap();
    }
    shell.run_user_line("PATH=.");
    let mut config = Config::default();
    config.rules.insert(
        "zzq".into(),
        Rule {
            expansion: "echo custom".into(),
            override_existing: false,
        },
    );
    let mut probe = Probe {
        serial: 1,
        config,
        context: shell
            .input_context(&TriggerConfig::default(), &Default::default())
            .unwrap(),
    };
    let mut checker = Checker::default();
    let registry = checker.check(&probe);
    let applicable = BTreeSet::from(["..".into(), "...".into(), "zzq".into()]);
    assert_eq!(registry.approved(1).applicable, applicable);
    assert!(
        registry
            .display()
            .contains("shell.abbreviations.probe_commands")
    );
    for name in ["git", "ls"] {
        assert!(!directory.path().join(format!("{name}-probed")).exists());
    }
    probe.config.probe_commands = true;
    let registry = checker.check(&probe);
    assert!(registry.entries.iter().all(|entry| entry.available));
    for name in ["git", "ls"] {
        let marker = directory.path().join(format!("{name}-probed"));
        assert_eq!(std::fs::read_to_string(&marker).unwrap(), "x");
        std::fs::remove_file(marker).unwrap();
    }
    probe.config.probe_commands = false;
    assert_eq!(checker.check(&probe).approved(2).applicable, applicable);
    for name in ["git", "ls"] {
        assert!(!directory.path().join(format!("{name}-probed")).exists());
    }
}

#[test]
fn abbreviation_probe_confirms_targets_and_rejects_conflicts() {
    let (_directory, mut shell) = fixture();
    let probe = |shell: &EmbeddedShell, config| Probe {
        serial: 1,
        config,
        context: shell
            .input_context(&TriggerConfig::default(), &Default::default())
            .unwrap(),
    };
    let registry = check(&probe(&shell, probe_config()));
    assert!(
        registry.entries.iter().all(|entry| entry.available),
        "{registry:?}"
    );
    assert_eq!(registry.approved(4).revision, 4);
    shell.run_user_line("gst() { :; }; alias ll='printf original'; alias ls='printf shadow'");
    let registry = check(&probe(&shell, probe_config()));
    for name in ["gst", "ll", "la"] {
        assert!(
            !registry
                .entries
                .iter()
                .find(|entry| entry.name == name)
                .unwrap()
                .available
        );
    }
    let mut config = Config::default();
    config.rules.insert(
        "gst".into(),
        Rule {
            expansion: "git status --short".into(),
            override_existing: true,
        },
    );
    let registry = check(&probe(&shell, config));
    assert_eq!(
        registry.approved(5).definitions["gst"].expansion,
        "git status --short"
    );
}

#[test]
fn abbreviation_probe_missing_or_unsupported_targets_are_not_candidates() {
    let (directory, shell) = fixture();
    std::fs::write(
        directory.path().join("git"),
        "#!/bin/sh\nprintf 'something else\\n'\n",
    )
    .unwrap();
    std::fs::remove_file(directory.path().join("ls")).unwrap();
    let registry = check(&Probe {
        serial: 1,
        config: probe_config(),
        context: shell
            .input_context(&TriggerConfig::default(), &Default::default())
            .unwrap(),
    });
    let approved = registry.approved(1);
    assert_eq!(
        approved.applicable,
        BTreeSet::from(["..".into(), "...".into()])
    );
    assert!(registry.display().contains("could not be confirmed"));
}

#[test]
fn abbreviation_capability_cache_rechecks_file_identity_and_missing_targets() {
    let (directory, shell) = fixture();
    let git = directory.path().join("git");
    std::fs::write(
        &git,
        "#!/bin/sh\nprintf x >> calls\nprintf 'git version 2.40.0\\n'\n",
    )
    .unwrap();
    let probe = Probe {
        serial: 1,
        config: probe_config(),
        context: shell
            .input_context(&TriggerConfig::default(), &Default::default())
            .unwrap(),
    };
    let mut checker = Checker::default();
    assert!(checker.check(&probe).approved(1).applicable.contains("gst"));
    assert!(checker.check(&probe).approved(2).applicable.contains("gst"));
    assert_eq!(
        std::fs::read_to_string(directory.path().join("calls")).unwrap(),
        "x"
    );
    std::fs::write(
        &git,
        "#!/bin/sh\nprintf 'not the supported implementation\\n'\n",
    )
    .unwrap();
    assert!(!checker.check(&probe).approved(3).applicable.contains("gst"));
    std::fs::remove_file(git).unwrap();
    assert!(!checker.check(&probe).approved(4).applicable.contains("gst"));
}

#[test]
fn uutils_ls_requires_a_successful_probe_of_the_expansion_flags() {
    let (directory, shell) = fixture();
    let ls = directory.path().join("ls");
    let probe = Probe {
        serial: 1,
        config: probe_config(),
        context: shell
            .input_context(&TriggerConfig::default(), &Default::default())
            .unwrap(),
    };
    for (exit, available) in [(0, true), (2, false)] {
        std::fs::write(&ls, format!(
            "#!/bin/sh\nif [ \"$1\" = --version ]; then printf 'ls (uutils coreutils) 0.8.0\\n'; exit 0; fi\n\
             [ \"$1\" = -ldAh ] && [ \"$2\" = . ] || exit 9\nexit {exit}\n"
        )).unwrap();
        let registry = check(&probe);
        for name in ["ll", "la"] {
            assert_eq!(registry.approved(1).applicable.contains(name), available);
        }
    }
}

#[test]
fn command_hash_warming_retains_only_confirmed_program_bindings() {
    let (directory, shell) = fixture();
    let previous = shell
        .input_context(&TriggerConfig::default(), &Default::default())
        .unwrap();
    let config = probe_config();
    let registry = check(&Probe {
        serial: 1,
        config: config.clone(),
        context: previous.clone(),
    });
    let mut current = previous.clone();
    current
        .hashed_commands
        .insert("git".into(), directory.path().join("git"));
    current
        .hashed_commands
        .insert("unrelated".into(), directory.path().join("other"));
    assert!(registry.compatible_context(&previous, &current, &config));
    current
        .hashed_commands
        .insert("git".into(), directory.path().join("other-git"));
    assert!(!registry.compatible_context(&previous, &current, &config));
    current = previous.clone();
    current
        .hashed_commands
        .insert("gst".into(), directory.path().join("git"));
    assert!(!registry.compatible_context(&previous, &current, &config));
    current = previous.clone();
    current.aliases.insert("gst".into());
    assert!(!registry.compatible_context(&previous, &current, &config));
    current = previous.clone();
    current.path = Some("/different/path".into());
    assert!(!registry.compatible_context(&previous, &current, &config));
}
