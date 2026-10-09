use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

use brush_core::sys::fs::PathExt;

use super::worker::{Kind, Worker};
use super::*;
use crate::{EmbeddedShell, ShellOptions, TriggerConfig};

static WORKER_TEST: Mutex<()> = Mutex::new(());

pub(super) struct Fixture {
    pub(super) shell: EmbeddedShell,
    root: tempfile::TempDir,
}

impl Fixture {
    pub(super) fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        for name in ["cat", "touch", "git", "grep"] {
            let path = bin.join(name);
            fs::write(&path, "#!/bin/sh\nexit 99\n").unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        }
        let mut shell = EmbeddedShell::new(ShellOptions {
            working_dir: Some(root.path().to_owned()),
            ..ShellOptions::default()
        })
        .unwrap();
        shell.run_user_line(&format!("PATH='{}'", bin.display()));
        Self { shell, root }
    }

    pub(super) fn context(&self) -> Context {
        self.shell
            .input_context(&TriggerConfig::default(), &Abbreviations::default())
            .unwrap()
    }

    pub(super) fn input(&self, text: &str) -> Input {
        Input {
            version: Version {
                input: 1,
                session: 1,
            },
            text: text.to_owned(),
            context: Arc::new(self.context()),
        }
    }
}

fn role_at(analysis: &Analysis, offset: usize) -> Role {
    analysis
        .spans
        .iter()
        .find(|s| s.range.contains(&offset))
        .map_or(Role::Text, |s| s.role)
}

#[test]
fn syntax_spans_use_original_utf8_bytes() {
    let fixture = Fixture::new();
    let text = "echo 中文 'e\u{301} 👩\u{200d}💻' \"$HOME\" # comment";
    let input = fixture.input(text);
    let result = analysis::analyze(&input);
    assert_eq!(role_at(&result, 0), Role::Builtin);
    assert_eq!(
        role_at(&result, text.find("e\u{301}").unwrap()),
        Role::String
    );
    assert_eq!(
        role_at(&result, text.find("$HOME").unwrap()),
        Role::Variable
    );
    assert_eq!(role_at(&result, text.find('#').unwrap()), Role::Comment);
    let reconstructed: String = result
        .spans
        .iter()
        .map(|span| text.get(span.range.clone()).unwrap())
        .collect();
    assert_eq!(reconstructed, text);
}

#[test]
fn incomplete_input_keeps_real_prefixes_and_reasons() {
    let fixture = Fixture::new();
    for text in [
        "echo 'hello",
        "echo \"hello",
        "echo $(printf x",
        "echo \"$(printf x",
        "echo x |",
        "for x in one; do\necho \"$x\"",
    ] {
        let result = analysis::analyze(&fixture.input(text));
        assert!(
            result.findings.iter().any(|f| f.state == State::Incomplete),
            "{text:?}: {:?}",
            result.findings
        );
        assert!(!result.findings.iter().any(|f| f.state == State::Error));
        if text.starts_with("echo") {
            assert_eq!(role_at(&result, 0), Role::Builtin, "{text:?}");
        }
    }
    let result = analysis::analyze(&fixture.input("echo foo\"bar"));
    assert!(!result.queries.iter().any(|q| q.word == "foo"));
    let result = analysis::analyze(&fixture.input("echo \"not # a comment"));
    assert_ne!(role_at(&result, "echo \"not ".len()), Role::Comment);
    let result = analysis::analyze(&fixture.input("echo x | | cat"));
    assert!(result.findings.iter().any(|f| f.state == State::Error));
}

#[test]
fn query_token_and_span_limits_are_explicit_not_clean_results() {
    let fixture = Fixture::new();
    let result =
        analysis::analyze(&fixture.input(&format!("echo {}", "word ".repeat(MAX_QUERIES + 1))));
    assert_eq!(result.queries.len(), MAX_QUERIES);
    assert!(
        result
            .findings
            .iter()
            .any(|f| matches!(f.reason, Reason::Limit))
    );

    let result = analysis::analyze(&fixture.input(&"echo ".repeat(MAX_NODES + 1)));
    assert!(
        result
            .findings
            .iter()
            .any(|f| f.state == State::Unavailable)
    );
    assert!(result.queries.is_empty());

    let text = format!("echo \"{}\"", ".$x".repeat(2200));
    let result = analysis::analyze(&fixture.input(&text));
    assert_eq!(result.spans.len(), 1);
    assert_eq!(result.spans[0].range, 0..text.len());
    assert!(
        result
            .findings
            .iter()
            .any(|f| matches!(f.reason, Reason::Limit))
    );
}

#[test]
fn ai_configuration_and_abbreviations_do_not_execute_or_rewrite() {
    let fixture = Fixture::new();
    let mut input = fixture.input("? what's using this port");
    Arc::make_mut(&mut input.context).ai_prefix = "?".into();
    let result = analysis::analyze(&input);
    assert_eq!(role_at(&result, 0), Role::Ai);
    assert!(result.queries.is_empty());
    Arc::make_mut(&mut input.context).ai_enabled = false;
    input.text = "# ordinary shell comment".into();
    let result = analysis::analyze(&input);
    assert_eq!(role_at(&result, 0), Role::Comment);
    assert!(
        !result
            .findings
            .iter()
            .any(|f| matches!(f.reason, Reason::Ai))
    );

    input.text = "gst".into();
    Arc::make_mut(&mut input.context)
        .abbreviations
        .applicable
        .insert("gst".into());
    let result = analysis::analyze(&input);
    assert_eq!(role_at(&result, 0), Role::Abbreviation);
    assert!(result.queries.is_empty());
    assert_eq!(input.text, "gst");
    Arc::make_mut(&mut input.context)
        .abbreviations
        .applicable
        .clear();
    let result = analysis::analyze(&input);
    assert!(result.queries.iter().any(|q| q.word == "gst"));
}

#[test]
fn ai_is_an_ordinary_shell_command() {
    let fixture = Fixture::new();
    let mut input = fixture.input("ai explain \"unfinished");
    let result = analysis::analyze(&input);
    assert!(
        result
            .findings
            .iter()
            .any(|finding| finding.state == State::Incomplete)
    );
    assert!(
        !result
            .findings
            .iter()
            .any(|finding| matches!(finding.reason, Reason::Ai))
    );
    let path = fixture.root.path().join("bin/ai");
    fs::write(&path, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    input.text = "ai explain".into();
    let result = analysis::analyze(&input);
    let observations = lookup::Lookup::default().run(&input, &result.queries);
    assert!(
        observations
            .iter()
            .any(|observation| observation.role == Some(Role::External))
    );
    assert!(
        !observations
            .iter()
            .any(|observation| observation.role == Some(Role::Ai))
    );
}

#[test]
fn inline_command_feedback_is_local_and_uses_the_configured_prefix() {
    let fixture = Fixture::new();
    for text in [
        "#",
        "#help",
        "#mode auto",
        "#think off",
        "#auto on",
        "#out 1",
        "#clear",
        "#ctx",
        "#status",
        "#fix what's \"wrong",
    ] {
        let result = analysis::analyze(&fixture.input(text));
        assert!(result.queries.is_empty(), "{text}");
        assert!(
            !result
                .findings
                .iter()
                .any(|finding| finding.state == State::Error),
            "{text}"
        );
        assert_eq!(role_at(&result, 0), Role::Builtin, "{text}");
    }
    let mut input = fixture.input("  问mode bad");
    Arc::make_mut(&mut input.context).ai_prefix = "问".into();
    let result = analysis::analyze(&input);
    assert!(result.queries.is_empty());
    assert_eq!(
        role_at(&result, input.text.find("bad").unwrap()),
        Role::Error
    );
    assert!(result.findings.iter().any(|finding| {
        matches!(&finding.reason, Reason::Syntax(message) if message.contains("问mode"))
    }));
    input.text = format!("问{}", "字".repeat(2000));
    let result = analysis::analyze(&input);
    assert!(
        result
            .findings
            .iter()
            .all(|finding| finding.reason.bounded())
    );
}

#[test]
fn inert_builtins_preserve_facts_but_overrides_expansions_and_traps_do_not() {
    let mut fixture = Fixture::new();
    for text in [
        "true; cat < missing",
        "false; cat < missing",
        ":; cat < missing",
    ] {
        let input = fixture.input(text);
        let analysis = analysis::analyze(&input);
        let result = lookup::Lookup::default().run(&input, &analysis.queries);
        assert!(
            result.iter().any(|o| o.finding.as_ref().is_some_and(|f| {
                f.state == State::Error && matches!(f.reason, Reason::MissingPath)
            })),
            "{text}: {result:?}"
        );
    }
    for text in [
        "touch missing; cat < missing",
        "true > missing; cat < missing",
        "true ${value:-$(touch missing)}; cat < missing",
        "true() { touch missing; }; true; cat < missing",
    ] {
        let input = fixture.input(text);
        let analysis = analysis::analyze(&input);
        let result = lookup::Lookup::default().run(&input, &analysis.queries);
        assert!(
            !result
                .iter()
                .any(|o| o.finding.as_ref().is_some_and(|f| f.state == State::Error)),
            "{text}: {result:?}"
        );
    }
    fixture.shell.run_user_line("trap 'PATH=/runtime' DEBUG");
    let input = fixture.input("true; missing");
    assert!(input.context.command_traps);
    let result = analysis::analyze(&input);
    assert!(result.findings.iter().any(|f| f.state == State::Unknown));
    assert!(result.queries.is_empty());
}

#[test]
fn submission_uses_the_same_static_scope_and_path_facts() {
    let mut fixture = Fixture::new();
    let new_bin = fixture.root.path().join("custom");
    fs::create_dir(&new_bin).unwrap();
    let executable = new_bin.join("custom_command");
    fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    let text = format!("PATH='{}' custom_command", new_bin.display());
    let input = fixture.input(&text);
    let result = analysis::analyze(&input);
    assert!(
        lookup::Lookup::default()
            .run(&input, &result.queries)
            .iter()
            .any(|o| o.role == Some(Role::External))
    );
    assert_eq!(
        crate::trigger::classify(&text, &mut fixture.shell, &TriggerConfig::default()),
        crate::trigger::Action::Execute
    );

    for text in [
        "local_function() { :; }; local_function",
        "if true; then local_function() { :; }; fi; local_function",
        "true && PATH=/runtime; gti status",
        "source setup; gti status",
        "touch custom_command; custom_command",
        "for PATH in /runtime; do custom_command; done",
    ] {
        assert_eq!(
            crate::trigger::classify(text, &mut fixture.shell, &TriggerConfig::default()),
            crate::trigger::Action::Execute,
            "{text}"
        );
    }
    let text = "not_yet_defined; not_yet_defined() { :; }";
    assert!(matches!(
        crate::trigger::classify(text, &mut fixture.shell, &TriggerConfig::default()),
        crate::trigger::Action::Ai { .. }
    ));
    let text = "(local_function() { :; }); local_function";
    assert!(matches!(
        crate::trigger::classify(text, &mut fixture.shell, &TriggerConfig::default()),
        crate::trigger::Action::Ai { .. }
    ));
}

#[test]
fn command_scope_and_temporary_path_are_not_global_index_guesses() {
    let mut fixture = Fixture::new();
    fixture
        .shell
        .run_user_line("shopt -s expand_aliases; alias ll='echo'; session_fn() { :; }");
    for (text, role) in [("ll", Role::Alias), ("session_fn", Role::Function)] {
        let result = analysis::analyze(&fixture.input(text));
        assert_eq!(role_at(&result, 0), role, "{text}");
    }
    let text = "f() { echo hi; }; f";
    let result = analysis::analyze(&fixture.input(text));
    assert_eq!(role_at(&result, text.len() - 1), Role::Function);
    let result = analysis::analyze(&fixture.input("f; f() { :; }"));
    assert!(result.queries.iter().any(|q| q.word == "f"));
    let result = analysis::analyze(&fixture.input("cd() { echo \"$@\"; }; cd missing"));
    assert!(
        !result
            .queries
            .iter()
            .any(|q| matches!(q.kind, QueryKind::Path(PathUse::Directory)))
    );
    let result = analysis::analyze(&fixture.input("PATH=$NEW echo ok"));
    assert_eq!(role_at(&result, "PATH=$NEW ".len()), Role::Builtin);

    let result = analysis::analyze(&fixture.input("PATH=/different unknown_tool"));
    assert!(result.queries.iter().any(|q| {
        matches!(&q.kind, QueryKind::Command { path, .. } if path.as_deref() == Some("/different"))
    }));
    for text in [
        "$CMD",
        "tool*",
        "PATH=$NEW tool",
        "source setup; later_command",
    ] {
        let result = analysis::analyze(&fixture.input(text));
        assert!(
            result.findings.iter().any(|f| f.state == State::Unknown),
            "{text}: {:?}",
            result.findings
        );
    }
}

#[test]
fn path_queries_preserve_creation_and_snapshot_boundaries() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("README"), "exists").unwrap();
    let mut lookup = lookup::Lookup::default();
    for text in ["echo hello", "echo hello > new.txt"] {
        let input = fixture.input(text);
        let analysis = analysis::analyze(&input);
        let results = lookup.run(&input, &analysis.queries);
        assert!(
            !results
                .iter()
                .any(|r| { r.finding.as_ref().is_some_and(|f| f.state == State::Error) }),
            "{text}: {results:?}"
        );
    }
    let input = fixture.input("cat README");
    let analysis = analysis::analyze(&input);
    let results = lookup.run(&input, &analysis.queries);
    assert!(results.iter().any(|r| r.role == Some(Role::Path)));

    let input = fixture.input("cat < missing");
    let analysis = analysis::analyze(&input);
    assert!(
        lookup
            .run(&input, &analysis.queries)
            .iter()
            .any(|r| { r.finding.as_ref().is_some_and(|f| f.state == State::Error) })
    );
    let input = fixture.input("echo '$(touch missing)'; cat < missing");
    let analysis = analysis::analyze(&input);
    assert!(
        lookup
            .run(&input, &analysis.queries)
            .iter()
            .any(|r| { r.finding.as_ref().is_some_and(|f| f.state == State::Error) })
    );
    let blocked = fixture.root.path().join("blocked-out");
    fs::create_dir(&blocked).unwrap();
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o0)).unwrap();
    if !blocked.executable() {
        let input = fixture.input(&format!("echo x > {}/new", blocked.display()));
        let analysis = analysis::analyze(&input);
        assert!(lookup.run(&input, &analysis.queries).iter().any(|r| {
            r.finding.as_ref().is_some_and(|f| {
                f.state == State::Error && matches!(f.reason, Reason::AccessDenied(_))
            })
        }));
    }
    fs::set_permissions(&blocked, fs::Permissions::from_mode(0o700)).unwrap();
    let input = fixture.input("touch missing; cat < missing");
    let analysis = analysis::analyze(&input);
    let results = lookup.run(&input, &analysis.queries);
    assert!(
        !results
            .iter()
            .any(|r| { r.finding.as_ref().is_some_and(|f| f.state == State::Error) })
    );
    assert!(results.iter().any(|r| {
        r.finding
            .as_ref()
            .is_some_and(|f| matches!(f.reason, Reason::Snapshot))
    }));
    let input = fixture.input("echo x > absent_parent/file");
    let analysis = analysis::analyze(&input);
    assert!(
        lookup
            .run(&input, &analysis.queries)
            .iter()
            .any(|r| { r.finding.as_ref().is_some_and(|f| f.state == State::Error) })
    );
}

#[test]
fn unset_path_is_not_an_empty_path_search() {
    let mut fixture = Fixture::new();
    let executable = fixture.root.path().join("local_tool");
    fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
    fixture.shell.run_user_line("unset PATH");
    let input = fixture.input("local_tool");
    assert!(input.context.path.is_none());
    let analysis = analysis::analyze(&input);
    let observations = lookup::Lookup::default().run(&input, &analysis.queries);
    assert!(
        !observations.iter().any(|o| o.role == Some(Role::External)),
        "{observations:?}"
    );
}

#[test]
fn non_definite_successes_are_snapshot_unknowns() {
    let fixture = Fixture::new();
    fs::write(fixture.root.path().join("README"), "data").unwrap();
    let mut lookup = lookup::Lookup::default();
    for text in ["rm README; cat README", "rm $(which cat); cat README"] {
        let input = fixture.input(text);
        let analysis = analysis::analyze(&input);
        let observations = lookup.run(&input, &analysis.queries);
        assert!(
            observations.iter().any(|o| {
                o.finding.as_ref().is_some_and(|f| {
                    f.state == State::Unknown && matches!(f.reason, Reason::Snapshot)
                })
            }),
            "{text}: {observations:?}"
        );
    }
}

#[test]
fn review_redirection_creation_is_not_a_later_missing_file_error() {
    let fixture = Fixture::new();
    for text in [
        "> input; cat < input",
        "VAR=1 > input; cat < input",
        "[[ 1 == 1 ]] > input; cat < input",
        "cat > input < input",
        "{ cat < input; } > input",
        "VAR=${unset:-$(touch input)}; cat < input",
        "[[ $(touch input) ]]; cat < input",
    ] {
        let input = fixture.input(text);
        let analysis = analysis::analyze(&input);
        let observations = lookup::Lookup::default().run(&input, &analysis.queries);
        assert!(
            !observations
                .iter()
                .any(|o| o.finding.as_ref().is_some_and(|f| f.state == State::Error)),
            "{text}: {observations:?}"
        );
    }
    let input = fixture.input("cat < missing > missing");
    let analysis = analysis::analyze(&input);
    assert!(
        lookup::Lookup::default()
            .run(&input, &analysis.queries)
            .iter()
            .any(|o| { o.finding.as_ref().is_some_and(|f| f.state == State::Error) })
    );
    for text in ["cat < ''", "cd ''", "echo > ''"] {
        let result = analysis::analyze(&fixture.input(text));
        assert!(
            result.findings.iter().any(|f| f.state == State::Error),
            "{text}"
        );
    }
}

#[test]
fn review_optional_path_changes_do_not_use_stale_resolution() {
    let fixture = Fixture::new();
    for text in [
        "true && PATH=/other; git status",
        "false || PATH=/other; git status",
        "if true; then PATH=/other; fi; git status",
        ": | PATH=/other; git status",
    ] {
        let result = analysis::analyze(&fixture.input(text));
        assert!(
            !result.queries.iter().any(|q| q.word == "git"),
            "{text}: {:?}",
            result.queries
        );
        assert!(
            result
                .findings
                .iter()
                .any(|f| { f.state == State::Unknown && text.get(f.range.clone()) == Some("git") })
        );
    }
}

#[test]
fn review_executable_status_matches_the_shell_access_check() {
    use brush_core::sys::fs::PathExt;

    let fixture = Fixture::new();
    let executable = fixture.root.path().join("bin/not_for_owner");
    fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o001)).unwrap();
    let input = fixture.input("./bin/not_for_owner");
    let analysis = analysis::analyze(&input);
    let result = lookup::Lookup::default().run(&input, &analysis.queries);
    assert_eq!(
        result[0].role == Some(Role::External),
        executable.executable()
    );
}

#[test]
fn an_external_ai_command_gets_full_shell_diagnostics_after_header_resolution() {
    let fixture = Fixture::new();
    let path = fixture.root.path().join("bin/ai");
    fs::write(&path, "#!/bin/sh\nexit 99\n").unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let assist = InputAssist::new(
        Some(launcher("input_assist::tests::worker_probe")),
        fixture.shell.input_index(),
        Arc::new(|| {}),
        80,
    );
    assist.prepare(Ok(fixture.context()));
    let highlighter = assist.highlighter();
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let styled = highlighter.highlight("ai < missing", 12);
        assert_eq!(styled.raw_string(), "ai < missing");
        if styled.buffer.iter().any(|(style, text)| {
            text == "missing" && style.foreground == Some(nu_ansi_term::Color::Red)
        }) {
            break;
        }
        assert!(Instant::now() < deadline, "{}", assist.status());
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn analysis_never_runs_substitutions_or_completion_functions() {
    let fixture = Fixture::new();
    let input =
        fixture.input("echo \"$(printf x)\"; cat <(touch marker); echo ${x:-$(touch marker)}");
    let result = analysis::analyze(&input);
    let mut lookup = lookup::Lookup::default();
    lookup.run(&input, &result.queries);
    assert!(!fixture.root.path().join("marker").exists());
    assert!(result.spans.iter().any(|s| s.role == Role::Builtin));
}

#[test]
fn snapshots_do_not_wait_for_the_shell_or_evaluate_dynamic_values() {
    let fixture = Fixture::new();
    let (_, shell) = fixture.shell.shared();
    let guard = shell.lock().unwrap();
    let before = Instant::now();
    assert!(
        fixture
            .shell
            .input_context(&TriggerConfig::default(), &Abbreviations::default())
            .is_err()
    );
    assert!(before.elapsed() < Duration::from_millis(100));
    drop(guard);
    assert!(fixture.context().builtins.contains("echo"));
}

pub(super) fn launcher(probe: &str) -> WorkerCommand {
    WorkerCommand {
        program: std::env::current_exe().unwrap(),
        args: vec!["--exact".into(), probe.into(), "--nocapture".into()],
    }
}

#[test]
fn worker_probe() {
    if let Some(code) = run_worker_from_env() {
        std::process::exit(code);
    }
}

#[test]
fn allocator_worker_probe() {
    if std::env::var("NOSH_INPUT_WORKER").is_err() {
        return;
    }
    assert!(allocation::enable());
    let mut too_large = Vec::<u8>::new();
    assert!(too_large.try_reserve_exact(256 * 1024 * 1024).is_err());
    let before = allocation::remaining();
    for _ in 0..70 {
        std::hint::black_box(vec![0_u8; 1024 * 1024]);
    }
    assert!(allocation::remaining() < before - 64 * 1024 * 1024);
    assert!(allocation::recycle());
    if let Some(code) = run_worker_from_env() {
        std::process::exit(code);
    }
}

#[test]
fn allocation_budget_is_not_refunded_and_recycling_is_explicit() {
    let _guard = WORKER_TEST.lock().unwrap();
    let fixture = Fixture::new();
    let launch = launcher("input_assist::tests::allocator_worker_probe");
    let mut worker = Worker::spawn(&launch, Kind::Syntax).unwrap();
    worker
        .start(&Request::Analyze(Arc::new(fixture.input("echo ok"))))
        .unwrap();
    assert!(matches!(
        await_response(&mut worker).unwrap(),
        Response::Analysis(_)
    ));
    assert!(
        worker.stopping,
        "a recycling worker must retain its slot until reaped"
    );
    stop_worker(&mut worker);
}

fn await_response(worker: &mut Worker) -> std::io::Result<Response> {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if let Some(response) = worker.poll()? {
            return Ok(response);
        }
        assert!(Instant::now() < deadline, "worker test watchdog");
        thread::sleep(Duration::from_millis(2));
    }
}

fn stop_worker(worker: &mut Worker) {
    worker.stop().unwrap();
    let deadline = Instant::now() + Duration::from_secs(3);
    while !worker.reaped().unwrap() {
        assert!(Instant::now() < deadline, "worker cleanup watchdog");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn owned_worker_roundtrip_and_hostile_parser_isolation() {
    let _guard = WORKER_TEST.lock().unwrap();
    let fixture = Fixture::new();
    let launch = launcher("input_assist::tests::worker_probe");
    let mut worker = Worker::spawn(&launch, Kind::Syntax).unwrap();
    worker
        .start(&Request::Analyze(Arc::new(
            fixture.input("echo \"$(printf x)\""),
        )))
        .unwrap();
    let Response::Analysis(result) = await_response(&mut worker).unwrap() else {
        panic!("unexpected response");
    };
    assert_eq!(role_at(&result, 0), Role::Builtin);
    worker
        .start(&Request::Analyze(Arc::new(fixture.input("# <<E$[\t\t"))))
        .unwrap();
    let Response::Analysis(result) = await_response(&mut worker).unwrap() else {
        panic!("AI input must not be interpreted as a shell program");
    };
    assert_eq!(role_at(&result, 0), Role::Ai);
    assert!(result.queries.is_empty());
    stop_worker(&mut worker);

    // Never pass this known non-progress case to an in-process parser.
    let mut worker = Worker::spawn(&launch, Kind::Syntax).unwrap();
    worker
        .start(&Request::Analyze(Arc::new(fixture.input("<<E$[\t\t"))))
        .unwrap();
    let start = Instant::now();
    let result = await_response(&mut worker);
    if let Ok(response) = result {
        let Response::Analysis(analysis) = response else {
            panic!("unexpected response");
        };
        assert!(
            analysis.findings.iter().any(|f| matches!(
                f.state,
                State::Error | State::Incomplete | State::Unavailable
            )),
            "malformed input must not turn into a success-shaped fallback"
        );
    }
    assert!(start.elapsed() < Duration::from_secs(2));
    stop_worker(&mut worker);
}
