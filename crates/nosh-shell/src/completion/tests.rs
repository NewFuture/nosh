use std::fs;
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

fn answer(server: &mut Server, query: Query, install: Install, script: bool) -> Answer {
    let outcome = server
        .run(query, Some(install), script, &mut |_| Ok(()))
        .unwrap();
    match outcome {
        Outcome::Ready { answer, .. } => answer,
        other => panic!("unexpected completion outcome: {other:?}"),
    }
}

#[test]
fn completion_context_uses_active_command_and_byte_ranges() {
    let (_, _, snapshot) = fixture();
    let mut request = query("echo 中; git switch fe tail; echo untouched");
    request.cursor = "echo 中; git switch fe".len();
    let context = Context::parse(&request, &snapshot.native).unwrap();
    assert_eq!(context.command.as_deref(), Some("git"));
    assert_eq!(context.words, ["git", "switch", "fe", "tail"]);
    assert_eq!(context.index, 2);
    assert_eq!(&request.text[context.span], "fe");
}

#[test]
fn native_paths_include_non_prefix_matches_with_exact_and_prefix_priority() {
    let (directory, _, snapshot) = fixture();
    for name in ["proj", "project", "projects", "my-project"] {
        fs::create_dir(directory.path().join(name)).unwrap();
    }
    let result = answer(
        &mut Server::default(),
        query("cd proj"),
        Install::Native(snapshot.native),
        false,
    );
    assert_eq!(
        result
            .candidates
            .iter()
            .map(|candidate| candidate.value.as_str())
            .collect::<Vec<_>>(),
        ["proj/", "project/", "projects/", "my-project/"]
    );
    assert_eq!(result.state, State::Complete);
}

#[test]
fn scripts_keep_order_options_session_arrays_and_do_not_mutate_user_shell() {
    let (directory, mut shell, _) = fixture();
    assert_eq!(shell.run_user_line(
        "COMP_LINE=original; VALUES=(zebra alpha); custom() { MUTATED=yes; COMPREPLY=(\"${VALUES[0]}\" \"${VALUES[1]}\"); compopt -o nosort -o nospace -o noquote; }; complete -F custom sample"
    ).exit_code, 0);
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    let result = answer(
        &mut Server::default(),
        query("sample "),
        Install::Script {
            native: snapshot.native,
            state: snapshot.script.unwrap(),
        },
        true,
    );
    assert_eq!(result.state, State::Complete);
    assert_eq!(
        result
            .candidates
            .iter()
            .map(|candidate| candidate.value.as_str())
            .collect::<Vec<_>>(),
        ["zebra", "alpha"]
    );
    assert!(
        result
            .candidates
            .iter()
            .all(|candidate| candidate.nospace && candidate.noquote)
    );
    assert_eq!(shell.var("COMP_LINE").as_deref(), Some("original"));
    assert!(shell.var("MUTATED").is_none());
    assert!(!directory.path().join("MUTATED").exists());
}

#[test]
fn missing_functions_are_failed_not_authoritative_empty() {
    let (_, mut shell, _) = fixture();
    assert_eq!(
        shell.run_user_line("complete -F missing sample").exit_code,
        0
    );
    let snapshot = snapshot::capture(&shell, true, &Default::default()).unwrap();
    let result = answer(
        &mut Server::default(),
        query("sample "),
        Install::Script {
            native: snapshot.native,
            state: snapshot.script.unwrap(),
        },
        true,
    );
    assert!(
        matches!(result.state, State::Failed(_)),
        "{:?}",
        result.state
    );
}

#[test]
fn static_make_targets_skip_dynamic_and_recipe_content_without_execution() {
    let (directory, _, _) = fixture();
    fs::write(directory.path().join("Makefile"),
        ".PHONY: clean\nall: input\n\tprintf 'false-target: nope'\ninclude extra.mk\n$(shell touch SIDE_EFFECT):\n").unwrap();
    fs::write(directory.path().join("extra.mk"), "test\\ target:\n").unwrap();
    let set = super::providers::make::targets(directory.path(), &["Makefile".into()], &[]);
    assert_eq!(
        set.entries
            .iter()
            .map(|entry| entry.value.as_str())
            .collect::<Vec<_>>(),
        ["all", "clean", "test target"]
    );
    assert!(set.reason.is_some());
    assert!(!directory.path().join("SIDE_EFFECT").exists());
}

#[cfg(unix)]
#[test]
fn provider_versions_are_reused_and_scoped_to_the_executable() {
    use std::os::unix::fs::PermissionsExt;

    let (directory, _, snapshot) = fixture();
    let bin = directory.path().join("bin");
    fs::create_dir(&bin).unwrap();
    let calls = directory.path().join("versions");
    for (name, version) in [("make", "GNU Make 4.4"), ("gmake", "BSD Make")] {
        let executable = bin.join(name);
        fs::write(
            &executable,
            format!(
                "#!/bin/sh\n[ \"$1\" = --version ] || exit 9\nprintf '{name}\\n' >> '{}'\nprintf '{version}\\n'\n",
                calls.display()
            ),
        )
        .unwrap();
        fs::set_permissions(executable, fs::Permissions::from_mode(0o755)).unwrap();
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

#[test]
fn larger_result_sets_are_partial_and_refiltered_from_the_collection() {
    let (directory, _, snapshot) = fixture();
    for index in 0..300 {
        fs::write(directory.path().join(format!("entry{index:03}")), "").unwrap();
    }
    let mut server = Server::default();
    let first = answer(
        &mut server,
        query("cat entry"),
        Install::Native(snapshot.native.clone()),
        false,
    );
    assert_eq!(first.candidates.len(), MAX_RESULTS);
    assert!(matches!(first.state, State::Partial(_)));
    let result = answer(
        &mut server,
        query("cat entry299"),
        Install::Native(snapshot.native),
        false,
    );
    assert_eq!(result.candidates[0].value, "entry299");
}

#[test]
fn raw_execution_snapshot_is_bounded_and_restores_completion_state() {
    let (_, shell, snapshot) = fixture();
    let state = snapshot.script.unwrap();
    assert!(state.get().len() < MAX_SNAPSHOT);
    let root = Arc::new(serde_json::from_str::<serde_json::Value>(state.get()).unwrap());
    assert!(root.get("env").is_some());
    assert!(root.get("funcs").is_some());
    assert!(root.get("history").is_none());
    assert_eq!(shell.cwd(), snapshot.native.context.cwd);
}

#[test]
fn native_git_switch_uses_real_refs_and_enum_values_not_files() {
    let (directory, _, snapshot) = fixture();
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
    fs::write(directory.path().join("feature-unrelated-file"), "").unwrap();
    let mut server = Server::default();
    let branch = answer(
        &mut server,
        query("git switch fe"),
        Install::Native(snapshot.native.clone()),
        false,
    );
    assert_eq!(branch.state, State::Complete);
    assert_eq!(
        branch
            .candidates
            .iter()
            .map(|candidate| candidate.value.as_str())
            .collect::<Vec<_>>(),
        ["feature-one"]
    );
    assert!(
        branch
            .candidates
            .iter()
            .all(|candidate| candidate.kind == Kind::Branch)
    );
    let enumeration = answer(
        &mut server,
        query("git switch --conflict=di"),
        Install::Native(snapshot.native.clone()),
        false,
    );
    assert_eq!(enumeration.candidates[0].value, "diff3");
    assert_eq!(
        &enumeration.query.text[enumeration.candidates[0].span.clone()],
        "di"
    );
    let commands = answer(
        &mut server,
        query("git sw"),
        Install::Native(snapshot.native),
        false,
    );
    assert_eq!(commands.candidates[0].value, "switch");
}

#[test]
fn make_comments_assignment_forms_nested_definitions_and_conditionals_are_not_targets() {
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
    let result = answer(
        &mut Server::default(),
        query("gc"),
        Install::Native(snapshot.native),
        false,
    );
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
    let cold = answer(
        &mut server,
        query("cat candidate"),
        Install::Native(snapshot.native.clone()),
        false,
    );
    let cold_time = start.elapsed();
    assert_eq!(cold.candidates.len(), MAX_RESULTS);
    assert!(matches!(cold.state, State::Partial(_)));
    let mut hot = Vec::new();
    for _ in 0..20 {
        let start = std::time::Instant::now();
        let narrowed = answer(
            &mut server,
            query("cat candidate4095"),
            Install::Native(snapshot.native.clone()),
            false,
        );
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
