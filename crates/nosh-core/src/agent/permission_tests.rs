use super::*;
use crate::{RecordUi, Scripted};
use nosh_engine::MockChatEngine;
use nosh_permissions::{
    ApprovalMode::{Auto, Confirm, Yolo},
    Risk, UserRule,
};
use std::cell::RefCell;

thread_local! {
    static EXECUTED: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn fake_run(
    _shell: &mut EmbeddedShell,
    command: &str,
    _opts: &AgentExecOpts,
    _sink: &mut dyn nosh_shell::OutputSink,
) -> Result<nosh_shell::CommandResult, nosh_shell::ShellError> {
    EXECUTED.with(|calls| calls.borrow_mut().push(command.into()));
    Ok(nosh_shell::CommandResult::default())
}

fn fake_agent(mode: ApprovalMode, rules: UserRules) -> Agent {
    EXECUTED.with(|calls| calls.borrow_mut().clear());
    let mut agent = Agent::new(
        Box::new(MockChatEngine::new(vec![])),
        AgentConfig {
            mode,
            rules,
            ..AgentConfig::default()
        },
        Environment {
            os: "Linux".into(),
            arch: "test".into(),
            user: "test".into(),
            available: vec![],
        },
        ToolSet::Full,
    );
    agent.command_runner = fake_run;
    agent
}

fn call(command: &str) -> ToolCall {
    ToolCall {
        name: "exec".into(),
        args: serde_json::json!({ "command": command })
            .as_object()
            .unwrap()
            .clone(),
    }
}

fn count() -> usize {
    EXECUTED.with(|calls| calls.borrow().len())
}

#[test]
fn full_matrix_counts_real_dispatch_without_executing_harmful_commands() {
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    for mode in [Confirm, Auto, Yolo] {
        let allow = UserRule::prefix("rm").unwrap();
        let mut agent = fake_agent(
            mode,
            UserRules {
                allow: vec![allow.clone()],
                deny: vec![allow.clone()],
            },
        );
        let mut approvals = Scripted::new([ApprovalResponse::Approve]);
        let mut ui = RecordUi::default();
        assert!(matches!(
            agent.exec_call(&mut shell, &call("rm -rf /"), &mut approvals, &mut ui),
            Exec::Denied(_)
        ));
        assert!(approvals.seen.is_empty());
        assert_eq!(count(), 0);
        assert!(ui.events.iter().any(|e| e.contains("user deny")));

        let mut agent = fake_agent(
            mode,
            UserRules {
                allow: vec![allow],
                deny: vec![],
            },
        );
        let mut approvals = Scripted::new([]);
        assert!(matches!(
            agent.exec_call(&mut shell, &call("rm -rf /"), &mut approvals, &mut ui),
            Exec::CommandResult(_)
        ));
        assert!(approvals.seen.is_empty());
        assert_eq!(count(), 1);

        let mut agent = fake_agent(mode, UserRules::default());
        let mut approvals = Scripted::new([ApprovalResponse::Approve]);
        let result = agent.exec_call(&mut shell, &call("rm -rf /"), &mut approvals, &mut ui);
        if mode == Confirm {
            assert!(matches!(result, Exec::CommandResult(_)));
            assert_eq!(approvals.seen.len(), 1);
            assert!(approvals.seen[0].strong);
            assert!(!approvals.seen[0].can_grant);
            assert_eq!(count(), 1);
            assert!(agent.allow.is_empty());
        } else {
            assert!(matches!(result, Exec::Denied(_)));
            assert!(approvals.seen.is_empty());
            assert_eq!(count(), 0);
        }

        let mut agent = fake_agent(mode, UserRules::default());
        let mut approvals = Scripted::new([ApprovalResponse::Approve]);
        agent.exec_call(&mut shell, &call("rm -rf build"), &mut approvals, &mut ui);
        assert_eq!(count(), 1);
        assert_eq!(approvals.seen.len(), usize::from(mode != Yolo));
        assert!(approvals.seen.iter().all(|req| req.strong));
    }
}

#[test]
fn edits_and_safe_rewrites_do_not_add_approval_to_valid_whitelists() {
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    let mut agent = fake_agent(
        Confirm,
        UserRules {
            allow: vec![UserRule::prefix("rm").unwrap()],
            deny: vec![],
        },
    );
    let mut approval = Scripted::new([ApprovalResponse::Edit("rm -rf /".into())]);
    agent.exec_call(
        &mut shell,
        &call("touch unapproved"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(approval.seen.len(), 1);
    assert_eq!(count(), 1);
    EXECUTED.with(|calls| assert_eq!(&*calls.borrow(), &["rm -rf /"]));

    let mut agent = fake_agent(
        Confirm,
        UserRules {
            allow: vec![
                UserRule::exact("sudo echo hello").unwrap(),
                UserRule::exact("echo hello").unwrap(),
            ],
            deny: vec![],
        },
    );
    let mut approval = Scripted::new([]);
    agent.exec_call(
        &mut shell,
        &call("sudo echo hello"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert!(approval.seen.is_empty());
    assert_eq!(count(), 1);
    EXECUTED.with(|calls| assert_eq!(&*calls.borrow(), &["sudo -n echo hello"]));
}

#[test]
fn invalid_grants_and_missing_terminals_never_execute() {
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    let mut agent = fake_agent(Confirm, UserRules::default());
    let mut approval = Scripted::new([ApprovalResponse::ApproveSimilar]);
    assert!(matches!(
        agent.exec_call(
            &mut shell,
            &call("rm -rf /"),
            &mut approval,
            &mut RecordUi::default()
        ),
        Exec::Denied(_)
    ));
    assert_eq!(count(), 0);
    assert!(agent.allow.is_empty());
    let mut ui = RecordUi::default();
    assert!(matches!(
        agent.exec_call(
            &mut shell,
            &call("cargo test"),
            &mut crate::NoTerminal,
            &mut ui
        ),
        Exec::Denied(_)
    ));
    assert_eq!(count(), 0);
    assert!(ui.events.iter().any(|e| e.contains("approval unavailable")));
}

#[test]
fn common_builds_execute_once_but_cannot_smuggle_an_extra_call() {
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    let mut agent = fake_agent(Auto, UserRules::default());
    let mut approval = Scripted::new([]);
    agent.exec_call(
        &mut shell,
        &call("cargo test"),
        &mut approval,
        &mut RecordUi::default(),
    );
    assert_eq!(count(), 1);
    assert!(approval.seen.is_empty());
    assert!(matches!(
        agent.exec_call(
            &mut shell,
            &call("cargo test && rm -rf /"),
            &mut approval,
            &mut RecordUi::default()
        ),
        Exec::Denied(_)
    ));
    assert_eq!(count(), 1);
    assert!(approval.seen.is_empty());
}

#[test]
fn policy_uses_the_same_environment_overlay_as_execution() {
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    shell.run_user_line("PAGER=less");
    let mut agent = fake_agent(
        Confirm,
        UserRules {
            allow: vec![],
            deny: vec![UserRule::exact("echo cat").unwrap()],
        },
    );
    let mut approval = Scripted::new([]);
    assert!(matches!(
        agent.exec_call(
            &mut shell,
            &call("echo \"$PAGER\""),
            &mut approval,
            &mut RecordUi::default()
        ),
        Exec::Denied(_)
    ));
    assert_eq!(count(), 0);
    assert!(approval.seen.is_empty());
    assert_eq!(
        shell.var("PAGER").as_deref(),
        Some("less"),
        "analysis must not mutate the session"
    );
}

#[test]
fn changed_symlink_scope_is_reassessed_before_dispatch() {
    struct Repoint {
        link: PathBuf,
        protected: PathBuf,
        seen: Vec<ApprovalRequest>,
    }
    impl ApprovalChannel for Repoint {
        fn request(&mut self, req: &ApprovalRequest) -> ApprovalResponse {
            self.seen.push(req.clone());
            if self.seen.len() == 1 {
                std::fs::remove_file(&self.link).unwrap();
                std::os::unix::fs::symlink(&self.protected, &self.link).unwrap();
                ApprovalResponse::Approve
            } else {
                ApprovalResponse::Deny { reason: None }
            }
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let path = root.join("out");
    let protected = root.join("protected");
    std::fs::write(&path, "old").unwrap();
    std::fs::write(&protected, "keep").unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    let mut agent = fake_agent(Confirm, UserRules::default());
    agent.cfg.protected.push(protected.clone());
    let mut approval = Repoint {
        link: path.clone(),
        protected: protected.clone(),
        seen: vec![],
    };
    let command = format!("printf x > '{}'", path.display());
    assert!(matches!(
        agent.exec_call(
            &mut shell,
            &call(&command),
            &mut approval,
            &mut RecordUi::default()
        ),
        Exec::Denied(_)
    ));
    assert_eq!(approval.seen.len(), 2);
    assert_eq!(approval.seen[1].risk, Risk::Dangerous);
    assert_eq!(count(), 0);
    assert_eq!(std::fs::read_to_string(protected).unwrap(), "keep");
}

#[test]
fn additional_effects_wait_for_approval_before_any_execution() {
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    for name in ["first", "second", "existing"] {
        std::fs::write(root.join(name), name).unwrap();
    }
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    shell.run_user_line(&format!("cd '{}'", root.display()));
    shell.set_workspace(root.clone());
    let mut agent = fake_agent(Auto, UserRules::default());
    let mut approvals = Scripted::new([]);
    let commands = [
        "ping -c 1 router.local > existing",
        "mvn test deploy",
        "mv first /dev/null",
        "cp first /etc/file",
        "printf new > missing",
        "npm run build --prefix=/etc",
        "mvn test --file=/etc/pom.xml",
    ];
    for command in commands {
        assert!(matches!(
            agent.exec_call(
                &mut shell,
                &call(command),
                &mut approvals,
                &mut RecordUi::default()
            ),
            Exec::Denied(_)
        ));
    }
    assert_eq!(approvals.seen.len(), commands.len());
    assert_eq!(count(), 0);
    for name in ["first", "second", "existing"] {
        assert_eq!(std::fs::read_to_string(root.join(name)).unwrap(), name);
    }
    assert!(!root.join("missing").exists());
}

#[test]
fn routine_copy_and_move_execute_without_approval() {
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    std::fs::write(root.join("source"), "content").unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(root.clone()),
        ..Default::default()
    })
    .unwrap();
    shell.set_workspace(root.clone());
    let mut agent = fake_agent(Auto, UserRules::default());
    agent.command_runner = EmbeddedShell::run_agent_command;
    let mut approvals = Scripted::new([]);
    let mut ui = RecordUi::default();
    let result = agent.exec_call(
        &mut shell,
        &call("cp source copied && mv copied moved"),
        &mut approvals,
        &mut ui,
    );
    assert!(matches!(result, Exec::CommandResult(_)));
    assert!(approvals.seen.is_empty());
    assert_eq!(
        std::fs::read_to_string(root.join("moved")).unwrap(),
        "content"
    );
    assert!(!root.join("copied").exists());
    assert!(ui.events.iter().any(|event| event.contains("ordinary cp")));
    assert!(ui.events.iter().any(|event| event.contains("ordinary mv")));
}

#[test]
fn scoped_script_and_deep_read_denies_never_execute() {
    use nosh_permissions::RuleSpec;
    let dir = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    std::fs::write(root.join("trusted.sh"), "bash -c 'printf new > blocked'\n").unwrap();
    std::fs::create_dir(root.join("private")).unwrap();
    std::fs::write(root.join("private/secret"), "needle").unwrap();
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    shell.run_user_line(&format!("cd '{}'", root.display()));
    shell.set_workspace(root.clone());
    for mode in [Confirm, Auto, Yolo] {
        let rule = UserRule::compile(
            RuleSpec {
                command_exact: Some("./trusted.sh".into()),
                write_paths: vec!["blocked".into()],
                ..RuleSpec::default()
            },
            "script deny",
        )
        .unwrap();
        let read_rule = UserRule::compile(
            RuleSpec {
                tool: Some("grep".into()),
                path: Some("private/secret".into()),
                ..RuleSpec::default()
            },
            "grep deny",
        )
        .unwrap();
        let mut agent = fake_agent(
            mode,
            UserRules {
                allow: vec![UserRule::prefix("./trusted.sh").unwrap()],
                deny: vec![rule, read_rule],
            },
        );
        agent.cfg.protected.push("private".into());
        let mut approvals = Scripted::new([ApprovalResponse::Approve]);
        let mut ui = RecordUi::default();
        let read = ToolCall {
            name: "grep".into(),
            args: serde_json::json!({"path": "private", "pattern": "needle"})
                .as_object()
                .unwrap()
                .clone(),
        };
        for call in [call("./trusted.sh"), read] {
            assert!(matches!(
                agent.exec_call(&mut shell, &call, &mut approvals, &mut ui),
                Exec::Denied(_)
            ));
        }
        assert_eq!(
            approvals.seen.len(),
            usize::from(mode != Yolo),
            "unexpected approval count in {mode:?}"
        );
        assert_eq!(count(), 0);
        assert!(ui.events.iter().any(|event| event.contains("user deny")));
    }
    assert!(!root.join("blocked").exists());
}

#[test]
fn path_and_hash_lookups_do_not_auto_admit_workspace_lookalikes() {
    use std::os::unix::fs::PermissionsExt;
    let directory = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(directory.path()).unwrap();
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    for name in ["mv", "cp", "cargo", "ping", "sort"] {
        let file = bin.join(name);
        std::fs::write(&file, "#!/bin/sh\nprintf fixture\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions {
        working_dir: Some(root.clone()),
        ..Default::default()
    })
    .unwrap();
    shell.set_workspace(root.clone());
    let path = shell.var("PATH").unwrap();
    shell.run_user_line(&format!(
        "export PATH='{}:{}'; hash -r",
        bin.display(),
        path
    ));
    let mut agent = fake_agent(Auto, UserRules::default());
    let mut approvals = Scripted::new([]);
    for command in ["mv a b", "cp a b", "cargo test", "ping host", "sort input"] {
        assert!(
            matches!(
                agent.exec_call(
                    &mut shell,
                    &call(command),
                    &mut approvals,
                    &mut RecordUi::default()
                ),
                Exec::Denied(_)
            ),
            "{command}"
        );
    }
    assert_eq!(count(), 0);
    assert_eq!(approvals.seen.len(), 5);
    shell.run_user_line(&format!(
        "export PATH='{path}'; hash -r; hash -p '{}' mv",
        bin.join("mv").display()
    ));
    let report = prepared_command("mv a b", &agent.cfg.permission_context(&shell), &shell);
    assert_eq!(
        report.operations[0].executable.as_deref(),
        Some(bin.join("mv").as_path())
    );
    assert!(report.operations[0].local_program);

    let user_install = tempfile::tempdir().unwrap();
    let file = user_install.path().join("cargo");
    std::fs::write(&file, "#!/bin/sh\nprintf fixture\n").unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    shell.run_user_line(&format!(
        "export PATH='{}:{path}'; hash -r",
        user_install.path().display()
    ));
    let mut approvals = Scripted::new([]);
    assert!(matches!(
        agent.exec_call(
            &mut shell,
            &call("cargo test"),
            &mut approvals,
            &mut RecordUi::default()
        ),
        Exec::CommandResult(_)
    ));
    assert!(
        approvals.seen.is_empty(),
        "user-installed tools outside the project do not require a system-directory allowlist"
    );
    let temporary_path = format!("PATH='{}:{path}' cargo test", bin.display());
    let report = prepared_command(
        &temporary_path,
        &agent.cfg.permission_context(&shell),
        &shell,
    );
    assert!(report.operations.iter().any(|op| op.local_program));
    assert_eq!(
        shell.var("PATH").unwrap(),
        format!("{}:{path}", user_install.path().display())
    );

    let venv = root.join(".venv");
    std::fs::create_dir_all(venv.join("bin")).unwrap();
    let python = venv.join("bin/python");
    std::fs::write(&python, "#!/bin/sh\nprintf fixture\n").unwrap();
    std::fs::set_permissions(&python, std::fs::Permissions::from_mode(0o755)).unwrap();
    shell.run_user_line(&format!(
        "export VIRTUAL_ENV='{}' PATH='{}:{path}'; hash -r",
        venv.display(),
        venv.join("bin").display()
    ));
    let report = prepared_command(
        "python -m pytest",
        &agent.cfg.permission_context(&shell),
        &shell,
    );
    assert!(!report.operations[0].local_program);
    assert_eq!(
        evaluate(
            &report,
            Auto,
            &UserRules::default(),
            &SessionAllowList::default()
        )
        .decision,
        Decision::Allow
    );
}

#[test]
fn expanded_commands_and_disappearing_arguments_keep_the_approval_boundary() {
    let mut shell = EmbeddedShell::new(nosh_shell::ShellOptions::default()).unwrap();
    shell.run_user_line("alias cargo='unknown_project_runner'");
    let mut agent = fake_agent(Auto, UserRules::default());
    let mut approvals = Scripted::new([]);
    let mut ui = RecordUi::default();
    assert!(matches!(
        agent.exec_call(&mut shell, &call("cargo test"), &mut approvals, &mut ui),
        Exec::Denied(_)
    ));
    assert_eq!(approvals.seen.len(), 1);
    assert_eq!(count(), 0);
    shell.run_user_line("alias cargo='/usr/bin/cargo'");
    assert!(matches!(
        agent.exec_call(&mut shell, &call("cargo test"), &mut approvals, &mut ui),
        Exec::CommandResult(_)
    ));
    assert_eq!(approvals.seen.len(), 1);
    assert_eq!(count(), 1);

    let mut agent = fake_agent(
        Yolo,
        UserRules {
            allow: vec![UserRule::prefix("git").unwrap()],
            deny: vec![UserRule::exact("git fetch").unwrap()],
        },
    );
    let mut approvals = Scripted::new([]);
    assert!(matches!(
        agent.exec_call(
            &mut shell,
            &call("ARG=x; read ARG; git fetch $ARG"),
            &mut approvals,
            &mut ui
        ),
        Exec::Denied(_)
    ));
    assert!(approvals.seen.is_empty());
    assert_eq!(count(), 0);
    assert!(ui.events.iter().any(|event| event.contains("user deny")));
}
