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
        "ctest",
        "pytest -q",
        "python3 -m pytest",
        "mvn test",
        "gradle build",
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
        "npm run deploy",
        "make install",
        "python3 unknown.py",
        "some-unknown-program",
        "npx arbitrary-package",
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
fn non_overwriting_moves_and_ordinary_network_diagnostics_are_automatic() {
    let (dir, context) = fixture();
    std::fs::write(dir.path().join("old.txt"), "original").unwrap();
    std::fs::write(dir.path().join("existing.txt"), "keep").unwrap();
    for command in [
        "mv old.txt new.txt",
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
        "mv old.txt existing.txt",
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
fn directory_denies_cover_descendants_and_large_renames_need_no_byte_copy() {
    let (dir, context) = fixture();
    let rule = UserRule::compile(
        RuleSpec {
            tool: Some("list_dir".into()),
            path: Some("private/**".into()),
            ..RuleSpec::default()
        },
        "private metadata",
    )
    .unwrap();
    let report = assess_read("list_dir", &context.cwd, Some(3), &context);
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
    let (dir, context) = fixture();
    let git = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .current_dir(dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
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
    assert_eq!(
        policy("git add file.txt", Auto, &context, &UserRules::default()).decision,
        Decision::Allow
    );
    git(&["add", "file.txt"]);
    assert_eq!(
        policy(
            "git restore --staged file.txt",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Allow
    );
    assert_eq!(
        policy(
            "git switch -c feature",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Allow
    );
    assert_eq!(
        policy(
            "git commit -m message",
            Auto,
            &context,
            &UserRules::default()
        )
        .decision,
        Decision::Allow
    );
    git(&["config", "core.hooksPath", "custom-hooks"]);
    assert!(matches!(
        policy("git add file.txt", Auto, &context, &UserRules::default()).decision,
        Decision::Ask { .. }
    ));
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
