//! Project environment contracts without a model or the user's configuration.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, Instant};

use nosh_shell::project_env::Provider;
use nosh_shell::{AgentExecOpts, EmbeddedShell, NullSink, ShellOptions};

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn executable(path: &Path, text: &str) {
    fs::write(path, text).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}

fn fixture(script: &str) -> (tempfile::TempDir, EmbeddedShell) {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("bin")).unwrap();
    executable(&dir.path().join("bin/direnv"), script);
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(dir.path().into()),
        catch_sigint: true,
        ..Default::default()
    })
    .unwrap();
    assert_eq!(shell.run_user_line(&format!(
        "export PATH={}/bin:/usr/bin:/bin; export HOME={}; export XDG_CONFIG_HOME={}/config; export XDG_DATA_HOME={}/data; export XDG_STATE_HOME={}/state; export XDG_CACHE_HOME={}/cache; unset DIRENV_DIFF DIRENV_DIR DIRENV_WATCHES DIRENV_CONFIG DIRENV_DISABLE __MISE_DIFF __MISE_SESSION __MISE_LAST_UNTRUSTED_CONFIG_WARNING_KEY",
        quote(dir.path()), quote(dir.path()), quote(dir.path()), quote(dir.path()), quote(dir.path()), quote(dir.path()),
    )).exit_code, 0);
    shell.configure_project_env(Ok(Provider::Direnv));
    (dir, shell)
}

fn agent(shell: &mut EmbeddedShell, command: &str) -> nosh_shell::CommandResult {
    shell
        .run_agent_command(command, &AgentExecOpts::default(), &mut NullSink)
        .unwrap()
}

#[test]
fn manual_and_agent_use_committed_environment_without_losing_temporary_guards() {
    let (_dir, mut shell) =
        fixture("#!/bin/sh\nprintf '{\"PROJECT\":\"A\",\"PAGER\":\"project-pager\"}'\n");
    shell.run_user_line("false");
    shell.refresh_project_env().unwrap();
    assert_eq!(shell.last_exit_status(), 1, "refresh preserves $?");
    assert_eq!(shell.var("PROJECT").as_deref(), Some("A"));
    let result = agent(&mut shell, "printf '%s:%s' \"$PROJECT\" \"$PAGER\"");
    assert_eq!(result.stdout, "A:cat");
    assert_eq!(shell.var("PAGER").as_deref(), Some("project-pager"));
}

#[test]
fn failure_is_atomic_and_never_blocks_manual_recovery() {
    let (dir, mut shell) = fixture("#!/bin/sh\nprintf '{\"PROJECT\":\"incorrect\"}'; exit 1\n");
    shell.run_user_line("export PROJECT=original");
    assert!(shell.refresh_project_env().is_err());
    assert_eq!(shell.var("PROJECT").as_deref(), Some("original"));
    assert!(
        shell
            .run_agent_command("touch must-not-run", &Default::default(), &mut NullSink)
            .is_err()
    );
    assert!(!dir.path().join("must-not-run").exists());
    assert_eq!(
        shell.run_user_line("export MANUAL_RECOVERY=yes").exit_code,
        0
    );
    executable(
        &dir.path().join("bin/direnv"),
        "#!/bin/sh\nprintf '{\"PROJECT\":\"fixed\"}'\n",
    );
    shell.refresh_project_env().unwrap();
    assert_eq!(
        agent(&mut shell, "printf '%s' \"$PROJECT\"").stdout,
        "fixed"
    );
}

#[test]
fn readonly_conflicts_do_not_install_any_part_of_a_delta() {
    let (_dir, mut shell) =
        fixture("#!/bin/sh\nprintf '{\"A_NEW\":\"new\",\"Z_LOCKED\":\"changed\"}'\n");
    shell.run_user_line("readonly Z_LOCKED=original");
    assert!(shell.refresh_project_env().is_err());
    assert_eq!(shell.var("A_NEW"), None);
    assert_eq!(shell.var("Z_LOCKED").as_deref(), Some("original"));
}

#[test]
fn merged_variable_count_limit_preserves_the_previous_environment() {
    let (dir, mut shell) = fixture("#!/bin/sh\ncat \"$HOME/delta.json\"\n");
    {
        let (_, shared) = shell.shared();
        let mut sh = shared.lock().unwrap();
        for index in 0..8192 {
            let mut variable = brush_core::ShellVariable::new("x");
            variable.export();
            sh.env_mut()
                .set_global(format!("ORIGINAL_{index}"), variable)
                .unwrap();
        }
    }
    shell.run_user_line("export PROJECT=original");
    let before = shell.snapshot();
    let mut patch: std::collections::BTreeMap<String, String> = (0..8192)
        .map(|index| (format!("ADDED_{index}"), "x".into()))
        .collect();
    patch.insert("PROJECT".into(), "changed".into());
    patch.insert("PATH".into(), "/unverified".into());
    fs::write(
        dir.path().join("delta.json"),
        serde_json::to_vec(&patch).unwrap(),
    )
    .unwrap();

    assert!(shell.refresh_project_env().unwrap_err().contains("limit"));
    assert!(
        shell.snapshot() == before,
        "failed refresh changed the live environment"
    );
    assert_eq!(shell.var("ADDED_0"), None);
}

#[test]
fn merged_transaction_size_limit_preserves_the_previous_environment() {
    let (dir, mut shell) = fixture("#!/bin/sh\ncat \"$HOME/delta.json\"\n");
    let value = "x".repeat(2 * 1024 * 1024);
    {
        let (_, shared) = shell.shared();
        shared
            .lock()
            .unwrap()
            .env_mut()
            .set_global(
                "UNEXPORTED_LARGE",
                brush_core::ShellVariable::new(value.clone()),
            )
            .unwrap();
    }
    let before = shell.snapshot();
    fs::write(
        dir.path().join("delta.json"),
        serde_json::to_vec(&serde_json::json!({"ADDED_LARGE": value, "PATH": "/unverified"}))
            .unwrap(),
    )
    .unwrap();

    assert!(
        shell
            .refresh_project_env()
            .unwrap_err()
            .contains("transaction size limit")
    );
    assert!(
        shell.snapshot() == before,
        "oversized candidate changed the live environment"
    );
    assert_eq!(shell.var("ADDED_LARGE"), None);
}

#[test]
fn exported_live_state_not_process_startup_environment_is_used() {
    let (_dir, mut shell) = fixture("#!/bin/sh\nprintf '{\"SEEN\":\"%s\"}' \"$LIVE_VALUE\"\n");
    shell.run_user_line("export LIVE_VALUE=after-startup");
    shell.refresh_project_env().unwrap();
    assert_eq!(shell.var("SEEN").as_deref(), Some("after-startup"));
}

#[test]
fn cancelled_tasks_do_not_install_late_values_and_other_user_jobs_survive() {
    let (dir, mut shell) = fixture("#!/bin/sh\nsleep 30\nprintf '{\"LATE\":\"bad\"}'\n");
    let mut unrelated = std::process::Command::new("sleep")
        .arg("30")
        .spawn()
        .unwrap();
    let started = Instant::now();
    shell.begin_project_env();
    assert!(started.elapsed() < Duration::from_millis(500));
    shell.cancel_project_env();
    assert!(shell.finish_project_env().is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
    assert_eq!(shell.var("LATE"), None);
    assert!(unrelated.try_wait().unwrap().is_none());
    unrelated.kill().unwrap();
    unrelated.wait().unwrap();
    executable(
        &dir.path().join("bin/direnv"),
        "#!/bin/sh\nprintf '{\"RECOVERED\":\"yes\"}'\n",
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if shell.refresh_project_env().is_ok() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "cancelled task was not reclaimed"
        );
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(shell.var("LATE"), None);
    assert_eq!(shell.var("RECOVERED").as_deref(), Some("yes"));
}

#[test]
fn directory_changes_invalidate_agent_readiness_but_not_bash_semantics() {
    let (dir, mut shell) = fixture(
        "#!/bin/sh\ncase \"$PWD\" in */child) printf '{\"PROJECT\":\"child\"}';; *) printf '{\"PROJECT\":\"parent\"}';; esac\n",
    );
    fs::create_dir(dir.path().join("child")).unwrap();
    shell.refresh_project_env().unwrap();
    assert_eq!(
        agent(&mut shell, "cd child && printf '%s' \"$PROJECT\"").stdout,
        "parent"
    );
    assert!(
        shell
            .run_agent_command("printf should-not-run", &Default::default(), &mut NullSink)
            .is_err()
    );
    shell.refresh_project_env().unwrap();
    assert_eq!(
        agent(&mut shell, "printf '%s' \"$PROJECT\"").stdout,
        "child"
    );
    shell.restore_working_dir(dir.path()).unwrap();
    assert!(
        shell
            .run_agent_command("true", &Default::default(), &mut NullSink)
            .is_err()
    );
    shell.refresh_project_env().unwrap();
    assert_eq!(shell.var("PROJECT").as_deref(), Some("parent"));
}

#[test]
fn conflicting_hook_and_invalid_configuration_are_visible_failures() {
    let (_dir, mut shell) = fixture("#!/bin/sh\nprintf '{\"SHOULD_NOT_LOAD\":\"yes\"}'\n");
    shell.run_user_line("_direnv_hook() { :; }");
    assert!(shell.refresh_project_env().is_err());
    assert_eq!(shell.var("SHOULD_NOT_LOAD"), None);
    shell.configure_project_env(Err("invalid project_env selection".into()));
    assert!(shell.refresh_project_env().is_err());
    assert_eq!(shell.run_user_line("true").exit_code, 0);
}

#[test]
fn disabled_mise_hook_is_allowed_unless_it_is_still_registered() {
    let (dir, mut shell) = fixture("#!/bin/sh\nexit 99\n");
    executable(
        &dir.path().join("bin/mise"),
        "#!/bin/sh\ncase \"$1\" in\n--version) printf '2026.10.5 linux-x64\\n';;\nhook-env) printf 'set,PROJECT,loaded\\n';;\nls) printf '{}\\n';;\nesac\n",
    );
    shell.configure_project_env(Ok(Provider::Mise));
    shell.run_user_line("__MISE_HOOK_ENABLED=0; _mise_hook() { :; }; PROMPT_COMMAND=':'");
    shell.refresh_project_env().unwrap();
    assert_eq!(
        agent(&mut shell, "printf '%s' \"$PROJECT\"").stdout,
        "loaded"
    );

    for registration in [
        "PROMPT_COMMAND='_mise_hook'",
        "PROMPT_COMMAND=(: _mise_hook_prompt_command)",
        "precmd_functions=(_mise_hook)",
        "chpwd_functions=(_mise_hook_chpwd)",
    ] {
        shell.run_user_line(&format!(
            "unset PROMPT_COMMAND precmd_functions chpwd_functions; {registration}"
        ));
        assert!(
            shell
                .refresh_project_env()
                .unwrap_err()
                .contains("hook already loaded"),
            "{registration}"
        );
    }
    shell.run_user_line(
        "unset PROMPT_COMMAND precmd_functions chpwd_functions; __MISE_HOOK_ENABLED=1",
    );
    assert!(
        shell
            .refresh_project_env()
            .unwrap_err()
            .contains("hook already loaded")
    );
}

#[test]
fn unset_and_empty_values_are_distinct_and_stale_results_are_discarded() {
    let (_dir, mut shell) = fixture(
        "#!/bin/sh\nsleep 0.1\nprintf '{\"REMOVE\":null,\"EMPTY\":\"\",\"NEW\":\"value\"}'\n",
    );
    shell.run_user_line("export REMOVE=before EMPTY=before");
    shell.begin_project_env();
    shell.run_user_line("export CHANGED_DURING_REFRESH=yes");
    assert!(shell.finish_project_env().is_err());
    assert_eq!(shell.var("REMOVE").as_deref(), Some("before"));
    shell.refresh_project_env().unwrap();
    assert_eq!(shell.var("REMOVE"), None);
    assert_eq!(shell.var("EMPTY").as_deref(), Some(""));
}

#[test]
fn mise_zero_exit_is_not_proof_of_trust_or_installed_tools() {
    let (dir, mut shell) = fixture("#!/bin/sh\nexit 99\n");
    let program = dir.path().join("bin/mise");
    let script = |delta: &str, tools: &str| {
        format!(
            "#!/bin/sh\ncase \"$1\" in\n--version) printf '2026.10.5 linux-x64\\n';;\nhook-env) printf '%s\\n' '{delta}';;\nls) printf '%s\\n' '{tools}';;\nesac\n"
        )
    };
    executable(
        &program,
        &script(
            "set,__MISE_LAST_UNTRUSTED_CONFIG_WARNING_KEY,untrusted",
            "{}",
        ),
    );
    shell.configure_project_env(Ok(Provider::Mise));
    assert!(
        shell
            .refresh_project_env()
            .unwrap_err()
            .contains("not trusted")
    );
    executable(
        &program,
        &script(
            "set,PROJECT,A",
            r#"{"python":[{"installed":false,"active":true}]}"#,
        ),
    );
    assert!(shell.refresh_project_env().unwrap_err().contains("missing"));
    assert_eq!(
        shell.var("PROJECT"),
        None,
        "failed tool readiness cannot partially apply variables"
    );
    executable(
        &program,
        &script(
            "set,PROJECT,A",
            r#"{"python":[{"installed":true,"active":true}]}"#,
        ),
    );
    shell.refresh_project_env().unwrap();
    assert_eq!(agent(&mut shell, "printf '%s' \"$PROJECT\"").stdout, "A");
}

#[test]
fn continuously_writing_tool_is_bounded_and_never_applies_output() {
    let (_dir, mut shell) = fixture(
        "#!/bin/sh\nwhile :; do printf 'xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\\n'; done\n",
    );
    let start = Instant::now();
    assert!(shell.refresh_project_env().is_err());
    assert!(start.elapsed() < Duration::from_secs(6));
    assert_eq!(shell.var("PROJECT"), None);
}

#[test]
fn timeout_preserves_shell_state_and_releases_the_owned_process() {
    let (dir, mut shell) = fixture("#!/bin/sh\ntrap '' TERM\nsleep 30\n");
    let start = Instant::now();
    assert!(shell.refresh_project_env().is_err());
    assert!(start.elapsed() >= Duration::from_secs(4));
    assert!(start.elapsed() < Duration::from_secs(6));
    executable(
        &dir.path().join("bin/direnv"),
        "#!/bin/sh\nprintf '{\"AFTER_TIMEOUT\":\"yes\"}'\n",
    );
    let deadline = Instant::now() + Duration::from_secs(3);
    while shell.refresh_project_env().is_err() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(30));
    }
    assert_eq!(shell.var("AFTER_TIMEOUT").as_deref(), Some("yes"));
}

fn real_provider(provider: Provider, variable: &str) {
    let binary =
        std::env::var_os(variable).expect("set the absolute path to the documented tool version");
    let (dir, mut shell) = fixture("#!/bin/sh\nexit 99\n");
    let link = dir.path().join("bin").join(provider.name());
    if link.exists() {
        fs::remove_file(&link).unwrap();
    }
    std::os::unix::fs::symlink(binary, &link).unwrap();
    shell.configure_project_env(Ok(provider));
    let state = shell.snapshot();
    let environment: Vec<_> = state
        .vars
        .iter()
        .filter(|(key, _)| state.exported.contains(*key))
        .collect();
    for name in ["a", "b"] {
        let project = dir.path().join(name);
        fs::create_dir(&project).unwrap();
        let runtime = project.join("runtime");
        assert!(
            std::process::Command::new("/usr/bin/python3")
                .args(["-m", "venv", "--without-pip"])
                .arg(&runtime)
                .status()
                .unwrap()
                .success()
        );
        let authorize = match provider {
            Provider::Direnv => {
                fs::write(
                    project.join(".envrc"),
                    format!(
                        "export PROJECT_LABEL={name}\nexport PATH={}/bin:$PATH\n",
                        quote(&runtime)
                    ),
                )
                .unwrap();
                vec!["allow", "."]
            }
            Provider::Mise => {
                fs::write(project.join("mise.toml"), format!(
                    "[tools]\npython = \"path:{}\"\n[env]\nPROJECT_LABEL = \"{name}\"\n[hooks]\nenter = \"touch forbidden-hook\"\n[shell_alias]\nforbidden_alias = \"echo forbidden\"\n",
                    runtime.display(),
                )).unwrap();
                vec!["trust", "mise.toml"]
            }
            Provider::Off => unreachable!(),
        };
        assert!(
            std::process::Command::new(&link)
                .args(authorize)
                .current_dir(&project)
                .env_clear()
                .envs(environment.iter().map(|(k, v)| (*k, *v)))
                .stdin(std::process::Stdio::null())
                .output()
                .unwrap()
                .status
                .success()
        );
    }
    if provider == Provider::Mise {
        let activation = std::process::Command::new(&link)
            .args(["activate", "bash", "--no-hook-env"])
            .current_dir(dir.path())
            .env_clear()
            .envs(environment.iter().map(|(k, v)| (*k, *v)))
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(activation.status.success());
        assert_eq!(
            shell
                .run_user_line(&String::from_utf8(activation.stdout).unwrap())
                .exit_code,
            0
        );
        assert!(shell.has_function("_mise_hook"));
        assert_eq!(shell.var("__MISE_HOOK_ENABLED").as_deref(), Some("0"));
    }
    for name in ["a", "b", ".", "a"] {
        shell.run_user_line(&format!("cd -- {}", quote(&dir.path().join(name))));
        let start = Instant::now();
        shell.refresh_project_env().unwrap();
        println!(
            "{} {name} refresh_ms={}",
            provider.name(),
            start.elapsed().as_millis()
        );
        assert_eq!(
            shell.var("PROJECT_LABEL").as_deref(),
            (name != ".").then_some(name)
        );
        if name != "." {
            let result = agent(
                &mut shell,
                "python -c 'import os,sys; print(os.environ[\"PROJECT_LABEL\"]); print(sys.prefix)'",
            );
            assert_eq!(result.exit_code, 0, "{}", result.stderr);
            assert!(result.stdout.starts_with(&format!("{name}\n")));
            assert!(
                result
                    .stdout
                    .contains(&dir.path().join(name).join("runtime").display().to_string())
            );
            assert!(!dir.path().join(name).join("forbidden-hook").exists());
        }
    }
    let untrusted = dir.path().join("untrusted");
    fs::create_dir(&untrusted).unwrap();
    match provider {
        Provider::Direnv => {
            fs::write(untrusted.join(".envrc"), "export UNTRUSTED_VALUE=bad\n").unwrap()
        }
        Provider::Mise => fs::write(
            untrusted.join("mise.toml"),
            "[env]\nUNTRUSTED_VALUE='bad'\n",
        )
        .unwrap(),
        Provider::Off => unreachable!(),
    }
    shell.run_user_line(&format!("cd -- {}", quote(&untrusted)));
    assert!(shell.refresh_project_env().is_err());
    assert_eq!(shell.var("UNTRUSTED_VALUE"), None);
    assert!(
        shell
            .run_agent_command("touch forbidden", &Default::default(), &mut NullSink)
            .is_err()
    );
}

#[test]
#[ignore = "requires NOSH_TEST_DIRENV pointing to direnv 2.38.1 and python3 with venv"]
fn real_direnv_switches_and_unloads_authorized_project_environments() {
    real_provider(Provider::Direnv, "NOSH_TEST_DIRENV");
}

#[test]
#[ignore = "requires NOSH_TEST_MISE pointing to mise 2026.10.5 and python3 with venv"]
fn real_mise_switches_without_hooks_installs_or_implicit_trust() {
    real_provider(Provider::Mise, "NOSH_TEST_MISE");
}
