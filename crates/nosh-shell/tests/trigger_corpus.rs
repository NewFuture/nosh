//! AI trigger corpus (design §13.2), checked against the local rules only.
//!
//! `fixtures/trigger_corpus.jsonl` holds one labelled line per record: `id`,
//! `category`, `input` and `expected`, plus the full `corrected` line for
//! typos. An optional `reason` explains a tricky label; it never excuses a
//! mismatch, and there are no skipped or expected failures.
//!
//! `classify` runs against a shell whose PATH holds only fake executables in a
//! temporary directory, with a controlled working tree, no rc and no model.
//! Corpus inputs are never executed, and every case is replayed in a second,
//! independent fixture.
//!
//! Every case must match. The four metrics are also asserted, each over one
//! category: Chinese requests routed to the AI (`zh_nl`; Chinese arguments of
//! valid commands do not count), false guards (`valid_command`), executed
//! destructive prose (`destructive_prose`; lines the guard lets through by
//! design, such as `rm -rf all temp files`, are labelled as valid commands)
//! and exact corrections (`typo`). CI runs this test in its own step so the
//! metrics show on success:
//!
//! ```sh
//! cargo test -p nosh-shell --test trigger_corpus -- --show-output
//! ```

use std::collections::HashSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;

use brush_core::ShellVariable;
use nosh_shell::trigger::{Action, Trigger, TriggerConfig, classify};
use nosh_shell::{EmbeddedShell, Resolution, ShellOptions};
use serde::Deserialize;
use tempfile::TempDir;

const COMMANDS: &[&str] = &[
    "awk",
    "basename",
    "cargo",
    "cat",
    "cc",
    "chgrp",
    "chmod",
    "chown",
    "cmp",
    "cp",
    "curl",
    "cut",
    "date",
    "dd",
    "df",
    "diff",
    "dirname",
    "docker",
    "du",
    "env",
    "file",
    "find",
    "free",
    "gcc",
    "git",
    "go",
    "grep",
    "gzip",
    "head",
    "hostname",
    "id",
    "ip",
    "killall",
    "kubectl",
    "ln",
    "ls",
    "make",
    "mkdir",
    "mkfs",
    "mkfs.ext4",
    "mv",
    "node",
    "npm",
    "npx",
    "pgrep",
    "pidof",
    "ping",
    "pkill",
    "pnpm",
    "printenv",
    "ps",
    "python",
    "python3",
    "readlink",
    "realpath",
    "rg",
    "rm",
    "rmdir",
    "rsync",
    "rustc",
    "scp",
    "sed",
    "shred",
    "sleep",
    "sort",
    "ss",
    "ssh",
    "stat",
    "sudo",
    "tail",
    "tar",
    "tee",
    "touch",
    "tr",
    "truncate",
    "uname",
    "uniq",
    "unlink",
    "unzip",
    "uptime",
    "wc",
    "wget",
    "which",
    "who",
    "whoami",
    "wipefs",
    "xargs",
    "yarn",
];

const FILES: &[&str] = &[
    "alpha",
    "beta",
    "gamma",
    "README.md",
    "notes.txt",
    "report final.txt",
    "中文.txt",
    "input.txt",
    "output.txt",
    "config.toml",
    "app.log",
    "old.log",
    "data.bin",
    "archive.tar",
    "script.py",
    "main.rs",
    "server.js",
    "args.txt",
];

struct Fixture {
    shell: EmbeddedShell,
    root: TempDir,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .prefix("nosh-trigger-corpus-")
            .tempdir()
            .unwrap();
        let bin = root.path().join("bin");
        let cwd = root.path().join("work");
        fs::create_dir(&bin).unwrap();
        fs::create_dir(&cwd).unwrap();
        for name in COMMANDS {
            Self::executable(&bin.join(name));
        }
        for name in [
            "src",
            "build",
            "target",
            "backup",
            "empty-dir",
            "tools",
            "folder with spaces",
            "目录",
        ] {
            fs::create_dir(cwd.join(name)).unwrap();
        }
        for name in FILES {
            fs::write(cwd.join(name), b"fixture\n").unwrap();
        }
        for name in ["runner", "rm"] {
            Self::executable(&cwd.join("tools").join(name));
        }
        let shell = EmbeddedShell::new(ShellOptions {
            working_dir: Some(cwd),
            ..ShellOptions::default()
        })
        .unwrap();
        let (_, shared) = shell.shared();
        let mut path = ShellVariable::new(bin.to_str().unwrap());
        path.export();
        shared
            .lock()
            .unwrap()
            .env_mut()
            .set_global("PATH", path)
            .unwrap();
        Self { shell, root }
    }

    fn executable(path: &std::path::Path) {
        fs::write(path, b"#!/bin/sh\n: > EXECUTED\nexit 97\n").unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn assert_untouched(&self) {
        let cwd = self.root.path().join("work");
        assert!(
            !cwd.join("EXECUTED").exists(),
            "a fake command was executed"
        );
        for name in FILES {
            assert_eq!(
                fs::read(cwd.join(name)).unwrap(),
                b"fixture\n",
                "corpus input modified {name}"
            );
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Category {
    ValidCommand,
    ZhNl,
    EnNl,
    Typo,
    DestructiveProse,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Expected {
    Execute,
    Ai,
    Correct,
    Guard,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Case {
    id: String,
    category: Category,
    input: String,
    expected: Expected,
    corrected: Option<String>,
    reason: Option<String>,
}

impl Case {
    fn matches(&self, actual: &Action) -> bool {
        match (self.expected, actual) {
            (Expected::Execute, Action::Execute) | (Expected::Guard, Action::Guard) => true,
            (Expected::Ai, Action::Ai { text, .. }) => text == self.input.trim(),
            (Expected::Correct, Action::Correct { corrected, .. }) => {
                self.corrected.as_deref() == Some(corrected.as_str())
            }
            _ => false,
        }
    }
}

fn corpus() -> Vec<Case> {
    let mut ids = HashSet::new();
    let mut inputs = HashSet::new();
    let cases: Vec<Case> = include_str!("fixtures/trigger_corpus.jsonl")
        .lines()
        .enumerate()
        .map(|(line, json)| {
            let case: Case = serde_json::from_str(json)
                .unwrap_or_else(|e| panic!("corpus line {}: {e}", line + 1));
            assert!(!case.id.trim().is_empty(), "empty ID on line {}", line + 1);
            assert!(ids.insert(case.id.clone()), "duplicate ID: {}", case.id);
            assert!(!case.input.trim().is_empty(), "{}: empty input", case.id);
            assert!(
                inputs.insert(case.input.trim().to_string()),
                "{}: duplicate input",
                case.id
            );
            let expected = match case.category {
                Category::ValidCommand => Expected::Execute,
                Category::ZhNl | Category::EnNl => Expected::Ai,
                Category::Typo => Expected::Correct,
                Category::DestructiveProse => Expected::Guard,
            };
            assert_eq!(case.expected, expected, "{}: category mismatch", case.id);
            if case.expected == Expected::Correct {
                assert!(
                    case.corrected
                        .as_deref()
                        .is_some_and(|s| !s.trim().is_empty() && s != case.input),
                    "{}: missing or unchanged correction",
                    case.id
                );
            } else {
                assert!(
                    case.corrected.is_none(),
                    "{}: unexpected correction field",
                    case.id
                );
            }
            assert!(
                case.reason.as_ref().is_none_or(|s| !s.trim().is_empty()),
                "{}: empty annotation",
                case.id
            );
            case
        })
        .collect();
    assert_eq!(cases.len(), 400, "update the documented corpus size too");
    for (category, count) in [
        (Category::ValidCommand, 200),
        (Category::ZhNl, 60),
        (Category::EnNl, 50),
        (Category::Typo, 50),
        (Category::DestructiveProse, 40),
    ] {
        assert_eq!(
            cases.iter().filter(|c| c.category == category).count(),
            count,
            "{category:?}: coverage changed"
        );
    }
    cases
}

#[derive(Default)]
struct Metrics {
    chinese: usize,
    chinese_ai: usize,
    valid: usize,
    false_guards: usize,
    typos: usize,
    corrected: usize,
    destructive: usize,
    executed: usize,
}

impl Metrics {
    fn record(&mut self, case: &Case, actual: &Action) {
        match case.category {
            Category::ZhNl => {
                self.chinese += 1;
                self.chinese_ai += usize::from(matches!(actual, Action::Ai { .. }));
            }
            Category::ValidCommand => {
                self.valid += 1;
                self.false_guards += usize::from(matches!(actual, Action::Guard));
            }
            Category::Typo => {
                self.typos += 1;
                self.corrected += usize::from(case.matches(actual));
            }
            Category::DestructiveProse => {
                self.destructive += 1;
                self.executed += usize::from(matches!(actual, Action::Execute));
            }
            Category::EnNl => {}
        }
    }

    fn violations(&self) -> Vec<&'static str> {
        let mut failures = Vec::new();
        if self.chinese == 0 || self.chinese_ai != self.chinese {
            failures.push("Chinese natural language must route to AI 100% of the time");
        }
        if self.destructive == 0 || self.executed != 0 {
            failures.push("destructive prose must never be executed");
        }
        if self.valid == 0 || self.false_guards * 200 >= self.valid {
            failures.push("false guards on valid commands must be strictly below 0.5%");
        }
        if self.typos == 0 || self.corrected * 10 < self.typos * 9 {
            failures.push("exact spelling correction hit rate must be at least 90%");
        }
        failures
    }

    fn report(&self) {
        for (label, numerator, denominator, threshold) in [
            (
                "Chinese NL -> AI",
                self.chinese_ai,
                self.chinese,
                "required: 100%",
            ),
            (
                "Destructive prose -> Execute",
                self.executed,
                self.destructive,
                "required: 0 executions",
            ),
            (
                "Valid commands -> Guard",
                self.false_guards,
                self.valid,
                "required: < 0.5%",
            ),
            (
                "Exact spelling corrections",
                self.corrected,
                self.typos,
                "required: >= 90%",
            ),
        ] {
            assert!(denominator > 0, "{label}: empty metric population");
            let percent = numerator as f64 * 100.0 / denominator as f64;
            println!("{label}: {numerator}/{denominator} ({percent:.2}%; {threshold})");
        }
    }
}

#[test]
fn trigger_corpus() {
    let cases = corpus();
    let mut first = Fixture::new();
    let mut second = Fixture::new();
    assert_ne!(first.root.path(), second.root.path());
    let first_before = first.shell.snapshot();
    let second_before = second.shell.snapshot();
    let cfg = TriggerConfig::default();
    let mut metrics = Metrics::default();
    let mut failures = Vec::new();
    for case in &cases {
        let actual = classify(&case.input, &mut first.shell, &cfg);
        let replay = classify(&case.input, &mut second.shell, &cfg);
        if actual != replay {
            failures.push(format!(
                "{}: fixture-dependent result: {actual:?} versus {replay:?}",
                case.id
            ));
        }
        metrics.record(case, &actual);
        if !case.matches(&actual) {
            failures.push(format!(
                "{}: {:?}\n  expected: {:?}, corrected: {:?}\n  actual: {actual:?}\n  reason: {:?}",
                case.id, case.input, case.expected, case.corrected, case.reason
            ));
        }
    }
    println!(
        "AI trigger corpus: {} cases (valid={}, zh={}, en={}, typo={}, guard={})",
        cases.len(),
        metrics.valid,
        metrics.chinese,
        cases
            .iter()
            .filter(|c| c.category == Category::EnNl)
            .count(),
        metrics.typos,
        metrics.destructive
    );
    metrics.report();
    failures.extend(metrics.violations().into_iter().map(str::to_string));
    first.assert_untouched();
    second.assert_untouched();
    assert_eq!(first.shell.snapshot(), first_before);
    assert_eq!(second.shell.snapshot(), second_before);
    assert!(
        failures.is_empty(),
        "Strict corpus failures:\n{}",
        failures.join("\n")
    );
}

#[test]
fn fixture_is_isolated_and_cleaned_up() {
    let path = std::env::var_os("PATH");
    let cwd = std::env::current_dir().unwrap();
    let root;
    {
        let mut fixture = Fixture::new();
        root = fixture.root.path().to_path_buf();
        let bin = root.join("bin");
        assert_eq!(fixture.shell.var("PATH").as_deref(), bin.to_str());
        assert_eq!(
            fs::canonicalize(fixture.shell.cwd()).unwrap(),
            fs::canonicalize(root.join("work")).unwrap()
        );
        for name in COMMANDS {
            assert_ne!(fixture.shell.resolve(name), Resolution::NotFound, "{name}");
        }
        for name in ["gti", "pyhton3", "sl", "ai", "bash"] {
            assert_eq!(fixture.shell.resolve(name), Resolution::NotFound, "{name}");
        }
        assert!(!fixture.shell.command_names().iter().any(|n| n == "bash"));
        assert_eq!(
            fixture.shell.resolve("git"),
            Resolution::File(bin.join("git"))
        );
        assert_eq!(
            classify(
                &format!("{} status", bin.join("git").display()),
                &mut fixture.shell,
                &TriggerConfig::default()
            ),
            Action::Execute
        );
        assert_eq!(std::env::var_os("PATH"), path);
        assert_eq!(std::env::current_dir().unwrap(), cwd);
    }
    assert!(!root.exists(), "fixture was not cleaned up");
}

#[test]
fn explicit_ai_and_incomplete_input_keep_their_contracts() {
    let mut fixture = Fixture::new();
    let cfg = TriggerConfig::default();
    for input in ["", " \t ", "#", " #  "] {
        assert_eq!(classify(input, &mut fixture.shell, &cfg), Action::Empty);
    }
    assert_eq!(
        classify(" # find big files ", &mut fixture.shell, &cfg),
        Action::Ai {
            trigger: Trigger::Hash,
            text: "find big files".into(),
        }
    );
    assert_eq!(
        classify("ai \"find files\"", &mut fixture.shell, &cfg),
        Action::AiBuiltin("\"find files\"".into())
    );
    for input in [
        "echo 'unfinished",
        "echo \"unfinished",
        "for item in alpha beta; do echo \"$item\"",
        "if true; then echo yes",
        "what's > output.txt",
    ] {
        assert_eq!(
            classify(input, &mut fixture.shell, &cfg),
            Action::Execute,
            "{input:?}"
        );
    }
    assert_eq!(
        classify("what's using port 8080", &mut fixture.shell, &cfg),
        Action::Ai {
            trigger: Trigger::ParseError,
            text: "what's using port 8080".into(),
        }
    );
}

#[test]
fn configuration_and_session_commands_take_precedence() {
    let mut fixture = Fixture::new();
    let disabled = TriggerConfig {
        ai_enabled: false,
        ..TriggerConfig::default()
    };
    for input in ["# comment", "gti status", "帮我找文件", "rm all temp files"] {
        assert_eq!(
            classify(input, &mut fixture.shell, &disabled),
            Action::Execute
        );
    }
    let no_guard = TriggerConfig {
        nl_guard: false,
        ..TriggerConfig::default()
    };
    assert_eq!(
        classify("rm all temp files", &mut fixture.shell, &no_guard),
        Action::Execute
    );
    let no_error = TriggerConfig {
        trigger_on_error: false,
        ..TriggerConfig::default()
    };
    for input in ["missing_unfixable_command --help", ")"] {
        assert_eq!(
            classify(input, &mut fixture.shell, &no_error),
            Action::Execute
        );
    }
    assert!(matches!(
        classify("gti status", &mut fixture.shell, &no_error),
        Action::Correct { .. }
    ));
    let custom = TriggerConfig {
        ai_prefix: "?".into(),
        builtin_name: "ask".into(),
        ..TriggerConfig::default()
    };
    assert_eq!(
        classify("? inspect logs", &mut fixture.shell, &custom),
        Action::Ai {
            trigger: Trigger::Hash,
            text: "inspect logs".into(),
        }
    );
    assert_eq!(
        classify("ask explain this", &mut fixture.shell, &custom),
        Action::AiBuiltin("explain this".into())
    );
    // Only fixture setup is executed, never an input from the corpus.
    assert_eq!(
        fixture
            .shell
            .run_user_line("alias ll='ls -l'; f() { :; }; ai() { :; }")
            .exit_code,
        0
    );
    for input in ["ll -a", "f", "ai explain this"] {
        assert_eq!(
            classify(input, &mut fixture.shell, &TriggerConfig::default()),
            Action::Execute,
            "{input}"
        );
    }
}

#[test]
fn questions_do_not_hide_real_commands_or_short_typos() {
    let mut fixture = Fixture::new();
    let cfg = TriggerConfig::default();
    let questions = [
        "why is the service slow",
        "whose socket is this",
        "can I remove the cache",
        "is the service running",
        "what process is using port 3000",
        "how many files are there",
        "echo ready && can you list files",
    ];
    for input in questions {
        assert_eq!(
            classify(input, &mut fixture.shell, &cfg),
            Action::Ai {
                trigger: Trigger::NotFound,
                text: input.into(),
            }
        );
    }
    for (input, expected) in [
        ("can README.md", "cat README.md"),
        ("is src output.txt", "ls src output.txt"),
        ("why -a", "who -a"),
        ("why alpha beta", "who alpha beta"),
        ("whose alpha beta", "who alpha beta"),
    ] {
        match classify(input, &mut fixture.shell, &cfg) {
            Action::Correct { corrected, .. } => assert_eq!(corrected, expected),
            actual => panic!("{input:?}: expected {expected:?}, got {actual:?}"),
        }
    }
    let no_error = TriggerConfig {
        trigger_on_error: false,
        ..cfg.clone()
    };
    assert_eq!(
        classify("can you list files", &mut fixture.shell, &no_error),
        Action::Execute
    );
    assert_eq!(
        fixture
            .shell
            .run_user_line("can() { :; }; alias why='who'")
            .exit_code,
        0
    );
    Fixture::executable(&fixture.root.path().join("bin").join("is"));
    for input in [
        "can you list files",
        "why is the service slow",
        "is the service running",
        "who is logged in",
        "whose() { :; }; whose socket is this",
    ] {
        assert_eq!(
            classify(input, &mut fixture.shell, &cfg),
            Action::Execute,
            "{input}"
        );
    }
}

#[test]
fn metric_thresholds_are_exact_and_reject_empty_populations() {
    assert_eq!(Metrics::default().violations().len(), 4);
    let mut metrics = Metrics {
        chinese: 10,
        chinese_ai: 10,
        valid: 201,
        false_guards: 1,
        typos: 10,
        corrected: 9,
        destructive: 10,
        executed: 0,
    };
    assert!(metrics.violations().is_empty());
    metrics.valid = 200;
    assert_eq!(metrics.violations().len(), 1, "0.5% must fail");
    metrics.false_guards = 0;
    metrics.corrected = 8;
    assert_eq!(metrics.violations().len(), 1, "80% must fail");
    metrics.corrected = 9;
    metrics.chinese_ai = 9;
    assert_eq!(metrics.violations().len(), 1, "one missed Chinese input");
    metrics.chinese_ai = 10;
    metrics.executed = 1;
    assert_eq!(metrics.violations().len(), 1, "one destructive execution");
}

#[test]
fn wrong_correction_is_a_miss_even_when_the_action_matches() {
    let case: Case = serde_json::from_str(
        r#"{"id":"counter-check","category":"typo","input":"gti status","expected":"correct","corrected":"git status"}"#,
    )
    .unwrap();
    let mut metrics = Metrics::default();
    let wrong = Action::Correct {
        corrected: "ls status".into(),
        from: "gti".into(),
        to: "ls".into(),
    };
    assert!(!case.matches(&wrong));
    metrics.record(&case, &wrong);
    assert_eq!(metrics.typos, 1);
    assert_eq!(metrics.corrected, 0);
}
