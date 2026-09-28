use nosh_permissions::{
    ApprovalMode::{self, Auto, Confirm, Yolo},
    Context, Decision, DecisionSource, RuleSpec, SessionAllowList, UserRule, UserRules,
    assess_command, assess_read, evaluate,
};

fn fixture() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::create_dir(root.join("child")).unwrap();
    let context = Context::new(&root, &root).with_home(root.join("home"));
    (dir, context)
}

fn policy(
    command: &str,
    mode: ApprovalMode,
    context: &Context,
    rules: &UserRules,
) -> nosh_permissions::PolicyDecision {
    evaluate(
        &assess_command(command, context),
        mode,
        rules,
        &SessionAllowList::default(),
    )
}

#[test]
fn defaults_and_non_monotonic_matrix() {
    let (_dir, context) = fixture();
    let rules = UserRules::default();
    assert_eq!(ApprovalMode::default(), Auto);
    for mode in [Confirm, Auto, Yolo] {
        assert_eq!(
            policy("pwd", mode, &context, &rules).decision,
            Decision::Allow
        );
        let forbidden = policy("rm -rf /", mode, &context, &rules);
        assert!(matches!(forbidden.source, DecisionSource::Builtin(_)));
        if mode == Confirm {
            assert_eq!(forbidden.decision, Decision::Ask { strong: true });
        } else {
            assert!(matches!(forbidden.decision, Decision::Deny { .. }));
        }
        assert_eq!(
            policy("rm -rf build", mode, &context, &rules).decision,
            if mode == Yolo {
                Decision::Allow
            } else {
                Decision::Ask { strong: true }
            }
        );
        assert_eq!(
            policy("cd child", mode, &context, &rules).decision,
            if mode == Confirm {
                Decision::Ask { strong: false }
            } else {
                Decision::Allow
            }
        );
    }
}

#[test]
fn deny_wins_over_user_allow_and_builtin_risk_in_every_mode() {
    let (_dir, context) = fixture();
    let mut rules = UserRules {
        allow: vec![UserRule::prefix("rm").unwrap()],
        deny: vec![UserRule::prefix("rm").unwrap()],
    };
    for mode in [Confirm, Auto, Yolo] {
        let decision = policy("rm -rf /", mode, &context, &rules);
        assert!(matches!(decision.decision, Decision::Deny { .. }));
        assert!(matches!(decision.source, DecisionSource::UserDeny(_)));
    }
    rules.deny.clear();
    for mode in [Confirm, Auto, Yolo] {
        let decision = policy("rm -rf /", mode, &context, &rules);
        assert_eq!(decision.decision, Decision::Allow);
        assert!(matches!(decision.source, DecisionSource::UserAllow(_)));
    }
}

#[test]
fn common_builds_are_an_explicit_convenience_exception() {
    let (_dir, context) = fixture();
    let rules = UserRules::default();
    for command in [
        "cargo build --release",
        "cargo test --workspace",
        "cargo check",
        "cargo clippy",
        "cargo fmt --check",
        "cargo fmt -- --check",
        "go build ./...",
        "go test ./...",
        "npm run build",
        "npm test",
        "pnpm run test",
        "yarn build",
        "make",
        "make -j8 test",
        "ninja",
        "cmake --build build",
        "cmake --build build --target all --config Release --parallel 2",
        "ctest",
        "pytest -q",
        "python3 -m pytest",
        "mvn test",
        "mvn -q test",
        "mvn test verify -pl app",
        "gradle build",
        "gradle --no-daemon test",
        "gradle build test -p app",
        "sbt -batch test",
        "bazel build //app:binary",
    ] {
        let decision = policy(command, Auto, &context, &rules);
        assert_eq!(
            decision.decision,
            Decision::Allow,
            "{command}: {decision:?}"
        );
        assert!(
            matches!(decision.source, DecisionSource::Automatic(ref a) if a.development),
            "{command}: {decision:?}"
        );
        assert_eq!(
            policy(command, Confirm, &context, &rules).decision,
            Decision::Ask { strong: false },
            "{command}"
        );
    }
    for command in [
        "cargo run",
        "cargo publish",
        "cargo fmt",
        "cargo clippy --fix",
        "cargo clippy --fix --allow-dirty",
        "npm run deploy",
        "make install",
        "python3 unknown.py",
        "some-unknown-program",
        "npx arbitrary-package",
        "mvn -q",
        "cargo test && some-unknown-program",
        "cargo test > unknown.log",
    ] {
        assert!(
            matches!(
                policy(command, Auto, &context, &rules).decision,
                Decision::Ask { .. }
            ),
            "{command}"
        );
    }
    assert!(matches!(
        policy("cargo test && rm -rf /", Auto, &context, &rules).decision,
        Decision::Deny { .. }
    ));
}

#[test]
fn pytest_basetemp_allows_temp_paths_but_guards_other_directories() {
    let (_dir, context) = fixture();
    let rules = UserRules::default();
    for command in [
        "pytest --basetemp=/tmp/nosh-pytest-basetemp",
        "python -m pytest --basetemp /tmp/nosh-pytest-basetemp",
    ] {
        assert_eq!(
            policy(command, Auto, &context, &rules).decision,
            Decision::Allow,
            "{command}"
        );
    }
    for command in [
        "pytest --basetemp=child",
        "pytest --basetemp child",
        "python -m pytest --basetemp=child",
        "python3 -m pytest --basetemp child",
    ] {
        assert_eq!(
            policy(command, Auto, &context, &rules).decision,
            Decision::Ask { strong: true },
            "{command}"
        );
    }
    for command in [
        "pytest --basetemp=/etc",
        "pytest --basetemp /etc",
        "python -m pytest --basetemp=/etc",
        "python3 -m pytest --basetemp /etc",
    ] {
        assert!(
            !matches!(
                policy(command, Auto, &context, &rules).decision,
                Decision::Allow
            ),
            "{command}"
        );
    }
    for command in ["pytest --basetem=/etc", "python -m pytest --basetem=/etc"] {
        assert_eq!(
            policy(command, Auto, &context, &rules).decision,
            Decision::Allow,
            "{command}"
        );
    }
}

#[test]
fn actual_effects_and_argument_boundaries_control_whitelists() {
    let (_dir, context) = fixture();
    let mut rules = UserRules {
        allow: vec![UserRule::prefix("git fetch").unwrap()],
        deny: vec![],
    };
    assert!(matches!(
        policy("git fetch --all", Confirm, &context, &rules).source,
        DecisionSource::UserAllow(_)
    ));
    for command in [
        "git fetcher",
        "git -C other fetch",
        "git fetch && touch x",
        "git fetch > x",
    ] {
        assert!(
            !matches!(
                policy(command, Confirm, &context, &rules).source,
                DecisionSource::UserAllow(_)
            ),
            "{command}"
        );
    }
    rules.allow = vec![
        UserRule::compile(
            RuleSpec {
                command_prefix: Some("printf".into()),
                write_paths: vec!["out.txt".into()],
                ..RuleSpec::default()
            },
            "test rule",
        )
        .unwrap(),
    ];
    assert!(matches!(
        policy("printf hi > out.txt", Confirm, &context, &rules).source,
        DecisionSource::UserAllow(_)
    ));
    assert!(!matches!(
        policy("printf hi > elsewhere.txt", Confirm, &context, &rules).source,
        DecisionSource::UserAllow(_)
    ));
    rules.allow = vec![UserRule::exact("printf '%s' 'a b'").unwrap()];
    assert!(matches!(
        policy("printf \"%s\" \"a b\"", Confirm, &context, &rules).source,
        DecisionSource::UserAllow(_)
    ));
    assert!(!matches!(
        policy("printf %s a b", Confirm, &context, &rules).source,
        DecisionSource::UserAllow(_)
    ));
}

#[test]
fn scripts_wrappers_and_substitutions_do_not_hide_user_deny() {
    let (dir, context) = fixture();
    std::fs::write(dir.path().join("script.sh"), "docker system prune -af\n").unwrap();
    let rules = UserRules {
        allow: vec![
            UserRule::compile(
                RuleSpec {
                    tool: Some("run_command".into()),
                    allow_opaque: true,
                    ..RuleSpec::default()
                },
                "broad allow",
            )
            .unwrap(),
        ],
        deny: vec![UserRule::prefix("docker system prune").unwrap()],
    };
    for command in [
        "sudo docker system prune -af",
        "env DOCKER_HOST=lab docker system prune -af",
        "echo $(docker system prune -af)",
        "bash script.sh",
        "./script.sh",
        "source script.sh",
    ] {
        assert!(
            matches!(
                policy(command, Yolo, &context, &rules).source,
                DecisionSource::UserDeny(_)
            ),
            "{command}"
        );
    }
}

#[test]
fn non_content_file_changes_and_ordinary_network_diagnostics_are_automatic() {
    let (dir, context) = fixture();
    std::fs::write(dir.path().join("old.txt"), "original").unwrap();
    std::fs::write(dir.path().join("existing.txt"), "keep").unwrap();
    for command in [
        "mv old.txt new.txt",
        "mv old.txt existing.txt",
        "cp old.txt new.txt",
        "cp old.txt existing.txt",
        "mkdir new-dir",
        "touch fresh.txt",
        "export NOTE=hello",
        "ping router.local",
        "ping -c 4 router.local",
        "dig router.local",
    ] {
        let decision = policy(command, Auto, &context, &UserRules::default());
        assert_eq!(
            decision.decision,
            Decision::Allow,
            "{command}: {decision:?}"
        );
    }
    for command in [
        "mv *.txt target",
        "ping -f router.local",
        "ping -b 255.255.255.255",
        "export PATH=/unknown",
        "source unknown.sh",
    ] {
        assert!(
            matches!(
                policy(command, Auto, &context, &UserRules::default()).decision,
                Decision::Ask { .. }
            ),
            "{command}"
        );
    }
    assert_eq!(
        std::fs::read_to_string(dir.path().join("old.txt")).unwrap(),
        "original"
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("existing.txt")).unwrap(),
        "keep"
    );
}

#[test]
fn normal_and_protected_read_tools_obey_user_rules() {
    let (dir, mut context) = fixture();
    let path = std::fs::canonicalize(dir.path()).unwrap().join("notes.md");
    std::fs::write(&path, "fixture").unwrap();
    let rule = UserRule::compile(
        RuleSpec {
            tool: Some("read_file".into()),
            path: Some("*.md".into()),
            ..RuleSpec::default()
        },
        "markdown files",
    )
    .unwrap();
    let report = assess_read("read_file", &path, None, &context);
    for mode in [Confirm, Auto, Yolo] {
        let result = evaluate(
            &report,
            mode,
            &UserRules {
                allow: vec![rule.clone()],
                deny: vec![rule.clone()],
            },
            &SessionAllowList::default(),
        );
        assert!(matches!(result.source, DecisionSource::UserDeny(_)));
    }
    context.protected.push(path.clone());
    let report = assess_read("read_file", &path, None, &context);
    assert!(report.reads_protected);
    for mode in [Confirm, Auto, Yolo] {
        let result = evaluate(
            &report,
            mode,
            &UserRules {
                allow: vec![rule.clone()],
                deny: vec![],
            },
            &SessionAllowList::default(),
        );
        assert_eq!(result.decision, Decision::Allow);
        assert!(matches!(result.source, DecisionSource::UserAllow(_)));
    }
}

#[test]
fn session_grants_do_not_combine_targets_or_cover_prohibitions() {
    let (_dir, context) = fixture();
    let mut grants = SessionAllowList::default();
    let first = assess_command("mkdir first", &context);
    assert!(grants.grant(&first));
    assert!(grants.covers(&first));
    assert!(!grants.covers(&assess_command("mkdir second", &context)));
    assert!(!grants.grant(&assess_command("rm -rf /", &context)));
    let second = assess_command("mkdir second", &context);
    assert!(grants.grant(&second));
    assert!(!grants.covers(&assess_command("mkdir first second", &context)));
}

#[test]
fn script_whitelists_cover_the_payload_but_not_siblings_or_aliases() {
    let (dir, mut context) = fixture();
    std::fs::write(dir.path().join("trusted.sh"), "#!/bin/sh\nrm -rf /\n").unwrap();
    let rules = UserRules {
        allow: vec![UserRule::exact("./trusted.sh").unwrap()],
        deny: vec![],
    };
    for mode in [Confirm, Auto, Yolo] {
        assert_eq!(
            policy("./trusted.sh", mode, &context, &rules).decision,
            Decision::Allow
        );
        assert_eq!(
            policy("./trusted.sh; pwd", mode, &context, &rules).decision,
            Decision::Allow
        );
        assert!(!matches!(
            policy("./trusted.sh; touch extra", mode, &context, &rules).source,
            DecisionSource::UserAllow(_)
        ));
    }
    context.aliases.insert("ls".into(), "rm -rf /".into());
    let rules = UserRules {
        allow: vec![UserRule::prefix("ls").unwrap()],
        deny: vec![],
    };
    assert!(matches!(
        policy("ls", Yolo, &context, &rules).decision,
        Decision::Deny { .. }
    ));
}

#[test]
fn actual_variable_values_and_extra_environment_cannot_bypass_policy() {
    let (_dir, mut context) = fixture();
    context.variables.insert("CMD".into(), "rm".into());
    context.variables.insert("ROOT".into(), "/".into());
    assert!(matches!(
        policy("$CMD -rf \"$ROOT\"", Yolo, &context, &UserRules::default()).decision,
        Decision::Deny { .. }
    ));
    for command in [
        "NOTE=1 cargo run",
        "NOTE=1 curl https://example.com",
        "declare -r NOTE=1",
    ] {
        assert!(
            matches!(
                policy(command, Auto, &context, &UserRules::default()).decision,
                Decision::Ask { .. }
            ),
            "{command}"
        );
    }
    context.unknown_variables.insert("ARRAY".into());
    assert!(matches!(
        policy("ARRAY=1", Auto, &context, &UserRules::default()).decision,
        Decision::Ask { .. }
    ));
    let rules = UserRules {
        allow: vec![],
        deny: vec![UserRule::prefix("git push").unwrap()],
    };
    context.unknown_variables.insert("UNKNOWN".into());
    assert!(matches!(
        policy("git \"$UNKNOWN\"", Yolo, &context, &rules).source,
        DecisionSource::UserDeny(_)
    ));
}

#[test]
fn project_code_uncertainty_is_not_confused_with_explicit_high_risk() {
    let (dir, context) = fixture();
    std::fs::write(
        dir.path().join("gradlew"),
        "#!/bin/sh\n\"$UNKNOWN_PROGRAM\" test\n",
    )
    .unwrap();
    let decision = policy("./gradlew test", Auto, &context, &UserRules::default());
    assert_eq!(decision.decision, Decision::Allow, "{decision:?}");
    std::fs::write(dir.path().join("gradlew"), "#!/bin/sh\nrm -rf /\n").unwrap();
    assert!(matches!(
        policy("./gradlew test", Auto, &context, &UserRules::default()).decision,
        Decision::Deny { .. }
    ));
    for command in [
        "cargo build --target-dir /etc",
        "npm run build --prefix /etc",
        "cmake --build /etc",
    ] {
        assert_eq!(
            policy(command, Auto, &context, &UserRules::default()).decision,
            Decision::Ask { strong: true },
            "{command}"
        );
    }
}

#[test]
fn read_denies_cover_descendants_and_ordinary_renames_are_automatic() {
    let (dir, context) = fixture();
    let rule = UserRule::compile(
        RuleSpec {
            tool: Some("grep".into()),
            path: Some("private/**".into()),
            ..RuleSpec::default()
        },
        "private metadata",
    )
    .unwrap();
    let report = assess_read("grep", &context.cwd.join("private/file"), None, &context);
    let result = evaluate(
        &report,
        Yolo,
        &UserRules {
            allow: vec![],
            deny: vec![rule],
        },
        &SessionAllowList::default(),
    );
    assert!(matches!(result.source, DecisionSource::UserDeny(_)));
    let file = std::fs::File::create(dir.path().join("large")).unwrap();
    file.set_len(2 * 1024 * 1024).unwrap();
    assert_eq!(
        policy("mv large renamed", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
    std::fs::write(dir.path().join("second"), "two").unwrap();
    assert_eq!(
        policy(
            "mv large second child",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Allow
    );
    assert_eq!(
        policy("mkdir -p created", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
}

#[test]
fn git_auto_admission_uses_real_index_objects_and_rejects_hooks() {
    let (dir, mut context) = fixture();
    // The probe must use the same Git as setup, not macOS's /usr/bin/git shim.
    context
        .variables
        .insert("PATH".into(), std::env::var("PATH").unwrap());
    context.exported.insert("PATH".into());
    let system_config = tempfile::NamedTempFile::new().unwrap();
    std::fs::write(
        system_config.path(),
        "[filter \"lfs\"]\n\tclean = git-lfs clean -- %f\n\tprocess = git-lfs filter-process\n",
    )
    .unwrap();
    context.execution_variables.insert(
        "GIT_CONFIG_SYSTEM".into(),
        Some(system_config.path().to_string_lossy().into_owned()),
    );
    // Both fixture commands and policy probes must ignore host configuration.
    for (name, value) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
    ] {
        context
            .execution_variables
            .insert(name.into(), Some(value.into()));
    }
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir.path())
            .envs(
                context
                    .execution_variables
                    .iter()
                    .filter_map(|(name, value)| value.as_ref().map(|value| (name, value))),
            )
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    let assert_allowed = |command: &str| {
        let result = policy(command, Auto, &context, &UserRules::default());
        assert_eq!(
            result.decision,
            Decision::Allow,
            "{command}: {:?}",
            result.source
        );
    };
    git(&["init", "--quiet"]);
    std::fs::write(dir.path().join("file.txt"), "initial").unwrap();
    git(&["add", "file.txt"]);
    git(&[
        "-c",
        "user.name=Tests",
        "-c",
        "user.email=tests@example.invalid",
        "commit",
        "--quiet",
        "-m",
        "Fixture\n\nCo-authored-by: Copilot App <223556219+Copilot@users.noreply.github.com>",
    ]);
    std::fs::write(dir.path().join("file.txt"), "updated").unwrap();
    let mut inherited_system_config = context.clone();
    inherited_system_config
        .execution_variables
        .insert("GIT_CONFIG_NOSYSTEM".into(), Some("0".into()));
    let result = policy(
        "git add file.txt",
        Auto,
        &inherited_system_config,
        &UserRules::default(),
    );
    assert_eq!(
        result.decision,
        Decision::Ask { strong: false },
        "{:?}",
        result.source
    );
    assert!(
        result.source.label().contains("filters"),
        "{:?}",
        result.source
    );
    assert_allowed("git add file.txt");
    git(&["add", "file.txt"]);
    assert_allowed("git restore --staged file.txt");
    assert_allowed("git switch -c feature");
    assert_allowed("git commit -m message");
    git(&["config", "core.hooksPath", "custom-hooks"]);
    assert!(matches!(
        policy("git add file.txt", Auto, &context, &UserRules::default()).decision,
        Decision::Ask { .. }
    ));
    git(&["config", "--unset", "core.hooksPath"]);
    git(&[
        "remote",
        "add",
        "origin",
        "https://example.invalid/project.git",
    ]);
    assert_allowed("git fetch origin");
    assert_allowed("git ls-remote origin");
    for key in [
        "credential.helper",
        "credential.https://example.invalid.helper",
        "core.askPass",
    ] {
        git(&["config", key, "!printf helper"]);
        let result = policy("git fetch origin", Auto, &context, &UserRules::default());
        assert!(
            matches!(result.decision, Decision::Ask { .. }),
            "{key}: {result:?}"
        );
        assert!(
            result.source.label().contains("executable configuration"),
            "{key}: {result:?}"
        );
        git(&["config", "--unset", key]);
    }
    git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/*:refs/heads/*",
    ]);
    let result = policy("git fetch origin", Auto, &context, &UserRules::default());
    assert!(matches!(result.decision, Decision::Ask { .. }));
    assert!(result.source.label().contains("refspec"), "{result:?}");
    git(&[
        "config",
        "remote.origin.fetch",
        "+refs/heads/*:refs/remotes/origin/*",
    ]);
    git(&["tag", "v1"]);
    let result = policy("git fetch origin", Auto, &context, &UserRules::default());
    assert_eq!(result.decision, Decision::Allow, "{result:?}");
    assert!(result.source.label().contains("refs/tags/v1"), "{result:?}");
    for key in [
        "fetch.prune",
        "fetch.pruneTags",
        "remote.origin.prune",
        "remote.origin.pruneTags",
        "remote.origin.mirror",
    ] {
        git(&["config", key, "true"]);
        let result = policy("git fetch origin", Auto, &context, &UserRules::default());
        assert!(
            matches!(result.decision, Decision::Ask { .. }),
            "{key}: {result:?}"
        );
        git(&["config", "--unset", key]);
    }
    git(&["config", "remote.origin.tagOpt", "--no-tags"]);
    assert_allowed("git fetch origin");
    let mut askpass = context.clone();
    askpass
        .variables
        .insert("SSH_ASKPASS".into(), "/fixture/askpass".into());
    askpass.exported.insert("SSH_ASKPASS".into());
    let result = policy("git fetch origin", Auto, &askpass, &UserRules::default());
    assert!(
        matches!(result.decision, Decision::Ask { .. }),
        "{result:?}"
    );
    assert!(result.source.label().contains("SSH_ASKPASS"), "{result:?}");
    assert_eq!(
        policy("git add file.txt", Auto, &askpass, &UserRules::default()).decision,
        Decision::Allow
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("file.txt")).unwrap(),
        "updated"
    );
}

#[test]
fn home_prohibitions_survive_workspace_and_home_changes() {
    let (dir, mut context) = fixture();
    let home = std::fs::canonicalize(dir.path()).unwrap();
    context = context.with_home(&home);
    assert!(matches!(
        policy("rm -rf ~", Yolo, &context, &UserRules::default()).decision,
        Decision::Deny { .. }
    ));
    context.home = Some(home.join("child"));
    context.variables.insert(
        "HOME".into(),
        home.join("child").to_string_lossy().into_owned(),
    );
    let command = format!("rm -rf '{}'", home.display());
    assert!(matches!(
        policy(&command, Yolo, &context, &UserRules::default()).decision,
        Decision::Deny { .. }
    ));
    context
        .variables
        .insert("TARGET".into(), home.to_string_lossy().into_owned());
    context.readonly_variables.insert("TARGET".into());
    assert!(matches!(
        policy(
            "TARGET=child; rm -rf \"$TARGET\"",
            Yolo,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Deny { .. }
    ));
}

#[test]
fn physical_shell_paths_are_checked_before_lexical_dotdot_collapses() {
    let (_dir, mut context) = fixture();
    let home = context.home.clone().unwrap();
    std::fs::create_dir_all(home.join(".ssh/child")).unwrap();
    std::fs::write(home.join(".ssh/key"), "fixture only").unwrap();
    std::os::unix::fs::symlink(home.join(".ssh/child"), context.cwd.join("link")).unwrap();
    let report = assess_command("cat link/../key", &context);
    assert!(report.reads_protected, "{:?}", report.findings);
    context
        .variables
        .insert("HOME".into(), context.cwd.to_string_lossy().into_owned());
    let report = assess_command("HOME=/etc; cat \"$HOME/passwd\"", &context);
    assert!(report.reads_protected, "{:?}", report.findings);
}

#[test]
fn canonical_targets_keep_the_aliased_workspace_scope() {
    let (dir, original) = fixture();
    let alias = dir.path().join("alias");
    std::os::unix::fs::symlink(&original.workspace, &alias).unwrap();
    let context = Context::new(&alias, &alias).with_home(original.home.unwrap());
    let target = original.workspace.join("new-file");
    assert_eq!(
        nosh_permissions::classify_path(&target, &context),
        nosh_permissions::PathClass::Workspace
    );
    assert_eq!(
        policy("touch new-file", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
    let rule = UserRule::compile(
        RuleSpec {
            command_prefix: Some("touch".into()),
            write_paths: vec!["new-file".into()],
            ..RuleSpec::default()
        },
        "workspace file",
    )
    .unwrap();
    assert_eq!(
        policy(
            "touch new-file",
            Confirm,
            &context,
            &UserRules {
                allow: vec![rule.clone()],
                deny: vec![],
            }
        )
        .decision,
        Decision::Allow
    );
    let outside = tempfile::tempdir().unwrap();
    std::os::unix::fs::symlink(outside.path(), original.workspace.join("escape")).unwrap();
    assert!(matches!(
        policy("touch escape/file", Auto, &context, &UserRules::default()).decision,
        Decision::Ask { .. }
    ));
    std::os::unix::fs::symlink(outside.path().join("target"), target).unwrap();
    assert!(!matches!(
        policy(
            "touch new-file",
            Confirm,
            &context,
            &UserRules {
                allow: vec![rule],
                deny: vec![],
            }
        )
        .source,
        DecisionSource::UserAllow(_)
    ));
}

#[test]
fn automatic_categories_do_not_cover_extra_effects() {
    let (dir, context) = fixture();
    std::fs::write(dir.path().join("existing"), "keep").unwrap();
    let failures: Vec<_> = [
        "ping -c 1 router.local > existing",
        "dig router.local > existing",
        "gradle build publish",
        "mvn test deploy",
        "cmake --build build --target install",
    ]
    .into_iter()
    .filter_map(|command| {
        let result = policy(command, Auto, &context, &UserRules::default());
        (!matches!(result.decision, Decision::Ask { .. })).then(|| format!("{command}: {result:?}"))
    })
    .collect();
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    assert_eq!(
        std::fs::read_to_string(dir.path().join("existing")).unwrap(),
        "keep"
    );
}

#[test]
fn build_disclosure_survives_an_earlier_recoverable_operation() {
    let (_dir, context) = fixture();
    let result = policy(
        "cd child && cargo test",
        Auto,
        &context,
        &UserRules::default(),
    );
    assert_eq!(result.decision, Decision::Allow);
    assert!(
        result.source.label().contains("not guaranteed recoverable"),
        "{result:?}"
    );
}

#[test]
fn payload_permission_cannot_skip_an_unapproved_intermediate_operation() {
    let (dir, context) = fixture();
    std::fs::write(
        dir.path().join("trusted.sh"),
        "bash -c 'printf hi > blocked'\n",
    )
    .unwrap();
    let rule = UserRule::compile(
        RuleSpec {
            command_exact: Some("./trusted.sh".into()),
            write_paths: vec!["allowed".into()],
            ..RuleSpec::default()
        },
        "scoped script",
    )
    .unwrap();
    let result = policy(
        "./trusted.sh",
        Confirm,
        &context,
        &UserRules {
            allow: vec![rule],
            deny: vec![],
        },
    );
    assert!(
        !matches!(result.source, DecisionSource::UserAllow(_)),
        "{result:?}"
    );
}

#[test]
fn quoted_noninteractive_flag_is_not_a_sudo_option_value() {
    let (_dir, context) = fixture();
    let rules = UserRules {
        allow: vec![UserRule::exact("sudo -u printf x").unwrap()],
        deny: vec![],
    };
    let result = policy("sudo -u -n printf x", Confirm, &context, &rules);
    assert!(
        !matches!(result.source, DecisionSource::UserAllow(_)),
        "{result:?}"
    );
}

#[test]
fn routine_move_copy_sequences_do_not_require_recovery_proofs() {
    let (dir, context) = fixture();
    for file in ["first", "second"] {
        std::fs::write(dir.path().join(file), file).unwrap();
    }
    for command in [
        "mv first destination; mv second destination",
        "mv first child; mv second child/first",
        "cargo test; mv second destination",
    ] {
        let result = policy(command, Auto, &context, &UserRules::default());
        assert_eq!(result.decision, Decision::Allow, "{command}: {result:?}");
    }
    assert_eq!(
        policy(
            "mv first new-first; mv second new-second",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Allow
    );
    let rules = UserRules {
        allow: vec![UserRule::prefix("mv").unwrap()],
        deny: vec![],
    };
    assert_eq!(
        policy(
            "mv first destination; mv second destination",
            Auto,
            &context,
            &rules
        )
        .decision,
        Decision::Allow,
        "auto-admission evidence must not add approval to a complete user rule"
    );
}

#[test]
fn session_grants_require_known_paths_and_working_directory() {
    let (_dir, context) = fixture();
    let mut report = assess_command("touch file", &context);
    assert!(SessionAllowList::can_grant(&report));
    report.operations[0].cwd_known = false;
    assert!(!SessionAllowList::can_grant(&report));
    report.operations[0].cwd_known = true;
    report.operations[0].paths[0].resolved = None;
    let mut grants = SessionAllowList::default();
    assert!(!grants.grant(&report));
    assert!(grants.is_empty());
}

#[test]
fn scoped_denies_follow_script_effects_without_covering_siblings() {
    let (dir, context) = fixture();
    let rule = UserRule::compile(
        RuleSpec {
            command_exact: Some("./trusted.sh".into()),
            write_paths: vec!["blocked".into()],
            ..RuleSpec::default()
        },
        "script cannot write blocked",
    )
    .unwrap();
    let rules = UserRules {
        allow: vec![],
        deny: vec![rule],
    };
    std::fs::write(
        dir.path().join("trusted.sh"),
        "bash -c 'printf hi > blocked'\n",
    )
    .unwrap();
    for mode in [Confirm, Auto, Yolo] {
        let result = policy("./trusted.sh", mode, &context, &rules);
        assert!(
            matches!(result.source, DecisionSource::UserDeny(_)),
            "{result:?}"
        );
    }
    std::fs::write(dir.path().join("trusted.sh"), "printf hi > allowed\n").unwrap();
    let result = policy(
        "./trusted.sh; printf sibling > blocked",
        Yolo,
        &context,
        &rules,
    );
    assert_eq!(result.decision, Decision::Allow, "{result:?}");
}

#[test]
fn attached_path_options_keep_their_actual_targets() {
    let (_dir, context) = fixture();
    for (plain, attached) in [
        ("mv -t /etc file", "mv -t/etc file"),
        (
            "mv --target-directory /etc file",
            "mv --target-directory=/etc file",
        ),
        ("cp -t /etc file", "cp -t/etc file"),
        (
            "cp --target-directory /etc file",
            "cp --target-directory=/etc file",
        ),
        ("npm run build --prefix /etc", "npm run build --prefix=/etc"),
        ("pnpm build -C /etc", "pnpm build -C/etc"),
        ("yarn build --cwd /etc", "yarn build --cwd=/etc"),
        (
            "cargo test --manifest-path /etc/Cargo.toml",
            "cargo test --manifest-path=/etc/Cargo.toml",
        ),
        (
            "cargo build --target-dir /etc",
            "cargo build --target-dir=/etc",
        ),
        (
            "mvn test --file /etc/pom.xml",
            "mvn test --file=/etc/pom.xml",
        ),
        ("mvn test -f /etc/pom.xml", "mvn test -f/etc/pom.xml"),
        ("cmake --build /etc", "cmake --build=/etc"),
        ("go build -o /etc/file", "go build -o/etc/file"),
        ("tsc --outDir /etc", "tsc --outDir=/etc"),
        (
            "tsc --project /etc/tsconfig.json",
            "tsc --project=/etc/tsconfig.json",
        ),
        (
            "touch -r /etc/reference dest",
            "touch -r/etc/reference dest",
        ),
        (
            "touch --reference /etc/reference dest",
            "touch --reference=/etc/reference dest",
        ),
    ] {
        let plain_report = assess_command(plain, &context);
        let attached_report = assess_command(attached, &context);
        let targets = |report: &nosh_permissions::RiskReport| {
            report
                .operations
                .iter()
                .flat_map(|op| &op.paths)
                .map(|path| (path.kind, path.resolved.clone()))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            targets(&attached_report),
            targets(&plain_report),
            "{attached}"
        );
        assert!(
            matches!(
                policy(attached, Auto, &context, &UserRules::default()).decision,
                Decision::Ask { .. }
            ),
            "{attached}"
        );
    }
    let rules = UserRules {
        allow: vec![
            UserRule::compile(
                RuleSpec {
                    command_prefix: Some("mv".into()),
                    write_paths: vec!["**".into()],
                    ..RuleSpec::default()
                },
                "workspace moves",
            )
            .unwrap(),
        ],
        deny: vec![],
    };
    assert!(!matches!(
        policy("mv -t/etc file", Yolo, &context, &rules).source,
        DecisionSource::UserAllow(_)
    ));
}

#[test]
fn routine_moves_and_copies_are_not_gated_on_atomic_execution() {
    let (dir, context) = fixture();
    std::fs::write(dir.path().join("source"), "keep").unwrap();
    for command in [
        "mv source dest",
        "mv -n source dest",
        "cp source dest",
        "cp -r source dest",
    ] {
        assert_eq!(
            policy(command, Auto, &context, &UserRules::default()).decision,
            Decision::Allow,
            "{command}"
        );
        assert_eq!(
            policy(command, Confirm, &context, &UserRules::default()).decision,
            Decision::Ask { strong: false },
            "{command}"
        );
    }
    for command in ["printf new > dest", "echo new >> dest"] {
        assert!(
            matches!(
                policy(command, Auto, &context, &UserRules::default()).decision,
                Decision::Ask { .. }
            ),
            "{command}"
        );
        assert_eq!(
            policy(command, Yolo, &context, &UserRules::default()).decision,
            Decision::Allow
        );
    }
    let rules = UserRules {
        allow: vec![UserRule::prefix("mv").unwrap()],
        deny: vec![],
    };
    assert_eq!(
        policy("mv source dest", Auto, &context, &rules).decision,
        Decision::Allow
    );
    assert_eq!(
        std::fs::read_to_string(dir.path().join("source")).unwrap(),
        "keep"
    );
    assert!(!dir.path().join("dest").exists());
}

#[test]
fn touch_reference_is_an_independent_read_effect() {
    let (dir, context) = fixture();
    std::fs::write(dir.path().join("reference"), "fixture").unwrap();
    let rule = UserRule::compile(
        RuleSpec {
            command_prefix: Some("touch".into()),
            read_paths: vec!["reference".into()],
            ..RuleSpec::default()
        },
        "reference deny",
    )
    .unwrap();
    for mode in [Confirm, Auto, Yolo] {
        for command in [
            "touch -r reference dest",
            "touch -rreference dest",
            "touch --reference=reference dest",
        ] {
            let result = policy(
                command,
                mode,
                &context,
                &UserRules {
                    allow: vec![],
                    deny: vec![rule.clone()],
                },
            );
            assert!(
                matches!(result.source, DecisionSource::UserDeny(_)),
                "{command}: {result:?}"
            );
        }
    }
    assert!(!dir.path().join("dest").exists());
}

#[test]
fn abbreviated_path_options_are_not_treated_as_read_only() {
    let (_dir, context) = fixture();
    for (full, abbreviated) in [
        (
            "sort --output=/etc/file input",
            "sort --out=/etc/file input",
        ),
        (
            "sort --output /etc/file input",
            "sort --out /etc/file input",
        ),
        (
            "cp --target-directory=/etc input",
            "cp --target-dir=/etc input",
        ),
        (
            "curl --output=/etc/file https://example.invalid",
            "curl --out=/etc/file https://example.invalid",
        ),
    ] {
        let report = assess_command(abbreviated, &context);
        let expected = assess_command(full, &context);
        let paths = |report: &nosh_permissions::RiskReport| {
            report
                .operations
                .iter()
                .flat_map(|op| &op.paths)
                .cloned()
                .collect::<Vec<_>>()
        };
        assert_eq!(paths(&report), paths(&expected), "{abbreviated}");
        assert!(
            matches!(
                policy(abbreviated, Auto, &context, &UserRules::default()).decision,
                Decision::Ask { .. }
            ),
            "{abbreviated}"
        );
        let rule = UserRule::compile(
            RuleSpec {
                tool: Some("run_command".into()),
                write_paths: vec!["/etc/**".into()],
                ..RuleSpec::default()
            },
            "system writes",
        )
        .unwrap();
        assert!(matches!(
            policy(
                abbreviated,
                Yolo,
                &context,
                &UserRules {
                    allow: vec![],
                    deny: vec![rule]
                }
            )
            .source,
            DecisionSource::UserDeny(_)
        ));
    }
}

#[test]
fn absolute_path_scopes_follow_root_aliases_but_not_escaping_children() {
    let (dir, context) = fixture();
    let actual = context.cwd.join("real");
    std::fs::create_dir(&actual).unwrap();
    std::fs::write(actual.join("file"), "fixture").unwrap();
    let alias = context.cwd.join("alias");
    std::os::unix::fs::symlink(&actual, &alias).unwrap();
    let outside = tempfile::tempdir().unwrap();
    std::fs::write(outside.path().join("file"), "outside").unwrap();
    std::os::unix::fs::symlink(outside.path(), actual.join("escape")).unwrap();
    for pattern in [
        format!("{}/**", alias.display()),
        alias.join("file").to_string_lossy().into_owned(),
    ] {
        let rule = UserRule::compile(
            RuleSpec {
                tool: Some("read_file".into()),
                path: Some(pattern),
                ..RuleSpec::default()
            },
            "aliased scope",
        )
        .unwrap();
        for path in [actual.join("file"), alias.join("file")] {
            let report = assess_read("read_file", &path, None, &context);
            let allow = evaluate(
                &report,
                Confirm,
                &UserRules {
                    allow: vec![rule.clone()],
                    deny: vec![],
                },
                &SessionAllowList::default(),
            );
            assert!(
                matches!(allow.source, DecisionSource::UserAllow(_)),
                "{allow:?}"
            );
            for mode in [Confirm, Auto, Yolo] {
                let deny = evaluate(
                    &report,
                    mode,
                    &UserRules {
                        allow: vec![rule.clone()],
                        deny: vec![rule.clone()],
                    },
                    &SessionAllowList::default(),
                );
                assert!(
                    matches!(deny.source, DecisionSource::UserDeny(_)),
                    "{deny:?}"
                );
            }
        }
        let report = assess_read("read_file", &alias.join("escape/file"), None, &context);
        let result = evaluate(
            &report,
            Confirm,
            &UserRules {
                allow: vec![rule],
                deny: vec![],
            },
            &SessionAllowList::default(),
        );
        assert!(
            !matches!(result.source, DecisionSource::UserAllow(_)),
            "{result:?}"
        );
    }
    assert!(dir.path().exists());
}

#[test]
fn known_tool_basenames_do_not_trust_arbitrary_path_programs() {
    let (_dir, context) = fixture();
    for (name, args) in [
        ("cargo", "test"),
        ("git", "fetch"),
        ("ping", "router.local"),
        ("mv", "a b"),
        ("cp", "a b"),
        ("ls", ""),
    ] {
        let executable = context.cwd.join(name);
        std::fs::write(&executable, b"\0opaque executable fixture").unwrap();
        for program in [format!("./{name}"), executable.display().to_string()] {
            let command = format!("{program} {args}");
            let result = policy(&command, Auto, &context, &UserRules::default());
            assert!(
                matches!(result.decision, Decision::Ask { .. }),
                "{command}: {result:?}"
            );
        }
    }
    for command in ["/usr/bin/cargo test", "/bin/cp a b", "/bin/mv a b"] {
        assert_eq!(
            policy(command, Auto, &context, &UserRules::default()).decision,
            Decision::Allow,
            "{command}"
        );
    }
}

#[test]
fn unknown_development_options_do_not_hide_fix_modes() {
    let (_dir, mut context) = fixture();
    context.unknown_variables.insert("MODE".into());
    for command in [
        "cargo clippy \"$MODE\"",
        "eslint \"$MODE\" .",
        "cargo test \"$MODE\"",
    ] {
        let result = policy(command, Auto, &context, &UserRules::default());
        assert!(
            matches!(result.decision, Decision::Ask { .. }),
            "{command}: {result:?}"
        );
    }
    context.unknown_variables.remove("MODE");
    context
        .variables
        .insert("MODE".into(), "--workspace".into());
    assert_eq!(
        policy(
            "cargo test \"$MODE\"",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Allow
    );
    context.variables.insert("MODE".into(), "--fix".into());
    assert!(matches!(
        policy(
            "cargo clippy \"$MODE\"",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Ask { .. }
    ));
}

#[test]
fn short_option_clusters_do_not_hide_path_effects() {
    let (_dir, context) = fixture();
    for command in [
        "sort -ro/etc/out input",
        "sort -ro /etc/out input",
        "curl -sSo/etc/out https://example.invalid",
        "mv -vt/etc input",
        "cp -vt/etc input",
        "touch -ar/etc/reference dest",
    ] {
        let result = policy(command, Auto, &context, &UserRules::default());
        assert!(
            matches!(result.decision, Decision::Ask { .. }),
            "{command}: {result:?}"
        );
    }
    assert_eq!(
        policy("sort -k1,2ro input", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
    let report = assess_command("curl -sSo/etc/out https://example.invalid", &context);
    assert!(
        report
            .operations
            .iter()
            .flat_map(|op| &op.paths)
            .any(|path| { path.lexical.as_deref() == Some(std::path::Path::new("/etc/out")) })
    );
}

#[test]
fn convenience_does_not_require_splitting_every_post_build_read() {
    let (_dir, context) = fixture();
    std::fs::write(context.cwd.join("result.log"), "fixture").unwrap();
    let result = policy(
        "cargo test; cat result.log",
        Auto,
        &context,
        &UserRules::default(),
    );
    assert_eq!(result.decision, Decision::Allow, "{result:?}");
    assert!(result.source.label().contains("not guaranteed recoverable"));
    assert!(matches!(
        policy(
            "cargo test; cat ~/.ssh/key",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Ask { .. }
    ));
}

#[test]
fn routine_file_convenience_does_not_override_destructive_effects_or_denies() {
    let (_dir, context) = fixture();
    for command in [
        "mv a /dev/null",
        "cp a /etc/file",
        "mv a /etc/file",
        "cp a ../../outside",
        "mv a b; rm -rf child",
    ] {
        assert_eq!(
            policy(command, Auto, &context, &UserRules::default()).decision,
            Decision::Ask { strong: true },
            "{command}"
        );
    }
    for name in ["mv", "cp"] {
        let rule = UserRule::prefix(name).unwrap();
        let result = policy(
            &format!("{name} a b"),
            Auto,
            &context,
            &UserRules {
                allow: vec![rule.clone()],
                deny: vec![rule],
            },
        );
        assert!(matches!(result.source, DecisionSource::UserDeny(_)));
    }
}

#[test]
fn wget_clusters_preserve_scoped_denies_in_every_mode() {
    let (_dir, context) = fixture();
    let rule = UserRule::compile(
        RuleSpec {
            command_prefix: Some("wget".into()),
            write_paths: vec!["/etc/**".into()],
            ..RuleSpec::default()
        },
        "system output deny",
    )
    .unwrap();
    for command in [
        "wget -qO/etc/out https://example.invalid",
        "wget -qO /etc/out https://example.invalid",
        "wget -qP/etc https://example.invalid",
        "wget -qP /etc https://example.invalid",
    ] {
        let report = assess_command(command, &context);
        assert!(
            report
                .operations
                .iter()
                .flat_map(|op| &op.paths)
                .any(|path| {
                    path.lexical
                        .as_ref()
                        .is_some_and(|path| path.starts_with("/etc"))
                }),
            "{command}: {:?}",
            report.operations
        );
        for mode in [Confirm, Auto, Yolo] {
            let result = policy(
                command,
                mode,
                &context,
                &UserRules {
                    allow: vec![UserRule::prefix("wget").unwrap()],
                    deny: vec![rule.clone()],
                },
            );
            assert!(
                matches!(result.source, DecisionSource::UserDeny(_)),
                "{command}: {result:?}"
            );
        }
    }
    for command in [
        "wget -qUagentO/etc/out https://example.invalid",
        "wget -qU -O/etc/out https://example.invalid",
    ] {
        let report = assess_command(command, &context);
        assert!(
            report.operations.iter().all(|op| op.paths.is_empty()),
            "{command}: {:?}",
            report.operations
        );
    }
}

#[test]
fn redundant_relative_separators_never_grant_absolute_scope() {
    let (_dir, mut context) = fixture();
    let outside = tempfile::tempdir().unwrap();
    let outside = std::fs::canonicalize(outside.path())
        .unwrap()
        .join("protected");
    context.protected.push(outside.clone());
    for pattern in [".//**", ".///**", "././/**"] {
        let rule = UserRule::compile(
            RuleSpec {
                tool: Some("read_file".into()),
                path: Some(pattern.into()),
                ..RuleSpec::default()
            },
            "workspace reads",
        )
        .unwrap();
        for (path, inside) in [
            (context.workspace.join("file"), true),
            (outside.clone(), false),
        ] {
            let report = assess_read("read_file", &path, None, &context);
            let result = evaluate(
                &report,
                Confirm,
                &UserRules {
                    allow: vec![rule.clone()],
                    deny: vec![],
                },
                &SessionAllowList::default(),
            );
            assert_eq!(
                matches!(result.source, DecisionSource::UserAllow(_)),
                inside,
                "{pattern}: {result:?}"
            );
            let result = evaluate(
                &report,
                Yolo,
                &UserRules {
                    allow: vec![],
                    deny: vec![rule.clone()],
                },
                &SessionAllowList::default(),
            );
            assert_eq!(
                matches!(result.source, DecisionSource::UserDeny(_)),
                inside,
                "{pattern}: {result:?}"
            );
        }
    }
}

#[test]
fn config_driven_builds_remain_automatic_but_obey_resource_denies() {
    let (_dir, context) = fixture();
    let rule = UserRule::compile(
        RuleSpec {
            tool: Some("run_command".into()),
            write_paths: vec!["private/**".into()],
            ..RuleSpec::default()
        },
        "private output deny",
    )
    .unwrap();
    for command in [
        "tsc",
        "eslint .",
        "meson compile build",
        "bazel build //app:bin",
        "sbt test",
        "cargo test",
    ] {
        let report = assess_command(command, &context);
        assert!(report.operations.iter().any(|op| op.opaque), "{command}");
        assert_eq!(
            evaluate(
                &report,
                Auto,
                &UserRules::default(),
                &SessionAllowList::default()
            )
            .decision,
            Decision::Allow,
            "{command}"
        );
        for mode in [Confirm, Auto, Yolo] {
            let result = evaluate(
                &report,
                mode,
                &UserRules {
                    allow: vec![],
                    deny: vec![rule.clone()],
                },
                &SessionAllowList::default(),
            );
            assert!(
                matches!(result.source, DecisionSource::UserDeny(_)),
                "{command}: {result:?}"
            );
        }
    }
    assert!(
        !assess_command("tsc --version", &context)
            .operations
            .iter()
            .any(|op| op.opaque)
    );
}

#[test]
fn sort_output_does_not_hide_input_reads_or_invent_option_input_files() {
    let (_dir, context) = fixture();
    let rule = UserRule::compile(
        RuleSpec {
            command_prefix: Some("sort".into()),
            read_paths: vec!["/etc/**".into()],
            ..RuleSpec::default()
        },
        "protected sort input",
    )
    .unwrap();
    for command in [
        "sort /etc/passwd -o out",
        "sort -ro out /etc/passwd",
        "sort --out=out --key 1,2 /etc/passwd",
        "sort -k1,2 -T /tmp -t : --buffer-size 1M /etc/passwd -o out",
    ] {
        let report = assess_command(command, &context);
        assert!(report.reads_protected, "{command}");
        let reads: Vec<_> = report
            .operations
            .iter()
            .flat_map(|op| &op.paths)
            .filter(|path| path.kind == nosh_permissions::AccessKind::Read)
            .map(|path| path.lexical.clone())
            .collect();
        assert_eq!(reads, [Some("/etc/passwd".into())], "{command}");
        for mode in [Confirm, Auto, Yolo] {
            let result = policy(
                command,
                mode,
                &context,
                &UserRules {
                    allow: vec![UserRule::prefix("sort").unwrap()],
                    deny: vec![rule.clone()],
                },
            );
            assert!(
                matches!(result.source, DecisionSource::UserDeny(_)),
                "{command}: {result:?}"
            );
        }
    }
    let report = assess_command("sort input -o /etc/out -- --literal-input", &context);
    assert!(!report.reads_protected);
    assert_eq!(
        report.operations[0]
            .paths
            .iter()
            .filter(|path| path.kind == nosh_permissions::AccessKind::Read)
            .count(),
        2
    );
}

#[test]
fn convenience_categories_follow_aliases_and_functions_instead_of_their_names() {
    let (_dir, mut context) = fixture();
    context
        .aliases
        .insert("cargo".into(), "unknown_project_runner".into());
    let result = policy("cargo test", Auto, &context, &UserRules::default());
    assert!(
        matches!(result.decision, Decision::Ask { .. }),
        "{result:?}"
    );
    context.aliases.clear();
    context
        .functions
        .insert("cargo".into(), "{ unknown_project_runner; }".into());
    let result = policy("cargo test", Auto, &context, &UserRules::default());
    assert!(
        matches!(result.decision, Decision::Ask { .. }),
        "{result:?}"
    );
    context.functions.clear();
    context.aliases.insert("cargo".into(), "cargo".into());
    assert_eq!(
        policy("cargo test", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
    context
        .aliases
        .insert("cargo".into(), "/usr/bin/cargo".into());
    assert_eq!(
        policy("cargo test", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
    context.aliases.clear();
    context
        .functions
        .insert("cargo".into(), "{ /usr/bin/cargo test; }".into());
    assert_eq!(
        policy("cargo test", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
}

#[test]
fn typescript_clean_and_initialization_have_distinct_effects() {
    let (_dir, context) = fixture();
    for command in ["tsc", "tsc --build", "tsc --init", "tsc --noEmit"] {
        let result = policy(command, Auto, &context, &UserRules::default());
        assert_eq!(result.decision, Decision::Allow, "{command}: {result:?}");
    }
    for command in [
        "tsc --build --clean",
        "tsc -b --clean",
        "meson compile --clean build",
    ] {
        let result = policy(command, Auto, &context, &UserRules::default());
        assert_eq!(
            result.decision,
            Decision::Ask { strong: false },
            "{command}: {result:?}"
        );
        assert_eq!(
            policy(command, Yolo, &context, &UserRules::default()).decision,
            Decision::Allow
        );
    }
    for command in [
        "tsc --incremental --tsBuildInfoFile /etc/state.tsbuildinfo",
        "tsc --incremental --tsBuildInfoFile=/etc/state.tsbuildinfo",
        "tsc --generateTrace /etc/trace",
        "tsc --generateCpuProfile=/etc/profile.cpuprofile",
    ] {
        assert_eq!(
            policy(command, Auto, &context, &UserRules::default()).decision,
            Decision::Ask { strong: true },
            "{command}"
        );
        let rule = UserRule::compile(
            RuleSpec {
                tool: Some("run_command".into()),
                write_paths: vec!["/etc/**".into()],
                ..RuleSpec::default()
            },
            "system output deny",
        )
        .unwrap();
        let result = policy(
            command,
            Yolo,
            &context,
            &UserRules {
                allow: vec![],
                deny: vec![rule],
            },
        );
        assert!(
            matches!(result.source, DecisionSource::UserDeny(_)),
            "{result:?}"
        );
    }
}

#[test]
fn tar_extract_and_list_treat_the_archive_as_an_input() {
    let (_dir, context) = fixture();
    let rule = UserRule::compile(
        RuleSpec {
            command_prefix: Some("tar".into()),
            read_paths: vec!["/etc/**".into()],
            ..RuleSpec::default()
        },
        "protected archive input",
    )
    .unwrap();
    for command in [
        "tar -x -f /etc/archive.tar",
        "tar --extract --file=/etc/archive.tar",
        "tar -t -f /etc/archive.tar",
    ] {
        let report = assess_command(command, &context);
        assert!(report.reads_protected, "{command}");
        let result = policy(
            command,
            Yolo,
            &context,
            &UserRules {
                allow: vec![],
                deny: vec![rule.clone()],
            },
        );
        assert!(
            matches!(result.source, DecisionSource::UserDeny(_)),
            "{command}: {result:?}"
        );
    }
}

#[test]
fn tar_effects_cover_creation_inputs_and_unknown_extraction_members() {
    let (_dir, context) = fixture();
    let protected_read = UserRule::compile(
        RuleSpec {
            command_prefix: Some("tar".into()),
            read_paths: vec!["/etc/**".into()],
            ..RuleSpec::default()
        },
        "protected archive input",
    )
    .unwrap();
    let create = "tar -cf out.tar /etc/passwd";
    let report = assess_command(create, &context);
    assert!(report.reads_protected, "{report:?}");
    assert!(matches!(
        policy(
            create,
            Yolo,
            &context,
            &UserRules {
                allow: vec![],
                deny: vec![protected_read],
            },
        )
        .source,
        DecisionSource::UserDeny(_)
    ));

    let extracted_member = UserRule::compile(
        RuleSpec {
            command_prefix: Some("tar".into()),
            write_paths: vec!["private/**".into()],
            ..RuleSpec::default()
        },
        "private extraction target",
    )
    .unwrap();
    assert!(matches!(
        policy(
            "tar -xf archive.tar",
            Yolo,
            &context,
            &UserRules {
                allow: vec![],
                deny: vec![extracted_member],
            },
        )
        .source,
        DecisionSource::UserDeny(_)
    ));
}

#[test]
fn directory_changes_scope_both_pwd_variables() {
    let (_dir, context) = fixture();
    let report = assess_command("cd .", &context);
    let variables = &report.operations[0].variables;
    assert!(variables.iter().any(|(name, _)| name == "PWD"));
    assert!(variables.iter().any(|(name, _)| name == "OLDPWD"));

    let rule = |variables| {
        UserRule::compile(
            RuleSpec {
                command_prefix: Some("cd".into()),
                variables,
                ..RuleSpec::default()
            },
            "directory variables",
        )
        .unwrap()
    };
    let denied = policy(
        "cd .",
        Yolo,
        &context,
        &UserRules {
            allow: vec![],
            deny: vec![rule(vec!["OLDPWD".into()])],
        },
    );
    assert!(matches!(denied.source, DecisionSource::UserDeny(_)));
    let allowed = policy(
        "cd .",
        Auto,
        &context,
        &UserRules {
            allow: vec![rule(vec!["PWD".into()])],
            deny: vec![],
        },
    );
    assert!(!matches!(allowed.source, DecisionSource::UserAllow(_)));
    assert_eq!(allowed.decision, Decision::Allow);
}

#[test]
fn an_exact_deny_cannot_be_escaped_with_disappearing_arguments() {
    let (_dir, context) = fixture();
    let rules = UserRules {
        allow: vec![UserRule::prefix("git").unwrap()],
        deny: vec![UserRule::exact("git fetch").unwrap()],
    };
    for mode in [Confirm, Auto, Yolo] {
        for command in [
            "ARG=x; read ARG; git fetch $ARG",
            "git fetch ${ARG}",
            "git fetch $ARG $OTHER",
        ] {
            let result = policy(command, mode, &context, &rules);
            assert!(
                matches!(result.source, DecisionSource::UserDeny(_)),
                "{command}: {result:?}"
            );
        }
    }
    for command in [
        r#"git fetch "$ARG""#,
        "git fetch fixed$ARG",
        "git fetch $ARG/suffix",
        r#"git fetch ''"#,
        "git fetch $((ARG))",
    ] {
        let result = policy(command, Yolo, &context, &rules);
        assert!(
            !matches!(result.source, DecisionSource::UserDeny(_)),
            "{command}: {result:?}"
        );
    }
    let rules = UserRules {
        allow: vec![UserRule::exact("git fetch").unwrap()],
        deny: vec![],
    };
    let result = policy("git fetch $ARG", Confirm, &context, &rules);
    assert!(
        !matches!(result.source, DecisionSource::UserAllow(_)),
        "{result:?}"
    );
    let rules = UserRules {
        allow: vec![],
        deny: vec![UserRule::exact("export").unwrap()],
    };
    assert!(!matches!(
        policy("export ARG=$UNKNOWN", Yolo, &context, &rules).source,
        DecisionSource::UserDeny(_)
    ));
}
