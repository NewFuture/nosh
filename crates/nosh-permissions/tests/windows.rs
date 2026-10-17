#![cfg(windows)]

use std::path::{Path, PathBuf};

use nosh_permissions::{
    ApprovalMode, Context, Decision, PathClass, Risk, RuleSpec, SessionAllowList, UserRule,
    UserRules, assess_command, assess_read, classify_path, classify_path_real, evaluate,
};

#[test]
fn explicit_native_programs_do_not_gain_help_or_version_admission() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = directory.path().join("workspace");
    let outside = directory.path().join("outside");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::create_dir(&outside).unwrap();
    let context = Context::new(&workspace, &workspace);
    for name in ["tool.exe", "cargo", "cp"] {
        let executable = outside.join(name);
        std::fs::write(&executable, b"MZ\0native fixture").unwrap();
        for path in [
            executable.to_string_lossy().into_owned(),
            executable.to_string_lossy().replace('\\', "/"),
            format!(r"..\outside\{name}"),
        ] {
            for flag in ["--help", "--version"] {
                let command = format!("'{path}' {flag}");
                let report = assess_command(&command, &context);
                assert!(report.syntax_error.is_none(), "{command}: {report:?}");
                assert!(report.risk() >= Risk::Mutating, "{command}: {report:?}");
                assert!(report.operations[0].opaque, "{command}: {report:?}");
                for mode in [ApprovalMode::Confirm, ApprovalMode::Auto] {
                    let policy = evaluate(
                        &report,
                        mode,
                        &UserRules::default(),
                        &SessionAllowList::default(),
                    );
                    assert!(
                        matches!(policy.decision, Decision::Ask { .. }),
                        "{command}: {mode:?}: {policy:?}"
                    );
                }
            }
        }
    }
    for flag in ["--help", "--version"] {
        assert_eq!(
            assess_command(&format!("tool {flag}"), &context).risk(),
            Risk::Safe,
        );
    }
}

#[test]
fn normalization_keeps_drives_unc_roots_and_clamps_parents() {
    let context = Context::new(r"C:\work\sub", r"C:\work").with_home(r"C:\Users\Alice");
    assert_eq!(context.resolve(r"..\file"), PathBuf::from(r"C:\work\file"));
    assert_eq!(
        context.resolve(r"D:\other\..\file"),
        PathBuf::from(r"D:\file")
    );
    assert_eq!(context.resolve(r"..\..\..\.."), PathBuf::from(r"C:\"));
    assert_eq!(
        context.resolve(r"\\server\share\dir\..\file"),
        PathBuf::from(r"\\server\share\file")
    );
    assert_eq!(
        context.resolve("~/.ssh/id"),
        PathBuf::from(r"C:\Users\Alice\.ssh\id")
    );
}

#[test]
fn scopes_compare_windows_case_and_canonical_prefixes_without_crossing_drives() {
    let context = Context::new(r"C:\work\sub", r"C:\work").with_home("C:\\Users\\\u{00c4}lice");
    for path in [
        "C:\\Users\\\u{00e4}LICE\\.SSH\\id_rsa",
        "\\\\?\\C:\\Users\\\u{00c4}lice\\.ssh\\id_rsa",
        "C:\\Users\\\u{00c4}lice\\.ssh. \\id_rsa",
    ] {
        assert!(
            matches!(
                classify_path(Path::new(path), &context),
                PathClass::Protected(_)
            ),
            "{path}"
        );
    }
    assert_eq!(
        classify_path(Path::new(r"c:\WORK\file"), &context),
        PathClass::Workspace
    );
    assert_eq!(
        classify_path(Path::new(r"D:\work\file"), &context),
        PathClass::Outside
    );
    assert_eq!(
        classify_path(Path::new(r"C:\work-other\file"), &context),
        PathClass::Outside
    );
    assert_eq!(classify_path(Path::new(r"C:\"), &context), PathClass::Root);
    assert!(matches!(
        classify_path(Path::new(r"C:\work\.ENV"), &context),
        PathClass::Protected(_)
    ));
}

#[test]
fn ambiguous_drive_paths_device_names_and_streams_are_not_workspace_files() {
    let context = Context::new(r"C:\work", r"C:\work");
    for path in [
        r"C:relative",
        r"C:\work\file:stream",
        r"\\.\PhysicalDrive0",
        r"C:\work\CON",
        r"C:\work\COM1.txt",
        r"\\?\C:\work \file",
        r"\\?\C:\work.\file",
        r"\\?\C:\work\NUL.txt",
        r"\\?\C:\work\..\outside",
    ] {
        assert!(
            matches!(
                classify_path(Path::new(path), &context),
                PathClass::Protected(_)
            ),
            "{path}"
        );
    }
    assert_eq!(
        classify_path(Path::new(r"C:\work\NUL"), &context),
        PathClass::Null
    );
}

#[test]
fn verbatim_literal_directories_cannot_inherit_workspace_authority() {
    let root = std::env::temp_dir().canonicalize().unwrap();
    let directory = tempfile::tempdir_in(root).unwrap();
    let workspace = directory.path().join("work");
    std::fs::create_dir(&workspace).unwrap();
    std::fs::write(workspace.join("file"), "inside").unwrap();
    let context = Context::new(&workspace, &workspace);
    let rule = UserRule::compile(
        RuleSpec {
            tool: Some("read_file".into()),
            path: Some("**".into()),
            ..Default::default()
        },
        "workspace.allow",
    )
    .unwrap();
    let rules = UserRules {
        allow: vec![rule],
        ..Default::default()
    };
    for name in ["work ", "work."] {
        let outside = directory.path().join(name);
        std::fs::create_dir(&outside).unwrap();
        let target = outside.join("file");
        std::fs::write(&target, "outside").unwrap();
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "outside");
        assert_eq!(
            std::fs::read_to_string(workspace.join("file")).unwrap(),
            "inside"
        );
        let resolved = context.resolve(target.to_str().unwrap());
        assert_eq!(resolved, target);
        for path in [&target, &target.canonicalize().unwrap()] {
            assert!(
                matches!(classify_path(path, &context), PathClass::Protected(_)),
                "{path:?}"
            );
            assert!(matches!(
                classify_path_real(path, &context, true).0,
                PathClass::Protected(_)
            ));
            let report = assess_read("read_file", path, None, &context);
            let policy = evaluate(
                &report,
                ApprovalMode::Confirm,
                &rules,
                &SessionAllowList::default(),
            );
            assert!(!matches!(policy.decision, Decision::Allow), "{policy:?}");
        }
        std::fs::remove_dir_all(outside).unwrap();
    }
}

#[test]
fn existing_files_have_the_same_scope_after_windows_canonicalization() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("file.txt");
    std::fs::write(&path, "data").unwrap();
    let context = Context::new(directory.path(), directory.path());
    let canonical = path.canonicalize().unwrap();
    assert_eq!(classify_path(&canonical, &context), PathClass::Workspace);
    assert_eq!(
        classify_path_real(&path, &context, true).0,
        PathClass::Workspace
    );
}

#[test]
fn deny_rules_match_windows_case_and_native_separators() {
    let directory = tempfile::tempdir().unwrap();
    let sensitive = directory.path().join("Sensitive");
    std::fs::create_dir(&sensitive).unwrap();
    std::fs::write(sensitive.join("Secret.txt"), "private").unwrap();
    let context = Context::new(directory.path(), directory.path());
    let target = directory.path().join("SENSITIVE").join("SECRET.TXT");
    let report = assess_read("read_file", &target, None, &context);
    for pattern in [
        "sensitive/**".to_string(),
        r"Sensitive\**".to_string(),
        format!(r"{}\Sensitive\**", directory.path().display()),
    ] {
        let rule = UserRule::compile(
            RuleSpec {
                tool: Some("read_file".into()),
                path: Some(pattern.clone()),
                ..Default::default()
            },
            "test.deny",
        )
        .unwrap();
        let policy = evaluate(
            &report,
            ApprovalMode::Yolo,
            &UserRules {
                deny: vec![rule],
                ..Default::default()
            },
            &SessionAllowList::default(),
        );
        assert!(
            matches!(policy.decision, Decision::Deny { .. }),
            "{pattern}: {policy:?}"
        );
    }
}

#[test]
fn home_rules_use_native_profile_when_home_is_unset() {
    if let Some(home) = std::env::var_os("NOSH_RULE_HOME_PROBE") {
        let home = PathBuf::from(home);
        assert!(std::env::var_os("HOME").is_none());
        let rule = UserRule::compile(
            RuleSpec {
                tool: Some("read_file".into()),
                path: Some("~/sensitive/**".into()),
                cwd: Some("~".into()),
                ..Default::default()
            },
            "test.deny",
        )
        .unwrap();
        let context = Context::new(&home, &home).with_home(&home);
        let report = assess_read(
            "read_file",
            &home.join("sensitive").join("file"),
            None,
            &context,
        );
        let policy = evaluate(
            &report,
            ApprovalMode::Yolo,
            &UserRules {
                deny: vec![rule],
                ..Default::default()
            },
            &SessionAllowList::default(),
        );
        assert!(
            matches!(policy.decision, Decision::Deny { .. }),
            "{policy:?}"
        );
        return;
    }
    use std::os::windows::process::CommandExt;
    let directory = tempfile::tempdir().unwrap();
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "home_rules_use_native_profile_when_home_is_unset",
            "--nocapture",
        ])
        .env_remove("HOME")
        .env("USERPROFILE", directory.path())
        .env("NOSH_RULE_HOME_PROBE", directory.path())
        .creation_flags(windows_sys::Win32::System::Threading::DETACHED_PROCESS)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}
