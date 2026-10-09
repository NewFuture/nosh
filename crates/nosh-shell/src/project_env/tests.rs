use super::*;

#[test]
fn strict_environment_operations_preserve_empty_and_unset() {
    let patch =
        mise_delta(b"set,PATH,/a:/b\nset,EMPTY,\nhide,REMOVE,\nset,TEXT,\"a,b\n\"\"quoted\"\"\"\n")
            .unwrap();
    assert_eq!(patch["EMPTY"], Some(String::new()));
    assert_eq!(patch["REMOVE"], None);
    assert_eq!(patch["TEXT"].as_deref(), Some("a,b\n\"quoted\""));
    for invalid in [
        "eval,KEY,command\n",
        "set,KEY\n",
        "hide,KEY,value\n",
        "set,BAD=NAME,value\n",
        "set,KEY,has\0nul\n",
        "set,PWD,/elsewhere\n",
        "set,PS1,$(unbounded-prompt-code)\n",
        "set,PROMPT_COMMAND,unexpected-code\n",
        "print hello\n",
        "set,KEY,\"unterminated\n",
        "set,KEY,\"closed\"extra\n",
        "set,KEY,unterminated",
    ] {
        assert!(mise_delta(invalid.as_bytes()).is_err(), "{invalid:?}");
    }
}

#[test]
fn defaults_never_activate_an_installed_tool() {
    assert_eq!(Provider::default(), Provider::Off);
    assert_eq!(Provider::parse("direnv"), Some(Provider::Direnv));
    assert_eq!(Provider::parse("mise"), Some(Provider::Mise));
    assert_eq!(Provider::parse("auto"), None);
    assert!(!Service::default().occupied());
}

#[test]
fn completed_refresh_does_not_time_out_while_the_user_edits() {
    let mut service = Service::new(Provider::Direnv);
    let thread = std::thread::spawn(|| ToolResult {
        program: PathBuf::from("/tool"),
        stamp: None,
        result: Ok(Patch::new()),
    });
    while !thread.is_finished() {
        std::thread::yield_now();
    }
    service.job = Some(Job {
        snapshot: Snapshot {
            generation: 0,
            cwd: "/project".into(),
            exported: BTreeMap::new(),
        },
        cancelled: Arc::new(AtomicBool::new(false)),
        verified: Arc::new(AtomicBool::new(true)),
        published: Arc::new(AtomicBool::new(true)),
        thread,
        started: Instant::now() - TIMEOUT - Duration::from_secs(1),
    });
    assert!(service.poll().unwrap().result.is_ok());
    assert!(service.error.is_none());
}

#[test]
fn a_panicked_worker_does_not_free_its_unverified_cleanup_slot() {
    let mut service = Service::new(Provider::Direnv);
    let snapshot = Snapshot {
        generation: 0,
        cwd: "/project".into(),
        exported: BTreeMap::new(),
    };
    let thread = std::thread::spawn(|| -> ToolResult { panic!("simulated worker failure") });
    while !thread.is_finished() {
        std::thread::yield_now();
    }
    service.job = Some(Job {
        snapshot: snapshot.clone(),
        cancelled: Arc::new(AtomicBool::new(false)),
        verified: Arc::new(AtomicBool::new(false)),
        published: Arc::new(AtomicBool::new(true)),
        thread,
        started: Instant::now(),
    });
    assert!(service.poll().unwrap().result.is_err());
    assert!(service.occupied() && service.job_cancelled());
    assert!(service.start(snapshot, "/replacement".into()).is_err());
}
