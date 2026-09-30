use std::os::unix::{fs::PermissionsExt, process::ExitStatusExt};
use std::path::Path;

use nosh_permissions::{ApprovalMode, UserRule, UserRules};
use nosh_shell::{EmbeddedShell, ShellOptions};
use serde_json::Value;

use super::*;

fn rendered(stdout: &str, stderr: &str, topic: Option<&str>) -> (Value, String) {
    let output = Captured {
        stdout: stdout.as_bytes().to_vec(),
        stderr: stderr.as_bytes().to_vec(),
        status: Some(ExitStatus::from_raw(0)),
        complete: true,
    };
    split(
        &help::render(
            &HelpCommand::parse("tar").unwrap(),
            Path::new("/bin/tar"),
            Path::new("/usr/bin/tar"),
            &output,
            topic,
        )
        .unwrap(),
    )
}

fn split(result: &str) -> (Value, String) {
    let mut parts = result.splitn(3, '\n');
    assert_eq!(parts.next(), Some("[command_help]"));
    (
        serde_json::from_str(parts.next().unwrap()).unwrap(),
        parts.next().unwrap().into(),
    )
}

#[test]
fn option_matching_ignores_case_but_keeps_boundaries_and_original_spelling() {
    let source = "\
Usage: tar [OPTION...] [FILE...]

  -c, --create               create a new archive
  -C, --directory=DIR        change to directory DIR
                            before reading the following file

                            and retain this continuation paragraph
      --directory-prefix=P  unrelated prefix
  -z, --gzip                filter the archive through gzip
  -Z, --compress            filter through compress
";
    for topic in ["-c", "-C"] {
        let (meta, body) = rendered(source, "", Some(topic));
        assert_eq!(meta["matched_blocks"], 2, "{topic}: {body}");
        assert!(body.contains("  -c, --create"), "{body}");
        assert!(body.contains("  -C, --directory=DIR"), "{body}");
        assert!(body.contains("continuation paragraph"), "{body}");
        assert!(!body.contains("unrelated prefix"), "{body}");
        assert_eq!(meta["excerpt_truncated"], false);
    }
    for (topic, expected, excluded) in [
        ("--directory", "before reading", "unrelated prefix"),
        ("--DIRECTORY", "before reading", "unrelated prefix"),
        ("--GZIP", "--gzip", "--compress"),
        ("gz", "--gzip", "--compress"),
        ("Gz", "--gzip", "--compress"),
        ("GZIP", "--gzip", "--compress"),
    ] {
        let (meta, body) = rendered(source, "", Some(topic));
        assert_eq!(meta["matched_blocks"], 1, "{topic}: {body}");
        assert!(body.contains(expected), "{topic}: {body}");
        assert!(!body.contains(excluded), "{topic}: {body}");
        assert_eq!(meta["excerpt_truncated"], false);
    }
    for topic in ["-d", "--dir", "--gzip-extra"] {
        let (meta, body) = rendered(source, "", Some(topic));
        assert_eq!(meta["matched_blocks"], 0);
        assert!(body.contains("does not establish"));
    }
}

#[test]
fn optional_negation_in_git_help_matches_both_spellings_without_rewriting_output() {
    let source = "Usage: git commit [--amend]\n\n    --[no-]amend          amend previous commit\n    --[no-]amend-extra    unrelated option\n    --[no-]Amend          differently spelled option\n";
    for query in ["--amend", "--no-amend", "--AMEND", "--NO-AMEND"] {
        let (meta, body) = rendered(source, "", Some(query));
        assert!(meta["matched_blocks"].as_u64().unwrap() > 0);
        assert!(
            body.contains("--[no-]amend          amend previous commit"),
            "{body}"
        );
        assert!(!body.contains("unrelated option"), "{body}");
        assert!(
            body.contains("--[no-]Amend          differently spelled option"),
            "{body}"
        );
    }
    for query in ["--amen", "--no-amen", "--AMEND-EXTRA-LONGER"] {
        let (meta, _) = rendered(source, "", Some(query));
        assert_eq!(meta["matched_blocks"], 0);
    }
}

#[test]
fn command_names_accept_literal_subcommands_but_not_shell_syntax_or_options() {
    for (name, program, arguments) in [
        ("tar", "tar", vec!["--help"]),
        ("git commit", "git", vec!["commit", "-h"]),
        (
            " /usr/bin/git  remote add ",
            "/usr/bin/git",
            vec!["remote", "add", "-h"],
        ),
        (
            "docker compose build",
            "docker",
            vec!["compose", "build", "--help"],
        ),
        ("cargo build", "cargo", vec!["build", "--help"]),
    ] {
        let parsed = HelpCommand::parse(name).unwrap();
        assert_eq!(parsed.program, program);
        assert_eq!(parsed.arguments().collect::<Vec<_>>(), arguments);
    }
    for name in [
        "",
        "  ",
        "git\ncommit",
        "git\tcommit",
        "git -C elsewhere commit",
        "git commit --amend",
        "git commit;touch marker",
        "git commit && touch marker",
        "git $(touch marker)",
        "git `touch marker`",
        "git commit >marker",
        "git commit | cat",
        "git commit #comment",
        "git commit ${X}",
        "env X=1 git",
        "sh ./script",
        "git commit *",
        "git commit 'extra'",
        "git commit ../path",
        "-git commit",
        "git commit\u{202e}",
    ] {
        assert!(HelpCommand::parse(name).is_err(), "{name:?}");
    }
}

#[test]
fn bsd_busybox_and_unicode_help_keep_original_text_and_streams() {
    for (source, topic, expected) in [
        (
            "usage: tar -c [-options] [files]\n  -c  Create\n  -x  Extract\n",
            "-c",
            "  -c  Create",
        ),
        (
            "BusyBox multi-call binary.\nUsage: tar [-cxtz] [-f TARFILE]\n\n\t-c\tCreate archive\n\t-z\tGzip\n",
            "gzip",
            "\t-z\tGzip",
        ),
        (
            "Options:\n  --mode=VALUE  \u{00c4}nderung aktivieren\n                Fortsetzung\n",
            "\u{00e4}nderung",
            "Fortsetzung",
        ),
        (
            "Options:\n  --mode=VALUE  \u{0130}TEM details\n",
            "i\u{0307}tem",
            "\u{0130}TEM details",
        ),
        (
            "Options:\n  --mode=VALUE  \u{0130}TEM details\n",
            "\u{0307}t",
            "\u{0130}TEM details",
        ),
        (
            "\u{0130}nformation:\n  --Mode=VALUE  original spelling\n",
            "--MODE",
            "--Mode=VALUE",
        ),
    ] {
        let (meta, body) = rendered("", source, Some(topic));
        assert!(meta["matched_blocks"].as_u64().unwrap() > 0);
        assert!(body.starts_with("[stderr]\n"));
        assert!(body.contains(expected), "{body}");
        assert_eq!(meta["stdout_bytes"], 0);
        assert_eq!(meta["stderr_bytes"], source.len());
    }
}

#[test]
fn a_section_topic_includes_its_options_but_not_the_next_section_or_stream() {
    let source = "Usage: tool\n\nCompression options:\n\n  -z  gzip\n  -j  bzip2\n\nOther options:\n\n  -v  verbose\n";
    let (meta, body) = rendered(source, "unrelated stderr", Some("compression"));
    assert_eq!(meta["matched_blocks"], 3);
    assert!(body.contains("Compression options:"));
    assert!(body.contains("-z  gzip"));
    assert!(body.contains("-j  bzip2"));
    assert!(!body.contains("verbose"));
    assert!(!body.contains("unrelated stderr"));
}

#[test]
fn a_match_in_a_large_block_retains_its_declaration_and_match() {
    let source = format!(
        "  --format=NAME  choose a format\n    {} gzip detail {}\n",
        "x ".repeat(1800),
        "y ".repeat(1800)
    );
    let (meta, body) = rendered(&source, "", Some("gzip"));
    assert!(body.contains("--format=NAME"));
    assert!(body.contains("gzip detail"));
    assert_eq!(meta["excerpt_truncated"], true);
    assert!(body.chars().count() <= 1800);
    assert!(body.contains("[...]"));
}

#[test]
fn overview_prioritizes_synopsis_without_splicing_in_an_unrelated_tail() {
    let source = format!(
        "tool description\n\nUsage: tool [OPTIONS]\n\n  --first  {}\n\nirrelevant footer\n",
        "details ".repeat(1000)
    );
    let (meta, body) = rendered(&source, "", None);
    assert!(body.starts_with("[stdout]\nUsage: tool"));
    assert!(!body.contains("irrelevant footer"));
    assert_eq!(meta["matched_blocks"], Value::Null);
    assert_eq!(meta["excerpt_truncated"], true);
    assert!(body.chars().count() <= 1800);
}

#[test]
fn topic_search_reaches_beyond_the_old_head_tail_excerpt_and_handles_terminal_formatting() {
    let source = format!(
        "{}\n  \x1b[1m-z, --gzip\x1b[0m  Gzip compression\r\n                  continued description\r\n\n{}",
        "introduction\n\n".repeat(400),
        "footer\n\n".repeat(400)
    );
    let (meta, body) = rendered(&source, "", Some("--gzip"));
    assert_eq!(meta["matched_blocks"], 1);
    assert_eq!(meta["excerpt_truncated"], false);
    assert!(body.contains("-z, --gzip  Gzip compression\n                  continued description"));
    assert!(!body.contains("footer"));
    assert!(!body.contains('\x1b'));
    assert!(!body.contains('\r'));
}

fn process(script: &str) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", script])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0);
    command
}

#[test]
fn nonzero_stderr_usage_is_evidence_not_success_or_an_automatic_retry() {
    let output = capture(
        &mut process("printf 'usage: tool [options]\\n  -c  Create\\n' >&2; exit 1"),
        Duration::from_secs(1),
        &CancelHandle::default(),
    )
    .unwrap();
    let (meta, body) = split(
        &help::render(
            &HelpCommand::parse("tool").unwrap(),
            Path::new("/bin/tool"),
            Path::new("/real/tool"),
            &output,
            Some("-c"),
        )
        .unwrap(),
    );
    assert_eq!(meta["exit_code"], 1);
    assert_eq!(meta["capture_complete"], true);
    assert_eq!(meta["program"], "/bin/tool");
    assert_eq!(meta["executable"], "/real/tool");
    assert_eq!(meta["argument"], "--help");
    assert!(body.contains("[stderr]\n  -c  Create"));
}

#[test]
fn capture_limit_still_filters_topics_and_does_not_fabricate_an_exit_code() {
    let output = capture(
        &mut process("printf '  --gzip  compressed output\\n'; i=0; while [ \"$i\" -lt 5000 ]; do printf 'irrelevant padding padding\\n'; i=$((i+1)); done"),
        Duration::from_secs(3), &CancelHandle::default(),
    ).unwrap();
    assert_eq!(output.stdout.len() + output.stderr.len(), 64 * 1024);
    assert!(!output.complete);
    assert!(output.status.is_none());
    for (topic, matches) in [("--gzip", 1), ("--missing", 0)] {
        let (meta, body) = split(
            &help::render(
                &HelpCommand::parse("tool").unwrap(),
                Path::new("/bin/tool"),
                Path::new("/bin/tool"),
                &output,
                Some(topic),
            )
            .unwrap(),
        );
        assert_eq!(meta["matched_blocks"], matches);
        assert_eq!(meta["exit_code"], Value::Null);
        assert_eq!(meta["capture_complete"], false);
        assert!(!body.contains("irrelevant padding"));
    }
}

#[test]
fn empty_help_is_an_explicit_error_and_signal_is_not_exit_zero() {
    for stdout in ["", "\n \t\n", "\x1b[31m\x1b[0m"] {
        let output = Captured {
            stdout: stdout.as_bytes().into(),
            stderr: vec![],
            status: Some(ExitStatus::from_raw(0)),
            complete: true,
        };
        assert!(
            help::render(
                &HelpCommand::parse("tool").unwrap(),
                Path::new("/bin/tool"),
                Path::new("/bin/tool"),
                &output,
                None
            )
            .unwrap_err()
            .contains("no help text")
        );
    }
    let output = Captured {
        stdout: b"usage: tool\n".to_vec(),
        stderr: vec![],
        status: Some(ExitStatus::from_raw(libc::SIGTERM)),
        complete: true,
    };
    let (meta, _) = split(
        &help::render(
            &HelpCommand::parse("tool").unwrap(),
            Path::new("/bin/tool"),
            Path::new("/bin/tool"),
            &output,
            None,
        )
        .unwrap(),
    );
    assert_eq!(meta["exit_code"], Value::Null);
    assert_eq!(meta["signal"], libc::SIGTERM);
}

#[test]
fn queries_remain_cancellable_and_time_bounded() {
    let cancel = CancelHandle::default();
    cancel.cancel();
    assert!(
        capture(&mut process("exit 0"), Duration::from_secs(1), &cancel)
            .err()
            .unwrap()
            .contains("cancelled")
    );
    cancel.reset();
    let started = Instant::now();
    assert!(
        capture(
            &mut process("sleep 10 & wait"),
            Duration::from_millis(20),
            &cancel
        )
        .err()
        .unwrap()
        .contains("timed out")
    );
    assert!(started.elapsed() < Duration::from_secs(2));
    let other = cancel.clone();
    let thread = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(20));
        other.cancel();
    });
    let result = capture(
        &mut process("sleep 10 & wait"),
        Duration::from_secs(3),
        &cancel,
    );
    thread.join().unwrap();
    assert!(result.err().unwrap().contains("cancelled"));
}

fn tool(args: Value) -> ToolCall {
    ToolCall {
        name: "command_help".into(),
        args: args.as_object().unwrap().clone(),
    }
}

#[test]
fn focused_parameters_never_become_shell_arguments() {
    let dir = tempfile::tempdir().unwrap();
    let shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(dir.path().into()),
        ..Default::default()
    })
    .unwrap();
    let cfg = AgentConfig::default();
    let commands = CommandSnapshot::capture(&shell).unwrap();
    let context = cfg.permission_context(&shell);
    for args in [
        json!({"name":"ls","topic":"gzip"}),
        json!({"name":"git -C elsewhere status"}),
        json!({"name":"ls","query":""}),
        json!({"name":"ls","query":" \t"}),
        json!({"name":"ls","query":"-c\n--version"}),
        json!({"name":"ls","query":false}),
        json!({"name":"ls","query":"a".repeat(121)}),
        json!({"name":""}),
        json!({"name":"ls","args":["--help"]}),
        json!({"name":"ls","query":"\u{202e}hidden"}),
    ] {
        assert!(
            query(
                &commands,
                &context,
                &cfg,
                &tool(args.clone()),
                &CancelHandle::default()
            )
            .is_err(),
            "{args}"
        );
    }
    let result = query(
        &commands,
        &context,
        &cfg,
        &tool(json!({"name":"ls","query":"; touch ran"})),
        &CancelHandle::default(),
    )
    .unwrap();
    let (meta, _) = split(&result);
    assert_eq!(meta["matched_blocks"], 0);
    assert_eq!(meta["argument"], "--help");
    assert_eq!(meta["query"], "; touch ran");
    assert!(meta.get("topic").is_none());
    assert!(!dir.path().join("ran").exists());
}

#[test]
fn installed_tar_help_matches_partial_and_mixed_case_queries_without_rewriting_options() {
    let dir = tempfile::tempdir().unwrap();
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(dir.path().into()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line("export LC_ALL=C");
    let cfg = AgentConfig::default();
    let commands = CommandSnapshot::capture(&shell).unwrap();
    let context = cfg.permission_context(&shell);
    let Resolution::File(program) = commands.resolve("tar") else {
        panic!("tar required");
    };
    let raw = Command::new(program)
        .arg("--help")
        .env("LC_ALL", "C")
        .output()
        .unwrap();
    let raw_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&raw.stdout),
        String::from_utf8_lossy(&raw.stderr)
    );
    assert!(raw.status.success(), "{raw_text}");
    let mut previous = None;
    for filter in ["gzip", "gz", "GZ", "GZip", "--GZIP"] {
        let result = query(
            &commands,
            &context,
            &cfg,
            &tool(json!({"name":"tar","query":filter})),
            &CancelHandle::default(),
        )
        .unwrap();
        let (meta, body) = split(&result);
        assert_eq!(meta["query"], filter);
        assert_eq!(meta["exit_code"], json!(raw.status.code()));
        assert_eq!(meta["capture_complete"], true);
        assert_eq!(meta["excerpt_truncated"], false);
        let expected = if filter.starts_with("--") {
            "--gzip"
        } else {
            "gzip"
        };
        if raw_text.contains(expected) {
            assert!(meta["matched_blocks"].as_u64().unwrap() > 0);
            assert!(body.contains(expected), "{filter}: {body}");
        } else {
            assert_eq!(meta["matched_blocks"], 0, "{filter}: {body}");
        }
        assert!(!body.contains("Report bugs"));
        assert!(!body.contains("backup suffix"));
        assert!(body.chars().count() <= 1800);
        if matches!(filter, "gz" | "GZ") {
            if let Some(previous) = &previous {
                assert_eq!(&body, previous);
            }
            previous = Some(body);
        }
    }
    let mut previous = None;
    for filter in ["-c", "-C"] {
        let result = query(
            &commands,
            &context,
            &cfg,
            &tool(json!({"name":"tar","query":filter})),
            &CancelHandle::default(),
        )
        .unwrap();
        let (meta, body) = split(&result);
        assert_eq!(meta["capture_complete"], true);
        assert_eq!(meta["excerpt_truncated"], false);
        for line in raw_text.lines().filter(|line| {
            let line = line.trim_start();
            line.starts_with("-c,") || line.starts_with("-C,")
        }) {
            assert!(body.contains(line), "{filter}: {body}");
        }
        for option in ["--create", "--directory"] {
            if raw_text.contains(option) {
                assert!(body.contains(option), "{filter}: {body}");
            }
        }
        if let Some(previous) = &previous {
            assert_eq!(&body, previous);
        }
        previous = Some(body);
    }
}

#[test]
fn focused_queries_respect_identity_workspace_protection_and_deny_even_in_yolo() {
    let dir = tempfile::tempdir().unwrap();
    let fake = dir.path().join("tar");
    std::fs::write(&fake, "#!/bin/sh\ntouch ran\n").unwrap();
    std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(dir.path().into()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line("f(){ touch ran; }; alias t='touch ran'");
    let mut cfg = AgentConfig {
        mode: ApprovalMode::Yolo,
        ..Default::default()
    };
    for name in [
        "./tar",
        "./tar list",
        "f",
        "f child",
        "t",
        "t child",
        "cd",
        "cd child",
        "nosh_missing_help_program",
    ] {
        let result = query(
            &CommandSnapshot::capture(&shell).unwrap(),
            &cfg.permission_context(&shell),
            &cfg,
            &tool(json!({"name":name})),
            &CancelHandle::default(),
        );
        assert!(result.is_err(), "{name}: {result:?}");
    }
    cfg.rules = UserRules {
        allow: vec![],
        deny: vec![UserRule::exact("ls --help").unwrap()],
    };
    assert!(
        query(
            &CommandSnapshot::capture(&shell).unwrap(),
            &cfg.permission_context(&shell),
            &cfg,
            &tool(json!({"name":"ls"})),
            &CancelHandle::default()
        )
        .unwrap_err()
        .contains("not authorized")
    );
    cfg.rules = UserRules::default();
    let Resolution::File(ls) = CommandSnapshot::capture(&shell).unwrap().resolve("ls") else {
        panic!("ls required")
    };
    let link = dir.path().join("protected-ls");
    std::os::unix::fs::symlink(&ls, &link).unwrap();
    cfg.protected.push(link.clone());
    assert!(
        query(
            &CommandSnapshot::capture(&shell).unwrap(),
            &cfg.permission_context(&shell),
            &cfg,
            &tool(json!({"name":link})),
            &CancelHandle::default()
        )
        .unwrap_err()
        .contains("protected path")
    );
    cfg.protected.clear();
    std::fs::copy(&fake, dir.path().join("git")).unwrap();
    shell.run_user_line("export PATH=.:$PATH");
    for name in ["tar", "git commit"] {
        assert!(
            query(
                &CommandSnapshot::capture(&shell).unwrap(),
                &cfg.permission_context(&shell),
                &cfg,
                &tool(json!({"name":name})),
                &CancelHandle::default()
            )
            .unwrap_err()
            .contains("not authorized")
        );
    }
    assert!(!dir.path().join("ran").exists());
}

#[test]
fn external_query_uses_snapshot_identity_environment_cwd_and_only_the_help_flag() {
    let dir = tempfile::tempdir().unwrap();
    let installed = tempfile::tempdir().unwrap();
    let executable = installed.path().join("implementation");
    let program = installed.path().join("nosh-help-fixture");
    std::fs::write(&executable, "#!/bin/sh\nprintf 'Usage: fixture version list resolve gzip\\nargs=%s:%s pager=%s env=%s cwd=%s\\n' \"$#\" \"$1\" \"$PAGER\" \"$SNAPSHOT_VALUE\" \"$PWD\"\nif read line; then printf 'unexpected stdin'; exit 1; fi\n").unwrap();
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
    std::os::unix::fs::symlink(&executable, &program).unwrap();
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(dir.path().into()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line(&format!(
        "export PATH='{}':$PATH SNAPSHOT_VALUE=before PAGER=less",
        installed.path().display()
    ));
    let cfg = AgentConfig::default();
    let commands = CommandSnapshot::capture(&shell).unwrap();
    let context = cfg.permission_context(&shell);
    shell.run_user_line("export PATH=/missing SNAPSHOT_VALUE=after");
    for filter in [
        None,
        Some("version"),
        Some("list"),
        Some("resolve"),
        Some("gzip"),
        Some("--help"),
    ] {
        let mut args = json!({"name":"nosh-help-fixture"});
        if let Some(filter) = filter {
            args["query"] = json!(filter);
        }
        let result = query(
            &commands,
            &context,
            &cfg,
            &tool(args),
            &CancelHandle::default(),
        )
        .unwrap();
        let (meta, body) = split(&result);
        assert_eq!(meta["program"], json!(program));
        assert_eq!(
            meta["executable"],
            json!(executable.canonicalize().unwrap())
        );
        assert_eq!(meta["exit_code"], 0);
        assert_eq!(meta["query"], json!(filter));
        assert!(meta.get("topic").is_none());
        assert!(body.contains("args=1:--help pager=cat env=before"));
        assert!(body.contains(&format!("cwd={}", dir.path().display())));
        assert!(!body.contains("unexpected stdin"));
    }
}

#[test]
fn git_commit_and_nested_help_return_targeted_usage_without_repository_changes() {
    let dir = tempfile::tempdir().unwrap();
    let git = |args: &[&str]| {
        let result = Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
    };
    git(&["init", "-q"]);
    std::fs::write(dir.path().join("tracked.txt"), "unchanged\n").unwrap();
    git(&["add", "tracked.txt"]);
    git(&[
        "-c",
        "user.name=Fixture",
        "-c",
        "user.email=fixture@example.invalid",
        "-c",
        "core.hooksPath=/dev/null",
        "commit",
        "-qm",
        "baseline",
    ]);
    std::fs::write(dir.path().join("tracked.txt"), "uncommitted change\n").unwrap();
    git(&["config", "alias.nosh-help-unsafe", "!touch alias-ran"]);
    fn snapshot(root: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        let mut pending = vec![root.to_path_buf()];
        while let Some(path) = pending.pop() {
            if path.is_dir() {
                pending.extend(
                    std::fs::read_dir(path)
                        .unwrap()
                        .map(|entry| entry.unwrap().path()),
                );
            } else {
                files.insert(
                    path.strip_prefix(root).unwrap().to_path_buf(),
                    std::fs::read(path).unwrap(),
                );
            }
        }
        files
    }
    let before = snapshot(dir.path());
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(dir.path().into()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line("export LC_ALL=C GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null");
    let commands = CommandSnapshot::capture(&shell).unwrap();
    let cfg = AgentConfig::default();
    let context = cfg.permission_context(&shell);
    for (name, filter, subcommands) in [
        ("git commit", Some("--amend"), vec!["commit"]),
        ("git commit", Some("--AMEND"), vec!["commit"]),
        ("git remote add", None, vec!["remote", "add"]),
        ("git worktree add", None, vec!["worktree", "add"]),
        ("git stash push", None, vec!["stash", "push"]),
    ] {
        let mut args = json!({"name":name});
        if let Some(filter) = filter {
            args["query"] = json!(filter);
        }
        let result = query(
            &commands,
            &context,
            &cfg,
            &tool(args),
            &CancelHandle::default(),
        )
        .unwrap();
        let (meta, body) = split(&result);
        assert_eq!(meta["name"], name);
        assert_eq!(meta["subcommands"], json!(subcommands));
        assert_eq!(meta["argument"], "-h");
        assert_eq!(meta["exit_code"], 129, "{result}");
        assert_eq!(meta["capture_complete"], true);
        if filter.is_some() {
            assert!(body.contains("amend previous commit"), "{result}");
        } else {
            assert!(body.to_lowercase().contains("usage:"), "{result}");
        }
    }
    assert!(
        query(
            &commands,
            &context,
            &cfg,
            &tool(json!({"name":"git nosh-help-unsafe"})),
            &CancelHandle::default()
        )
        .unwrap_err()
        .contains("not authorized")
    );
    assert!(!dir.path().join("alias-ran").exists());
    for mode in [
        ApprovalMode::Auto,
        ApprovalMode::Confirm,
        ApprovalMode::Yolo,
    ] {
        let config = AgentConfig {
            mode,
            ..cfg.clone()
        };
        assert!(
            query(
                &commands,
                &context,
                &config,
                &tool(json!({"name":"git stash create"})),
                &CancelHandle::default()
            )
            .unwrap_err()
            .contains("not authorized")
        );
    }
    let cfg = AgentConfig {
        mode: ApprovalMode::Yolo,
        rules: UserRules {
            allow: vec![],
            deny: vec![UserRule::prefix("git commit").unwrap()],
        },
        ..cfg
    };
    assert!(
        query(
            &commands,
            &context,
            &cfg,
            &tool(json!({"name":"git commit","query":"--amend"})),
            &CancelHandle::default()
        )
        .unwrap_err()
        .contains("not authorized")
    );
    let after = snapshot(dir.path());
    assert_eq!(before, after);
}

#[test]
fn generic_subcommand_help_uses_separate_argv_and_does_not_bypass_permissions() {
    let dir = tempfile::tempdir().unwrap();
    let installed = tempfile::tempdir().unwrap();
    let helper = installed.path().join("ls");
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf 'Usage: fixture\\n'; printf '[%s]\\n' \"$@\"\n",
    )
    .unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut shell = EmbeddedShell::new(ShellOptions {
        working_dir: Some(dir.path().into()),
        ..Default::default()
    })
    .unwrap();
    shell.run_user_line(&format!(
        "export PATH='{}':$PATH",
        installed.path().display()
    ));
    let commands = CommandSnapshot::capture(&shell).unwrap();
    let cfg = AgentConfig::default();
    let context = cfg.permission_context(&shell);
    let result = query(
        &commands,
        &context,
        &cfg,
        &tool(json!({"name":"ls child nested","query":"nested"})),
        &CancelHandle::default(),
    )
    .unwrap();
    let (meta, body) = split(&result);
    assert_eq!(meta["subcommands"], json!(["child", "nested"]));
    assert_eq!(meta["argument"], "--help");
    assert!(body.contains("[child]\n[nested]\n[--help]"), "{result}");
    std::fs::write(dir.path().join("payload"), "touch ran\n").unwrap();
    for name in ["sh payload", "python3 payload"] {
        assert!(
            query(
                &commands,
                &context,
                &cfg,
                &tool(json!({"name":name})),
                &CancelHandle::default()
            )
            .is_err()
        );
    }
    assert!(!dir.path().join("ran").exists());
    let cfg = AgentConfig {
        rules: UserRules {
            allow: vec![],
            deny: vec![UserRule::exact("ls child nested --help").unwrap()],
        },
        ..cfg
    };
    assert!(
        query(
            &commands,
            &context,
            &cfg,
            &tool(json!({"name":"ls child nested"})),
            &CancelHandle::default()
        )
        .is_err()
    );
}
