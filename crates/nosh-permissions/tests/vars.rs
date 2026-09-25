//! Reads through variables and function or script arguments (review of
//! PR #1): a value the analysis knows is checked against the protected paths
//! like a literal path, so it needs the same confirmation. Values it cannot
//! know keep their grading (design §6: convenience first), and write targets
//! built from variables stay computed at runtime.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use nosh_permissions::{
    ApprovalMode, Context, Decision, Risk, SessionAllowList, UserRules, assess_command, decide,
};

static FIXTURE_ID: AtomicU64 = AtomicU64::new(0);

/// `<tmp>/home/proj` as workspace and cwd, with a few scripts and session
/// variables.
fn fixture() -> (Context, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "nosh-vars-{}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
        FIXTURE_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let home = root.join("home");
    let ws = home.join("proj");
    std::fs::create_dir_all(&ws).unwrap();
    for (name, body) in [
        ("show.sh", "#!/bin/sh\ncat \"$1\"\n"),
        ("env.sh", "#!/bin/sh\ncat \"$SECRET\"\n"),
    ] {
        std::fs::write(ws.join(name), body).unwrap();
    }
    let key = home.join(".ssh/id_rsa").display().to_string();
    let mut c = Context::new(&ws, &ws).with_home(&home);
    for (k, v) in [
        ("KEY_PATH", key.clone()),
        ("KEY_DIR", home.join(".ssh").display().to_string()),
        ("SPACED", format!("{key} {}", ws.join("x").display())),
        ("EMPTY", String::new()),
    ] {
        c.variables.insert(k.to_string(), v);
    }
    c.functions
        .insert("showsess".to_string(), "{ cat \"$1\"; }".to_string());
    (c, root)
}

fn check(c: &Context, cases: &[(&str, bool)], failures: &mut Vec<String>) {
    let rules = UserRules::default();
    let none = SessionAllowList::default();
    for (cmd, protected) in cases {
        let r = assess_command(cmd, c);
        let auto = decide(&r, cmd, ApprovalMode::Auto, &rules, &none);
        let ok = if *protected {
            // Like `cat ~/.ssh/id_rsa`: Mutating, and auto mode asks once.
            r.reads_protected
                && r.risk() == Risk::Mutating
                && auto == Decision::Ask { strong: false }
        } else {
            !r.reads_protected
        };
        if !ok {
            failures.push(format!(
                "{cmd:?}: want protected={protected}, got {} protected={} auto {auto:?} ({:?})",
                r.risk(),
                r.reads_protected,
                r.findings
            ));
        }
    }
}

#[test]
fn known_values_are_checked_like_literal_paths() {
    let (c, root) = fixture();
    let cases: &[(&str, bool)] = &[
        // Session variables.
        ("cat \"$KEY_PATH\"", true),
        ("cat $KEY_PATH", true),
        ("cat \"${KEY_DIR}/id_rsa\"", true),
        ("head -n 3 < \"$KEY_PATH\"", true),
        // Assignments earlier in the line.
        ("KEY=~/.ssh/id_rsa; cat \"$KEY\"", true),
        ("KEY=~/.ssh/id_rsa && cat \"$KEY\"", true),
        ("D=~/.ssh; F=\"$D/id_rsa\"; cat \"$F\"", true),
        ("F=~/.ssh; F+=/id_rsa; cat \"$F\"", true),
        ("export KEY=~/.ssh/id_rsa; cat \"$KEY\"", true),
        // Functions defined in the line: `$1`… are the call's arguments,
        // variables are those known at the call.
        ("show() { cat \"$1\"; }; show ~/.ssh/id_rsa", true),
        ("show() { cat \"$2\"; }; show a ~/.ssh/id_rsa", true),
        (
            "show() { local f=\"$1\"; cat \"$f\"; }; show ~/.ssh/id_rsa",
            true,
        ),
        ("show() { cat \"$1\"; }; show \"$KEY_PATH\"", true),
        ("show() { cat \"$K\"; }; K=~/.ssh/id_rsa; show", true),
        // Session functions.
        ("showsess ~/.ssh/id_rsa", true),
        // Scripts and child shells: their arguments, exported variables.
        ("./show.sh ~/.ssh/id_rsa", true),
        ("bash show.sh ~/.ssh/id_rsa", true),
        ("SECRET=~/.ssh/id_rsa ./env.sh", true),
        ("export SECRET=~/.ssh/id_rsa; ./env.sh", true),
        ("bash -c 'cat \"$1\"' sh ~/.ssh/id_rsa", true),
        // Other values.
        ("show() { cat \"$1\"; }; show /tmp/x", false),
        ("KEY=~/.ssh/id_rsa; KEY=/tmp/x; cat \"$KEY\"", false),
        ("cat \"$KEY_DIR/../proj/notes.txt\"", false),
    ];
    let mut failures = Vec::new();
    check(&c, cases, &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn unknown_values_keep_their_grading() {
    let (c, root) = fixture();
    let cases: &[(&str, bool)] = &[
        ("cat \"$UNKNOWN\"", false),
        ("cat \"$EMPTY\"", false),
        // Split into several words at runtime: not one known path.
        ("cat $SPACED", false),
        // Reassigned in ways whose result is unknown.
        ("read -r KEY_PATH; cat \"$KEY_PATH\"", false),
        ("unset KEY_PATH; cat \"$KEY_PATH\"", false),
        ("for KEY_PATH in a b; do cat \"$KEY_PATH\"; done", false),
        ("KEY_PATH=$(pick); cat \"$KEY_PATH\"", false),
        // A subshell's assignment stays in it.
        ("(KEY=~/.ssh/id_rsa); cat \"$KEY\"", false),
        // Child processes only see exported variables.
        ("KEY=~/.ssh/id_rsa; bash -c 'cat \"$KEY\"'", false),
        ("bash -c 'cat \"$KEY_PATH\"'", false),
        ("./env.sh", false),
        ("SECRET=\"$KEY_PATH\"; ./env.sh", false),
        // A function's own `$1` is not the caller's.
        ("show() { cat \"$1\"; }", false),
    ];
    let mut failures = Vec::new();
    check(&c, cases, &mut failures);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Nothing else changes: an unknown read target is Safe and runs in auto
    // mode without confirmation, as before.
    let r = assess_command("cat \"$UNKNOWN\"", &c);
    assert_eq!(r.risk(), Risk::Safe, "{:?}", r.findings);
    assert_eq!(
        decide(
            &r,
            "cat \"$UNKNOWN\"",
            ApprovalMode::Auto,
            &UserRules::default(),
            &SessionAllowList::default()
        ),
        Decision::Allow
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn exported_session_variables_reach_scripts() {
    let (mut c, root) = fixture();
    c.variables.insert(
        "SECRET".to_string(),
        c.home
            .as_ref()
            .unwrap()
            .join(".ssh/id_rsa")
            .display()
            .to_string(),
    );
    let mut failures = Vec::new();
    check(
        &c,
        &[("./env.sh", false), ("bash -c 'cat \"$SECRET\"'", false)],
        &mut failures,
    );
    c.exported.insert("SECRET".to_string());
    check(
        &c,
        &[
            ("./env.sh", true),
            ("bash -c 'cat \"$SECRET\"'", true),
            ("export -n SECRET; ./env.sh", false),
        ],
        &mut failures,
    );
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn write_targets_from_variables_stay_computed_at_runtime() {
    let (c, root) = fixture();
    // A value can be stale after a branch or loop, so known values only add
    // confirmations (protected reads); writes keep their grading.
    for (cmd, want) in [
        ("OUT=~/.bashrc; echo x > \"$OUT\"", Risk::Dangerous),
        ("OUT=build/x.txt; echo x > \"$OUT\"", Risk::Dangerous),
        ("DIR=build; rm -rf \"$DIR\"", Risk::Dangerous),
        ("CMD=ls; $CMD", Risk::Dangerous),
    ] {
        let r = assess_command(cmd, &c);
        assert_eq!(r.risk(), want, "{cmd}: {:?}", r.findings);
    }
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn calls_are_checked_without_applying_the_body_twice() {
    let (mut c, root) = fixture();
    // The definition applies `cd ..` once (to the workspace), the call does
    // not move further up and out of it.
    c.cwd = c.workspace.join("a");
    std::fs::create_dir_all(&c.cwd).unwrap();
    let r = assess_command("up() { cd ..; }; up; touch x", &c);
    assert!(!r.writes_outside_workspace, "{:?}", r.findings);
    assert_eq!(r.risk(), Risk::Mutating, "{:?}", r.findings);
    // `sudo` in the body is rewritten once, where it is written.
    let cmd = "f() { sudo apt-get update; }; f; f";
    let r = assess_command(cmd, &c);
    assert_eq!(
        r.rewritten.as_deref(),
        Some("f() { sudo -n apt-get update; }; f; f")
    );
    assert!(
        !r.findings.iter().any(|f| f.reason.contains("nested sudo")),
        "{:?}",
        r.findings
    );
    let _ = std::fs::remove_dir_all(root);
}
