use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use super::context::Context;
use super::snapshot;
use super::types::*;
use super::worker::Server;
use crate::{EmbeddedShell, ShellOptions};

fn fixture() -> (tempfile::TempDir, EmbeddedShell, Snapshot) {
    let directory = tempfile::tempdir().unwrap();
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(directory.path().into()),
        ..Default::default()
    })
    .unwrap();
    assert_eq!(shell.run_user_line("PATH=/usr/bin:/bin").exit_code, 0);
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    (directory, shell, snapshot)
}

fn query(text: &str) -> Query {
    Query {
        text: text.into(),
        cursor: text.len(),
        session: 1,
        epoch: 1,
        trigger: Trigger::Explicit,
    }
}

fn answer(server: &mut Server, query: Query, snapshot: Snapshot) -> Answer {
    let context = Context::parse(&query, &snapshot.native).unwrap();
    let outcome = server
        .run(query, &context, Some(snapshot), &mut |_| Ok(()))
        .unwrap();
    match outcome {
        Outcome::Ready { answer, .. } => answer,
        other => panic!("unexpected completion outcome: {other:?}"),
    }
}

fn candidate_values(answer: &Answer) -> Vec<&str> {
    answer
        .candidates
        .iter()
        .map(|candidate| candidate.value.as_str())
        .collect()
}

fn write_executable(path: impl AsRef<std::path::Path>, contents: &str) {
    let path = path.as_ref();
    fs::write(path, contents).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
}

#[test]
fn completion_context_uses_active_command_and_byte_ranges() {
    let (_, _, mut snapshot) = fixture();
    for operator in [";", "|", "|&", "||", "&&", "&"] {
        let prefix = format!("echo 中 {operator} git switch fe");
        let mut request = query(&format!("{prefix} tail {operator} echo untouched"));
        request.cursor = prefix.len();
        let context = Context::parse(&request, &snapshot.native).unwrap();
        assert_eq!(context.command(), Some("git"), "{operator}");
        assert_eq!(context.words.as_ref(), ["git", "switch", "fe", "tail"]);
        assert_eq!(context.index, 2);
        assert_eq!(&request.text[context.span], "fe");
    }
    let mut request = query("(git sw)");
    request.cursor -= 1;
    let context = Context::parse(&request, &snapshot.native).unwrap();
    assert_eq!(context.words.as_ref(), ["git", "sw"]);
    assert_eq!(&request.text[context.span], "sw");
    assert_eq!(context.command_end, request.cursor);
    let native = Arc::make_mut(&mut snapshot.native);
    native.context.path = None;
    native
        .context
        .functions
        .insert("nosh_pipeline_fixture".into());
    let result = answer(
        &mut Server::default(),
        query("echo x |& nosh_pipe"),
        snapshot,
    );
    assert_eq!(result.candidates[0].value, "nosh_pipeline_fixture");
    assert_eq!(result.candidates[0].source, Source::Command);
}

#[test]
fn compound_command_prefixes_preserve_command_and_argument_positions() {
    let (_, _, snapshot) = fixture();
    for prefix in [
        "if ",
        "if true; then ",
        "if true; then :; else ",
        "while true; do ",
        "until false; do ",
        "{ ",
        "if true; then { ! ",
    ] {
        for (text, index, word) in [("gi", 0, "gi"), ("git switch fe", 2, "fe")] {
            let request = query(&format!("{prefix}{text}"));
            let context = Context::parse(&request, &snapshot.native).unwrap();
            assert_eq!(
                context.command(),
                Some(if index == 0 { "gi" } else { "git" })
            );
            assert_eq!(context.index, index, "{prefix}{text}");
            assert_eq!(&request.text[context.span.clone()], word);
            assert_eq!(&request.text[context.command_start..], text);
        }
    }
    for (text, command) in [
        ("echo then gi", "echo"),
        ("echo '{' gi", "echo"),
        ("'then' gi", "then"),
        ("\"do\" gi", "do"),
        ("\\if gi", "if"),
        ("X=1 then gi", "then"),
    ] {
        let context = Context::parse(&query(text), &snapshot.native).unwrap();
        assert_eq!(context.command(), Some(command), "{text}");
    }
}

#[test]
fn mid_line_token_ranges() {
    let (_, _, snapshot) = fixture();
    let mut request = query("sample first   tail");
    request.cursor = "sample first ".len();
    let context = Context::parse(&request, &snapshot.native).unwrap();
    let words = context.script_words(&request, &snapshot.native).unwrap();
    assert_eq!(words.values, ["sample", "first", "", "tail"]);
    assert_eq!(words.starts, [0, 7, 13, 15]);
    assert_eq!(words.index, 2);
    assert_eq!(words.span, 13..13);
}

#[test]
fn long_unicode_command_chains_keep_byte_ranges_within_the_lookup_budget() {
    let (_, _, snapshot) = fixture();
    let text = format!("{}git switch fe", "echo 中; ".repeat(8192));
    let request = query(&text);
    let started = std::time::Instant::now();
    let context = Context::parse(&request, &snapshot.native).unwrap();
    let elapsed = started.elapsed();
    assert_eq!(context.command(), Some("git"));
    assert_eq!(&text[context.span], "fe");
    assert!(
        elapsed < std::time::Duration::from_millis(500),
        "{elapsed:?}"
    );
    println!("completion context8192={elapsed:?}");
}

#[test]
fn provider_collection_budgets_remain_partial_after_deduplication() {
    let set =
        super::cache::Set::lines(std::iter::repeat_n("same", MAX_SET + 1), Kind::Branch, None);
    assert_eq!(set.entries.len(), 1);
    assert!(set.reason.is_some());
    let long = "x".repeat(MAX_SET_BYTES);
    let set = super::cache::Set::lines(std::iter::once(long.as_str()), Kind::Branch, None);
    assert!(set.entries.is_empty());
    assert!(set.reason.is_some());
    let entry = super::cache::Entry {
        value: "valid".into(),
        kind: Kind::Branch,
        description: None,
    };
    let set =
        super::cache::Set::collect([Err("unreadable entry".into()), Ok(entry.clone())].into_iter());
    assert_eq!(set.entries[0].value, "valid");
    assert_eq!(set.reason.as_deref(), Some("unreadable entry"));
    let oversized = super::cache::Entry {
        description: Some(long),
        ..entry
    };
    let set = super::cache::Set::collect(std::iter::once(Ok(oversized)));
    assert!(set.entries.is_empty());
    assert!(set.reason.is_some());
}

#[test]
fn merged_command_sources_keep_precedence_and_exclude_duplicates_and_invalid_executables() {
    let (directory, _, mut snapshot) = fixture();
    for index in 0..300 {
        write_executable(directory.path().join(format!("fixture{index:03}")), "");
    }
    fs::write(directory.path().join("fixture-invalid"), "").unwrap();
    let native = Arc::make_mut(&mut snapshot.native);
    native.context.path = Some(directory.path().display().to_string());
    native.context.builtins.insert("fixture010".into());
    native.context.aliases.insert("fixture020".into());
    native.context.functions.insert("fixture020".into());
    native
        .context
        .hashed_commands
        .insert("fixture030".into(), directory.path().join("fixture030"));
    native
        .context
        .hashed_commands
        .insert("fixture-hash".into(), directory.path().join("fixture030"));
    let mut server = Server::default();
    let result = answer(&mut server, query("fixture"), snapshot.clone());
    assert_eq!(result.candidates.len(), MAX_RESULTS);
    assert!(matches!(result.state, State::Partial(_)));
    assert_eq!(
        result
            .candidates
            .iter()
            .map(|value| &value.value)
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        MAX_RESULTS
    );
    assert!(
        !result
            .candidates
            .iter()
            .any(|value| value.value == "fixture-invalid")
    );
    for (text, description) in [
        ("fixture010", "builtin"),
        ("fixture020", "alias"),
        ("fixture030", "executable"),
        ("fixture-hash", "hashed executable"),
        ("fixture299", "executable"),
    ] {
        let result = answer(&mut server, query(text), snapshot.clone());
        assert_eq!(result.state, State::Complete);
        assert_eq!(result.candidates.len(), 1);
        assert_eq!(result.candidates[0].value, text);
        assert_eq!(
            result.candidates[0].description.as_deref(),
            Some(description)
        );
    }
}

#[test]
fn directory_collection_retains_symlinks_hidden_names_and_partial_errors() {
    use std::os::unix::{ffi::OsStringExt, fs::symlink};
    let (directory, _, snapshot) = fixture();
    fs::create_dir(directory.path().join("folder")).unwrap();
    symlink("folder", directory.path().join("linked")).unwrap();
    symlink("missing", directory.path().join("broken")).unwrap();
    fs::write(directory.path().join(".hidden"), "").unwrap();
    let non_utf8 = match fs::write(
        directory
            .path()
            .join(std::ffi::OsString::from_vec(vec![0xff])),
        "",
    ) {
        Ok(()) => true,
        Err(error) if error.raw_os_error() == Some(libc::EILSEQ) => false,
        Err(error) => panic!("non-UTF-8 filename fixture: {error}"),
    };
    let mut server = Server::default();
    for (text, expected) in [
        ("cat ", vec!["broken", "folder/", "linked/"]),
        ("cd ", vec!["folder/", "linked/"]),
        ("cat .", vec![".hidden"]),
    ] {
        let result = answer(&mut server, query(text), snapshot.clone());
        assert_eq!(candidate_values(&result), expected);
        if non_utf8 {
            assert!(matches!(result.state, State::Partial(_)));
        } else {
            assert_eq!(result.state, State::Complete);
        }
    }
}

#[test]
fn native_paths_include_non_prefix_matches_with_exact_and_prefix_priority() {
    let (directory, _, snapshot) = fixture();
    for name in ["proj", "project", "projects", "my-project"] {
        fs::create_dir(directory.path().join(name)).unwrap();
    }
    let result = answer(&mut Server::default(), query("cd proj"), snapshot);
    assert_eq!(
        candidate_values(&result),
        ["proj/", "project/", "projects/", "my-project/"]
    );
    assert_eq!(result.state, State::Complete);
}

#[test]
fn script_options_and_session_isolation() {
    let (_, mut shell, _) = fixture();
    assert_eq!(shell.run_user_line(
        "COMP_LINE=original; VALUES=(zebra alpha); custom() { MUTATED=yes; COMPREPLY=(\"${VALUES[0]}\" \"${VALUES[1]}\"); compopt -o nosort -o nospace -o noquote; }; complete -F custom sample"
    ).exit_code, 0);
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    let result = answer(&mut Server::default(), query("sample "), snapshot);
    assert_eq!(result.state, State::Complete);
    assert_eq!(candidate_values(&result), ["zebra", "alpha"]);
    assert!(
        result
            .candidates
            .iter()
            .all(|candidate| candidate.nospace && candidate.noquote)
    );
    assert_eq!(shell.var("COMP_LINE").as_deref(), Some("original"));
    assert!(shell.var("MUTATED").is_none());
    assert_eq!(
        shell
            .run_user_line("plain() { COMPREPLY=('space name'); }")
            .exit_code,
        0
    );
    for (options, noquote) in [
        ("", true),
        ("-o filenames", false),
        ("-o filenames -o noquote", true),
    ] {
        assert_eq!(
            shell
                .run_user_line(&format!("complete {options} -F plain sample"))
                .exit_code,
            0
        );
        let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
        let result = answer(&mut Server::default(), query("sample "), snapshot);
        assert_eq!(result.candidates[0].value, "space name");
        assert_eq!(result.candidates[0].noquote, noquote);
    }
    assert_eq!(shell.run_user_line(
        "FALLBACK_VARIABLE=value; empty() { COMPREPLY=(); }; complete -o bashdefault -F empty sample"
    ).exit_code, 0);
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    for (text, value) in [
        ("sample $FALLBACK_V", "$FALLBACK_VARIABLE"),
        ("sample ${FALLBACK_V", "${FALLBACK_VARIABLE}"),
    ] {
        let result = answer(&mut Server::default(), query(text), snapshot.clone());
        assert_eq!(result.state, State::Complete);
        assert_eq!(candidate_values(&result), [value]);
        assert_eq!(result.candidates[0].source, Source::Variable);
    }
}

#[test]
fn disabled_scripts_keep_native_sources_and_isolated_path_expansion() {
    let (directory, mut shell, _) = fixture();
    fs::write(directory.path().join("native_file"), "").unwrap();
    assert_eq!(
        shell.run_user_line(
            "ROOT=.; nosh_native_fixture() { :; }; scripted() { : > provider_called; COMPREPLY=(scripted); }; \
             complete -F scripted git sample; complete -D -F scripted; complete -I -F scripted"
        ).exit_code,
        0
    );
    let snapshot = snapshot::capture(&shell, false, &Default::default()).unwrap();
    let mut server = Server::default();
    for (text, value, source) in [
        ("nosh_native_f", "nosh_native_fixture", Source::Command),
        ("git sw", "switch", Source::Git),
        ("sample nat", "native_file", Source::Path),
        ("sample $ROOT/nat", "./native_file", Source::Path),
    ] {
        let request = query(text);
        let context = Context::parse(&request, &snapshot.native).unwrap();
        assert_eq!(context.requires_execution, text.contains("$ROOT"));
        let result = answer(&mut server, request, snapshot.clone());
        assert_eq!(result.state, State::Complete, "{text}");
        assert!(
            result
                .candidates
                .iter()
                .any(|candidate| candidate.value == value && candidate.source == source),
            "{text}: {:?}",
            result.candidates
        );
    }
    assert!(!directory.path().join("provider_called").exists());
}

#[test]
fn plusdirs_preserves_script_order_and_deduplicates_directories() {
    let (directory, mut shell, _) = fixture();
    for name in ["alpha", "beta", "zebra"] {
        fs::create_dir(directory.path().join(name)).unwrap();
    }
    assert_eq!(
        shell.run_user_line(
            "directories() { COMPREPLY=(zebra alpha/); }; complete -o filenames -o nosort -o plusdirs -F directories sample"
        ).exit_code,
        0
    );
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    let result = answer(&mut Server::default(), query("sample "), snapshot);
    assert_eq!(result.state, State::Complete);
    assert_eq!(
        result
            .candidates
            .iter()
            .map(|candidate| (candidate.value.as_str(), candidate.source.clone()))
            .collect::<Vec<_>>(),
        [
            ("zebra/", Source::Script),
            ("alpha/", Source::Script),
            ("beta/", Source::Path)
        ]
    );
}

#[test]
fn provider_environment_uses_exported_values_with_bounded_capture() {
    let (_, mut shell, _) = fixture();
    assert_eq!(
        shell.run_user_line(
            "unset GIT_DIR MAKEFILES GIT_CONFIG_VALUE_1; GIT_DIR=local-only; MAKEFILES=local.mk; \
             export HOME=provider-home GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.abbrev GIT_CONFIG_VALUE_0=9 GIT_CONFIG_VALUE_1"
        ).exit_code,
        0
    );
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    for name in ["GIT_DIR", "MAKEFILES", "GIT_CONFIG_VALUE_1"] {
        assert!(!snapshot.native.environment.contains_key(name), "{name}");
    }
    assert!(snapshot.native.variables.contains("GIT_DIR"));
    for (name, value) in [
        ("HOME", "provider-home"),
        ("GIT_CONFIG_COUNT", "1"),
        ("GIT_CONFIG_KEY_0", "core.abbrev"),
        ("GIT_CONFIG_VALUE_0", "9"),
    ] {
        assert_eq!(
            snapshot.native.environment.get(name).map(String::as_str),
            Some(value)
        );
    }
    assert_eq!(shell.run_user_line("export -n HOME").exit_code, 0);
    let unexported = snapshot::capture(&shell, true, &Default::default()).unwrap();
    assert_eq!(
        unexported.native.context.home.as_deref(),
        Some("provider-home")
    );
    assert!(!unexported.native.environment.contains_key("HOME"));
    let (_, shared) = shell.shared();
    let mut oversized =
        brush_core::ShellVariable::new("x".repeat(crate::input_assist::MAX_CONTEXT));
    oversized.export();
    shared
        .lock()
        .unwrap()
        .env_mut()
        .set_global("GIT_CONFIG_VALUE_0", oversized)
        .unwrap();
    assert!(snapshot::capture(&shell, true, &Default::default()).is_err());
}

#[test]
fn provider_process_environment_comes_from_the_snapshot() {
    if std::env::var_os("NOSH_TEST_PROVIDER_ENV").is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "completion::tests::provider_process_environment_comes_from_the_snapshot",
                "--nocapture",
            ])
            .env("NOSH_TEST_PROVIDER_ENV", "1")
            .env("GIT_DIR", "stale-host-directory")
            .env("GIT_WORK_TREE", "stale-host-worktree")
            .env("MAKEFILES", "stale-host.mk")
            .env(crate::procs::RUN_VAR, "provider-environment-fixture")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    let (directory, mut shell, _) = fixture();
    write_executable(
        directory.path().join("make"),
        "#!/bin/sh\n[ \"$1\" = --version ] || exit 9\n\
         [ -z \"${GIT_DIR+x}${GIT_WORK_TREE+x}${MAKEFILES+x}${HOME+x}\" ] || exit 10\n\
         [ \"$NOSH_AGENT_RUN\" = provider-environment-fixture ] || exit 11\n\
         [ \"$GIT_CONFIG_KEY_0\" = core.abbrev ] && [ \"$GIT_CONFIG_VALUE_0\" = 9 ] || exit 12\n\
         printf 'GNU Make 4.4\\n'\n",
    );
    assert_eq!(
        shell
            .run_user_line(
                "PATH=.; HOME=provider-home; export -n HOME; unset GIT_DIR GIT_WORK_TREE MAKEFILES; \
         export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.abbrev GIT_CONFIG_VALUE_0=9"
            )
            .exit_code,
        0
    );
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    let result = answer(&mut Server::default(), query("make --dry"), snapshot);
    assert_eq!(result.state, State::Complete);
    assert_eq!(result.candidates[0].value, "--dry-run");
}

#[test]
fn autoloaded_unavailable_definitions_survive_worker_replacement() {
    let (directory, mut shell, _) = fixture();
    assert_eq!(shell.run_user_line(
        "load() { printf x >> loads; complete -r -D; complete -A job jobsample; return 124; }; complete -D -F load"
    ).exit_code, 0);
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    let request = query("jobsample ");
    let context = Context::parse(&request, &snapshot.native).unwrap();
    let Outcome::Ready {
        answer: first,
        snapshot: Some(checkpoint),
    } = Server::default()
        .run(request.clone(), &context, Some(snapshot), &mut |_| Ok(()))
        .unwrap()
    else {
        panic!("missing autoload checkpoint")
    };
    assert!(matches!(first.state, State::Unavailable(_)));
    let next = answer(&mut Server::default(), request, checkpoint);
    assert!(matches!(next.state, State::Unavailable(_)));
    assert_eq!(
        fs::read_to_string(directory.path().join("loads")).unwrap(),
        "x"
    );
}

#[test]
fn static_make_targets_skip_dynamic_and_recipe_content_without_execution() {
    let (directory, _, _) = fixture();
    fs::write(
        directory.path().join("Makefile"),
        concat!(
            ".PHONY: clean\nall: input\n",
            "\tprintf '%s' \\\n",
            "false-target: \\\n",
            "also-false:\n",
            "\tprintf '%s' \\\\\n",
            "real-target:\n",
            "include extra.mk\n$(shell touch SIDE_EFFECT):\n",
        ),
    )
    .unwrap();
    fs::write(directory.path().join("extra.mk"), "test\\ target:\n").unwrap();
    let set = super::providers::make::targets(directory.path(), &["Makefile".into()], &[]);
    assert_eq!(
        set.entries
            .iter()
            .map(|entry| entry.value.as_str())
            .collect::<Vec<_>>(),
        ["all", "clean", "real-target", "test target"]
    );
    assert!(set.reason.is_some());
    assert!(!directory.path().join("SIDE_EFFECT").exists());
}

#[test]
fn makeflags_include_directories_are_used_for_static_targets() {
    let (directory, _, mut snapshot) = fixture();
    write_executable(
        directory.path().join("make"),
        "#!/bin/sh\nprintf 'GNU Make 4.4\\n'\n",
    );
    fs::write(
        directory.path().join("Makefile"),
        "include targets.mk\nlocal:\n",
    )
    .unwrap();
    for (name, target) in [("first", "first_target"), ("second path", "second_target")] {
        fs::create_dir(directory.path().join(name)).unwrap();
        fs::write(
            directory.path().join(name).join("targets.mk"),
            format!("{target}:\n"),
        )
        .unwrap();
    }
    Arc::make_mut(&mut snapshot.native).context.path = Some(".".into());
    let mut server = Server::default();
    for (flags, expected) in [
        ("-Ifirst", "first_target"),
        ("-I first", "first_target"),
        ("I first", "first_target"),
        ("ks -I first", "first_target"),
        ("--include-dir=first", "first_target"),
        ("--include-dir first", "first_target"),
        ("-Isecond\\ path", "second_target"),
        ("--include-dir=second\\ path", "second_target"),
        ("-C ignored -f ignored.mk -I first", "first_target"),
        ("-f -Isecond\\ path -I first", "first_target"),
    ] {
        Arc::make_mut(&mut snapshot.native)
            .environment
            .insert("MAKEFLAGS".into(), flags.into());
        let result = answer(&mut server, query("make "), snapshot.clone());
        assert_eq!(result.state, State::Complete, "{flags}");
        let values: std::collections::BTreeSet<_> = result
            .candidates
            .iter()
            .map(|candidate| candidate.value.as_str())
            .collect();
        assert_eq!(values, [expected, "local"].into(), "{flags}");
    }
    for flags in [
        "-I",
        "--include-dir=",
        "-kIfirst",
        "-I$(dynamic)",
        "-- -Ifirst",
    ] {
        Arc::make_mut(&mut snapshot.native)
            .environment
            .insert("MAKEFLAGS".into(), flags.into());
        let result = answer(&mut server, query("make "), snapshot.clone());
        assert!(matches!(result.state, State::Partial(_)), "{flags}");
        assert_eq!(result.candidates[0].value, "local");
    }
    Arc::make_mut(&mut snapshot.native)
        .environment
        .remove("MAKEFLAGS");
    for text in [
        "make -Cfirst ",
        "make -ftargets.mk ",
        "make -Ifirst ",
        "make -Cfirst",
        "make -ftargets.mk",
        "make -Ifirst",
        "make -Cfirst --file=",
        "make -Cfirst --d",
    ] {
        let result = answer(&mut server, query(text), snapshot.clone());
        assert!(
            matches!(result.state, State::Unavailable(_)),
            "{text}: {:?}",
            result.state
        );
        assert!(result.candidates.is_empty());
    }
    fs::write(directory.path().join("-Cfile"), "other_target:\n").unwrap();
    let result = answer(&mut server, query("make -f -Cfile oth"), snapshot.clone());
    assert_eq!(result.state, State::Complete);
    assert_eq!(candidate_values(&result), ["other_target"]);
    fs::write(directory.path().join("Makefile"), "-Cfirst:\n").unwrap();
    let result = answer(&mut Server::default(), query("make -- -Cf"), snapshot);
    assert_eq!(result.state, State::Complete);
    assert_eq!(candidate_values(&result), ["-Cfirst"]);
}

#[cfg(unix)]
#[test]
fn provider_versions_are_reused_and_scoped_to_the_executable() {
    let (directory, _, snapshot) = fixture();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let calls = directory.path().join("versions");
    for (name, version) in [("make", "GNU Make 4.4"), ("gmake", "BSD Make")] {
        write_executable(
            bin.join(name),
            &format!(
                "#!/bin/sh\n[ \"$1\" = --version ] || exit 9\nprintf '{name}\\n' >> '{}'\nprintf '{version}\\n'\n",
                calls.display()
            ),
        );
    }
    let mut native = snapshot.native.as_ref().clone();
    native.context.path = Some(bin.display().to_string());
    let mut cache = super::cache::Cache::default();
    for _ in 0..2 {
        for name in ["make", "gmake"] {
            let request = query(&format!("{name} --d"));
            let context = Context::parse(&request, &native).unwrap();
            let result =
                super::providers::generate(request, &context, &native, &mut cache).unwrap();
            if name == "make" {
                assert_eq!(result.state, State::Complete);
                assert!(
                    result
                        .candidates
                        .iter()
                        .any(|value| value.value == "--dry-run")
                );
            } else {
                assert!(matches!(result.state, State::Failed(_)));
            }
        }
    }
    assert_eq!(fs::read_to_string(calls).unwrap(), "make\ngmake\n");
}

fn package_fixture() -> (tempfile::TempDir, Snapshot) {
    let (directory, _, mut snapshot) = fixture();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    for name in ["npm", "yarn"] {
        write_executable(
            bin.join(name),
            "#!/bin/sh\ntouch manager_executed\nexit 9\n",
        );
    }
    Arc::make_mut(&mut snapshot.native).context.path = Some(bin.display().to_string());
    (directory, snapshot)
}

#[test]
fn scoped_path_overrides_ignore_stale_command_hashes_for_all_native_sources() {
    let (directory, mut snapshot) = package_fixture();
    let old = directory.path().join("bin").join("npm");
    Arc::make_mut(&mut snapshot.native)
        .context
        .hashed_commands
        .insert("npm".into(), old);
    let empty = directory.path().join("empty");
    fs::create_dir(&empty).unwrap();
    fs::write(
        directory.path().join("package.json"),
        r#"{"scripts":{"build":"compile"}}"#,
    )
    .unwrap();
    let mut server = Server::default();
    let result = answer(
        &mut server,
        query(&format!("PATH={} npm run b", empty.display())),
        snapshot.clone(),
    );
    assert!(matches!(result.state, State::Failed(_)));
    let commands = answer(
        &mut server,
        query(&format!("PATH={} np", empty.display())),
        snapshot,
    );
    assert!(
        commands
            .candidates
            .iter()
            .all(|candidate| candidate.value != "npm")
    );
}

#[test]
fn package_scripts_complete_npm_and_yarn_without_running_the_manager_or_scripts() {
    let (directory, snapshot) = package_fixture();
    fs::write(
        directory.path().join("package.json"),
        r#"{"scripts":{
        "build":"touch script_executed","build:prod":"build --production","dev":"serve"
    }}"#,
    )
    .unwrap();
    let mut server = Server::default();
    for text in ["npm run b", "npm run-script b", "yarn run b", "yarn b"] {
        let result = answer(&mut server, query(text), snapshot.clone());
        assert_eq!(result.state, State::Complete);
        assert_eq!(candidate_values(&result), ["build", "build:prod"]);
        assert_eq!(
            result.candidates[0].description.as_deref(),
            Some("touch script_executed")
        );
        assert_eq!(&result.query.text[result.candidates[0].span.clone()], "b");
    }
    let verbs = answer(&mut server, query("npm ru"), snapshot);
    assert_eq!(candidate_values(&verbs), ["run", "run-script"]);
    assert!(!directory.path().join("manager_executed").exists());
    assert!(!directory.path().join("script_executed").exists());
}

#[test]
fn package_directory_options_nearest_manifest_and_mid_line_spans_are_respected() {
    let (directory, snapshot) = package_fixture();
    fs::write(
        directory.path().join("package.json"),
        r#"{"scripts":{"root":"root command"}}"#,
    )
    .unwrap();
    for (text, option) in [("npm --p", "--prefix="), ("yarn --c", "--cwd=")] {
        let result = answer(&mut Server::default(), query(text), snapshot.clone());
        assert_eq!(result.candidates[0].value, option);
        assert!(result.candidates[0].nospace);
    }
    let project = directory.path().join("child project");
    fs::create_dir(&project).unwrap();
    fs::create_dir(project.join("src")).unwrap();
    fs::write(
        project.join("package.json"),
        r#"{"scripts":{"child":"child command"}}"#,
    )
    .unwrap();
    for text in [
        "npm --prefix 'child project' run ch tail",
        "npm --prefix='child project' run ch tail",
        "yarn --cwd 'child project' run ch tail",
    ] {
        let mut request = query(text);
        request.cursor = text.find("ch tail").unwrap() + 2;
        let result = answer(&mut Server::default(), request, snapshot.clone());
        assert_eq!(result.candidates[0].value, "child");
        assert_eq!(&result.query.text[result.candidates[0].span.clone()], "ch");
        assert!(result.query.text.ends_with(" tail"));
    }
    let mut nested = snapshot;
    Arc::make_mut(&mut nested.native).context.cwd = project.join("src");
    let result = answer(&mut Server::default(), query("yarn ch"), nested);
    assert_eq!(result.candidates[0].value, "child");
}

#[test]
fn package_command_boundaries() {
    let (directory, snapshot) = package_fixture();
    fs::write(
        directory.path().join("package.json"),
        r#"{"scripts":{"install":"setup","build":"compile"}}"#,
    )
    .unwrap();
    let mut server = Server::default();
    let shortcut = answer(&mut server, query("yarn inst"), snapshot.clone());
    assert!(shortcut.candidates.is_empty());
    let explicit = answer(&mut server, query("yarn run inst"), snapshot.clone());
    assert_eq!(explicit.candidates[0].value, "install");
    for text in [
        "npm install fi",
        "npm run build arg",
        "yarn add arg",
        "yarn run build --cwd ",
        "npm run build --prefix=child",
    ] {
        let result = answer(&mut server, query(text), snapshot.clone());
        assert!(matches!(result.state, State::Unavailable(_)));
        assert!(result.candidates.is_empty());
    }
}

#[test]
fn package_manifest_validation_and_refiltering() {
    let (directory, snapshot) = package_fixture();
    let manifest = directory.path().join("package.json");
    for text in [
        "{".into(),
        r#"{"scripts":{"bad":123}}"#.into(),
        " ".repeat(2 * 1024 * 1024 + 1),
    ] {
        fs::write(&manifest, text).unwrap();
        let result = answer(&mut Server::default(), query("npm run "), snapshot.clone());
        assert!(matches!(result.state, State::Failed(_)));
    }
    let scripts: std::collections::BTreeMap<_, _> = (0..300)
        .map(|index| (format!("task{index:03}"), "echo task"))
        .collect();
    fs::write(
        &manifest,
        serde_json::to_vec(&serde_json::json!({"scripts":scripts})).unwrap(),
    )
    .unwrap();
    let mut server = Server::default();
    let first = answer(&mut server, query("npm run task"), snapshot.clone());
    assert_eq!(first.candidates.len(), MAX_RESULTS);
    assert!(matches!(first.state, State::Partial(_)));
    let narrowed = answer(&mut server, query("npm run task299"), snapshot);
    assert_eq!(narrowed.state, State::Complete);
    assert_eq!(narrowed.candidates[0].value, "task299");
}

#[test]
fn loaded_package_completion_definitions_override_builtin_script_names() {
    let (directory, mut shell, _) = fixture();
    fs::write(
        directory.path().join("package.json"),
        r#"{"scripts":{"build":"compile"}}"#,
    )
    .unwrap();
    assert_eq!(
        shell
            .run_user_line("custom() { COMPREPLY=(provided); }; complete -F custom npm")
            .exit_code,
        0
    );
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    let result = answer(&mut Server::default(), query("npm run "), snapshot);
    assert_eq!(result.candidates[0].value, "provided");
    assert_eq!(result.candidates[0].source, Source::Script);
}

#[test]
fn native_git_switch_uses_real_refs_and_enum_values_not_files() {
    let (directory, mut shell, _) = fixture();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(directory.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--quiet"]);
    git(&[
        "-c",
        "user.name=fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "commit",
        "--allow-empty",
        "--quiet",
        "-m",
        "fixture",
    ]);
    git(&["branch", "feature-one"]);
    git(&["tag", "feature-tag"]);
    git(&["update-ref", "refs/remotes/upstream/feature-two", "HEAD"]);
    assert_eq!(
        shell
            .run_user_line(
                "unset GIT_DIR; GIT_DIR=not-exported; \
             export GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.abbrev GIT_CONFIG_VALUE_0=9 \
             GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_NOSYSTEM=1"
            )
            .exit_code,
        0
    );
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    fs::write(directory.path().join("feature-unrelated-file"), "").unwrap();
    fs::create_dir(directory.path().join("feature-directory")).unwrap();
    let mut server = Server::default();
    let directory_value = answer(&mut server, query("git -C fe"), snapshot.clone());
    assert_eq!(candidate_values(&directory_value), ["feature-directory/"]);
    for option in [
        "-c new",
        "-C new",
        "--create new",
        "--force-create new",
        "--orphan new",
        "--create=new",
        "--force-create=new",
        "--orphan=new",
    ] {
        let result = answer(
            &mut server,
            query(&format!("git switch {option}")),
            snapshot.clone(),
        );
        assert!(
            matches!(result.state, State::Unavailable(_)),
            "{option}: {:?}",
            result.state
        );
        assert!(result.candidates.is_empty());
    }
    let branch = answer(&mut server, query("git switch fe"), snapshot.clone());
    assert_eq!(branch.state, State::Complete);
    assert_eq!(candidate_values(&branch), ["feature-one"]);
    assert!(
        branch
            .candidates
            .iter()
            .all(|candidate| candidate.kind == Kind::Branch)
    );
    for (options, prefix, expected) in [
        ("--track", "up", vec!["upstream/feature-two"]),
        ("-t", "up", vec!["upstream/feature-two"]),
        ("--track=direct", "up", vec!["upstream/feature-two"]),
        ("-c child --track=direct", "fe", vec!["feature-one"]),
        ("-c child --track=inherit", "fe", vec!["feature-one"]),
        ("--track --no-track", "fe", vec!["feature-one"]),
        ("-c child", "fe", vec!["feature-one", "feature-tag"]),
    ] {
        let result = answer(
            &mut server,
            query(&format!("git switch {options} {prefix}")),
            snapshot.clone(),
        );
        assert_eq!(result.state, State::Complete, "{options}");
        assert_eq!(candidate_values(&result), expected, "{options}");
    }
    let enumeration = answer(
        &mut server,
        query("git switch --conflict=di"),
        snapshot.clone(),
    );
    assert_eq!(enumeration.candidates[0].value, "diff3");
    assert_eq!(
        &enumeration.query.text[enumeration.candidates[0].span.clone()],
        "di"
    );
    let commands = answer(&mut server, query("git sw"), snapshot);
    assert_eq!(commands.candidates[0].value, "switch");
}

#[test]
fn make_static_target_parsing() {
    let (directory, _, _) = fixture();
    fs::write(
        directory.path().join("Makefile"),
        concat!(
            "comment#fake-target: ignored\n",
            "NAME ::= value\n",
            "define outer\n",
            "define inner\n",
            "endef\n",
            "hidden: should-not-appear\n",
            "endef\n",
            "ifdef SOMETHING\nmaybe: ignored\nendif # end\n",
            "elsewhere:\n",
            "include\textra.mk\n",
            "escaped\\:colon:\n",
            ".PHONY: clean; echo not-a-target\n",
            "%.out: %.in\n",
        ),
    )
    .unwrap();
    fs::write(directory.path().join("extra.mk"), "included:\n").unwrap();
    let set = super::providers::make::targets(directory.path(), &["Makefile".into()], &[]);
    assert_eq!(
        set.entries
            .iter()
            .map(|entry| entry.value.as_str())
            .collect::<Vec<_>>(),
        ["clean", "elsewhere", "escaped:colon", "included"]
    );
    assert!(set.reason.is_some());
}

#[test]
fn owner_approved_abbreviations_show_exact_expansion_and_exclude_disabled_rules() {
    let (_, shell, _) = fixture();
    let rules = crate::input_assist::Abbreviations {
        revision: 9,
        applicable: ["gco".into()].into(),
        definitions: [
            (
                "gco".into(),
                crate::input_assist::Abbreviation {
                    expansion: "git checkout".into(),
                    source: "user rule".into(),
                },
            ),
            (
                "gone".into(),
                crate::input_assist::Abbreviation {
                    expansion: "touch SHOULD_NOT_RUN".into(),
                    source: "disabled rule".into(),
                },
            ),
        ]
        .into(),
    };
    let snapshot = snapshot::capture(&shell, true, &rules).unwrap();
    let result = answer(&mut Server::default(), query("gc"), snapshot);
    let expanded = result
        .candidates
        .iter()
        .find(|candidate| candidate.kind == Kind::Abbreviation)
        .unwrap();
    assert_eq!(expanded.display.as_deref(), Some("gco"));
    assert_eq!(expanded.value, "git checkout");
    assert!(expanded.description.as_ref().unwrap().contains("user rule"));
    assert!(expanded.noquote);
    assert!(
        !result
            .candidates
            .iter()
            .any(|candidate| candidate.value.contains("SHOULD_NOT_RUN"))
    );
}

#[test]
fn cold_hot_collection_reuse_preserves_candidates_beyond_the_first_screen() {
    let (directory, _, snapshot) = fixture();
    for index in 0..4096 {
        fs::write(directory.path().join(format!("candidate{index:04}")), "").unwrap();
    }
    let mut server = Server::default();
    let start = std::time::Instant::now();
    let cold = answer(&mut server, query("cat candidate"), snapshot.clone());
    let cold_time = start.elapsed();
    assert_eq!(cold.candidates.len(), MAX_RESULTS);
    assert!(matches!(cold.state, State::Partial(_)));
    let mut hot = Vec::new();
    for _ in 0..20 {
        let start = std::time::Instant::now();
        let narrowed = answer(&mut server, query("cat candidate4095"), snapshot.clone());
        assert_eq!(narrowed.candidates[0].value, "candidate4095");
        assert_eq!(narrowed.state, State::Complete);
        hot.push(start.elapsed());
    }
    hot.sort();
    println!(
        "completion fixture4096 cold={cold_time:?} hot_p50={:?} hot_p95={:?} target_rank=1",
        hot[10], hot[19]
    );
    assert!(cold_time < std::time::Duration::from_millis(500));
    assert!(hot[19] < std::time::Duration::from_millis(100));
}
