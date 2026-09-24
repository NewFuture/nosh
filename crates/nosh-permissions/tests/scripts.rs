//! Convenience first (design §6): commands whose effects are unknown stay
//! Mutating (no extra confirmation); shell scripts are analyzed with the same
//! rules instead, and only Dangerous or Forbidden contents escalate. Writes
//! to workspace files chosen at runtime are Mutating; deletions and paths
//! that may leave the workspace stay Dangerous.

use std::path::PathBuf;

use nosh_permissions::{
    ApprovalMode, Context, Decision, Risk, SessionAllowList, UserRules, assess_command, decide,
};

use Risk::*;

/// `<tmp>/home/proj` as workspace and cwd, with the scripts below.
fn fixture() -> (Context, PathBuf) {
    let root = std::env::temp_dir().join(format!(
        "nosh-scripts-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let home = root.join("home");
    let ws = home.join("proj");
    std::fs::create_dir_all(ws.join("scripts")).unwrap();
    let files: &[(&str, &str)] = &[
        ("clean.sh", "#!/bin/sh\nrm -rf build\n"),
        (
            "build.sh",
            "#!/usr/bin/env bash\nset -euo pipefail\ncd \"$(dirname \"$0\")\"\nexport RUSTFLAGS=-Dwarnings\ncargo build --release\ncp target/release/app dist/\ncurl -sSO https://example.com/asset.tar.gz\n[ -f dist/app ] || exit 1\nexit 0\n",
        ),
        ("wipe", "rm -rf /\n"),
        ("calls.sh", "#!/bin/sh\necho start\n./clean.sh\n"),
        ("self.sh", "#!/bin/sh\n./self.sh\n"),
        ("replace.sh", "#!/bin/bash\nexec rm -rf build\n"),
        (
            "scripts/deploy.sh",
            "#!/bin/bash\nsudo systemctl restart app\n",
        ),
        (
            "fn.sh",
            "#!/bin/sh\ncleanup() { rm -rf \"$1\"; }\necho ok\n",
        ),
        (
            "tool.py",
            "#!/usr/bin/env python3\nimport os\nos.system('rm -rf ~')\n",
        ),
        ("bad.sh", "#!/bin/sh\nif then fi ((\n"),
        ("aliases.sh", "#!/bin/sh\nll\n"),
    ];
    for (name, body) in files {
        std::fs::write(ws.join(name), body).unwrap();
    }
    std::fs::write(ws.join("bin.run"), [0x7f, b'E', b'L', b'F', 0, 1, 2]).unwrap();
    let mut big = "#!/bin/sh\n".to_string();
    while big.len() < 300 * 1024 {
        big.push_str("echo padding padding padding padding\n");
    }
    big.push_str("rm -rf build\n");
    std::fs::write(ws.join("big.sh"), big).unwrap();
    let mut c = Context::new(&ws, &ws).with_home(&home);
    c.aliases.insert("ll".to_string(), "rm -rf /".to_string());
    (c, root)
}

#[test]
fn shell_scripts_are_analyzed_and_only_escalate_for_dangerous_contents() {
    let (c, root) = fixture();
    let ws = c.workspace.display().to_string();
    let abs = format!("{ws}/clean.sh");
    let abs_bin = format!("{ws}/bin.run --version");
    let cases: Vec<(String, Risk)> = [
        // Analyzed: Dangerous or Forbidden contents escalate.
        ("./clean.sh", Dangerous),
        ("bash clean.sh", Dangerous),
        ("sh ./clean.sh", Dangerous),
        (abs.as_str(), Dangerous),
        ("./wipe", Forbidden),
        ("./calls.sh", Dangerous),
        ("./replace.sh", Dangerous),
        ("scripts/deploy.sh", Dangerous),
        ("./fn.sh", Dangerous),
        ("timeout 60 ./clean.sh", Dangerous),
        ("cd scripts && ./deploy.sh", Dangerous),
        // Analyzed, nothing destructive: Mutating. `exit`, `cd`, `export` and
        // network access inside the script stay in the script.
        ("./build.sh", Mutating),
        ("bash build.sh", Mutating),
        ("./self.sh", Mutating),
        // A child shell does not see the session's aliases (ll = rm -rf /).
        ("./aliases.sh", Mutating),
        // Not analyzed: other interpreters, binaries, unparsable, too big,
        // missing. Mutating, as before.
        ("./tool.py", Mutating),
        ("python3 tool.py", Mutating),
        ("./bin.run", Mutating),
        ("./bad.sh", Mutating),
        ("./big.sh", Mutating),
        ("./missing.sh", Mutating),
        ("./configure", Mutating),
        // Unlisted commands and inline code keep their classification.
        ("frobnicate --all", Mutating),
        ("node -e 'console.log(1)'", Mutating),
        // `--version`/`--help` makes only unlisted programs run by name Safe;
        // local programs and scripts are graded as without it.
        ("./bin.run --version", Mutating),
        (abs_bin.as_str(), Mutating),
        ("./build.sh --help", Mutating),
        ("./clean.sh --help", Dangerous),
    ]
    .iter()
    .map(|(c, r)| (c.to_string(), *r))
    .collect();
    let rules = UserRules::default();
    let none = SessionAllowList::default();
    let mut failures = Vec::new();
    for (cmd, want) in &cases {
        let r = assess_command(cmd, &c);
        if r.risk() != *want {
            failures.push(format!(
                "{cmd:?}: want {want}, got {} ({:?})",
                r.risk(),
                r.findings
            ));
        }
        // Mutating scripts need no extra confirmation: auto runs them (the
        // build script's network access does not surface), confirm asks once.
        if *want == Mutating {
            let d = |m| decide(&r, cmd, m, &rules, &none);
            if d(ApprovalMode::Auto) != Decision::Allow
                || d(ApprovalMode::Confirm) != (Decision::Ask { strong: false })
            {
                failures.push(format!(
                    "{cmd:?}: auto {:?}, confirm {:?}",
                    d(ApprovalMode::Auto),
                    d(ApprovalMode::Confirm)
                ));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // The reason names the script.
    let r = assess_command("./clean.sh", &c);
    assert!(
        r.top_reasons()
            .iter()
            .any(|t| t.starts_with("./clean.sh: ")),
        "{:?}",
        r.top_reasons()
    );
    // Script internals are not simple commands of the line (allow rules and
    // grants keep matching what the user sees).
    assert_eq!(
        assess_command("./build.sh", &c).commands,
        vec!["./build.sh"]
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn child_shells_keep_exit_cd_and_functions_to_themselves() {
    let (c, root) = fixture();
    let cases: &[(&str, Risk)] = &[
        ("bash -c 'make || exit 1'", Mutating),
        ("sh -c 'exec make'", Mutating),
        ("bash -c 'exec rm -rf build'", Dangerous),
        // The session itself still may not be left.
        ("exit", Forbidden),
        ("make || exit 1", Forbidden),
        // A subshell is a child shell too.
        ("(exit 1)", Safe),
        ("echo $(cd /tmp; exit 3)", Safe),
        // `cd` in a child shell does not move the next command.
        ("bash -c 'cd /'; rm -rf ./*", Dangerous),
        // A function defined in a child shell or subshell does not shadow `rm`.
        ("bash -c 'rm() { :; }'; rm -rf build", Dangerous),
        ("(rm() { :; }); rm -rf build", Dangerous),
    ];
    let mut failures = Vec::new();
    for (cmd, want) in cases {
        let r = assess_command(cmd, &c);
        if r.risk() != *want {
            failures.push(format!(
                "{cmd:?}: want {want}, got {} ({:?})",
                r.risk(),
                r.findings
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let r = assess_command("bash -c 'export PATH=/opt/x:$PATH; make'", &c);
    assert!(!r.changes_session, "a child shell's export stays in it");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn runtime_targets_inside_the_workspace_are_mutating() {
    let (c, root) = fixture();
    let cases: &[(&str, Risk)] = &[
        // Chosen at runtime among workspace files: Mutating.
        (
            "for f in *.txt; do mv \"$f\" \"${f%.txt}.md\"; done",
            Mutating,
        ),
        ("for f in *.log; do cp \"$f\" \"$f.bak\"; done", Mutating),
        (
            "for f in src/*.rs; do sed -i 's/a/b/' \"$f\"; done",
            Mutating,
        ),
        ("for f in a.txt b.txt; do touch \"$f\"; done", Mutating),
        ("for f in *.txt; do echo x > \"out/$f\"; done", Mutating),
        ("find . -name '*.log' -exec cp {} {}.bak \\;", Mutating),
        ("find src -type f -exec chmod 644 {} +", Mutating),
        ("find . -name '*.txt' -execdir mv {} {}.old \\;", Mutating),
        // Deletions stay Dangerous.
        ("for f in *.log; do rm \"$f\"; done", Dangerous),
        ("find . -name '*.tmp' -exec rm {} \\;", Dangerous),
        ("find . -name '*.tmp' -delete", Dangerous),
        // Targets that may leave the workspace stay Dangerous.
        ("for f in ../*.txt; do mv \"$f\" x; done", Dangerous),
        ("for f in *.txt; do mv \"$f\" \"../$f\"; done", Dangerous),
        ("for f in *.txt; do cp \"$f\" \"/tmp/$f\"; done", Dangerous),
        ("for f in *.txt; do cp \"$f\" ~/\"$f\"; done", Dangerous),
        ("for f in .*; do cp \"$f\" \"$f.bak\"; done", Dangerous),
        ("for f in $(ls); do mv \"$f\" \"$f.bak\"; done", Dangerous),
        ("for f; do mv \"$f\" \"$f.bak\"; done", Dangerous),
        ("for f in *.txt; do cd /tmp; mv \"$f\" x; done", Dangerous),
        (
            "for f in *.txt; do f=/etc/hosts; cp x \"$f\"; done",
            Dangerous,
        ),
        ("for f in *.txt; do read -r f; cp x \"$f\"; done", Dangerous),
        ("for f in *.txt; do mv \"$f\" \"${f#*.}\"; done", Dangerous),
        ("cd /tmp && for f in *.txt; do mv \"$f\" x; done", Dangerous),
        ("find / -name '*.log' -exec cp {} {}.bak \\;", Dangerous),
        ("find -L . -exec cp {} {}.bak \\;", Dangerous),
        ("find .. -exec cp {} {}.bak \\;", Dangerous),
        ("find . -exec cp {} ../{} \\;", Dangerous),
        ("echo x > \"$OUT\"", Dangerous),
        // find -delete with leading options still sees the start directory.
        ("find -L / -delete", Forbidden),
    ];
    let mut failures = Vec::new();
    for (cmd, want) in cases {
        let r = assess_command(cmd, &c);
        if r.risk() != *want {
            failures.push(format!(
                "{cmd:?}: want {want}, got {} ({:?})",
                r.risk(),
                r.findings
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    // Workspace-only runtime writes run in auto mode without confirmation.
    let cmd = "for f in *.txt; do mv \"$f\" \"${f%.txt}.md\"; done";
    let r = assess_command(cmd, &c);
    assert!(!r.writes_outside_workspace);
    let d = decide(
        &r,
        cmd,
        ApprovalMode::Auto,
        &UserRules::default(),
        &SessionAllowList::default(),
    );
    assert_eq!(d, Decision::Allow);
    let _ = std::fs::remove_dir_all(root);
}
