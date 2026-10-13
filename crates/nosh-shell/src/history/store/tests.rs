use super::*;
use brush_core::history::Observer;
use nosh_platform::tr;
use std::time::Instant;

fn query(store: &Store, text: &str, worktree: Option<String>) -> Page {
    page(
        store,
        Query {
            text: text.into(),
            worktree,
            before: None,
            include_anchor: false,
            older: true,
            snapshot: None,
        },
    )
}

fn page(store: &Store, query: Query) -> Page {
    let generation = store.query(query);
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(page) = store.poll(generation) {
            return page;
        }
        assert!(Instant::now() < until, "history query did not complete");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn shell(file: &Path, cwd: &Path) -> Store {
    let store = Store::new(Some(file), None, None);
    store.context(cwd, true);
    store.sync().unwrap();
    store
}

fn executed(store: &Store, text: &str, exit_code: i32) -> i64 {
    let id = store.record(text, Kind::Command).unwrap();
    store.execution_started(id).unwrap().unwrap()(exit_code).unwrap();
    store.sync().unwrap();
    id
}

#[test]
fn exit_codes_persist_and_mixed_results_remain_one_repeat_group() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    for code in [0, 1, 2] {
        executed(&store, "same command", code);
    }
    store.record("same command", Kind::Command);
    store.sync().unwrap();
    let result = query(&store, "", None);
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].repeat_count(), 4);
    assert_eq!(
        result.entries[0].outcomes(),
        Outcomes {
            succeeded: 1,
            failed: 2,
            unknown: 1
        }
    );
    let connection = Connection::open(database_path(&file)).unwrap();
    let mut statement = connection
        .prepare("SELECT exit_code FROM commands ORDER BY seq")
        .unwrap();
    let codes = statement
        .query_map([], |row| row.get::<_, Option<i32>>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(codes, [Some(0), Some(1), Some(2), None]);
    let reopened = shell(&file, root.path());
    let restored = query(&reopened, "", None);
    assert_eq!(restored.entries[0].outcomes(), result.entries[0].outcomes());
}

#[test]
fn outcome_labels_only_repeat_counts_for_mixed_results() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    executed(&store, "compact results", 0);
    let mut entry = query(&store, "", None).entries.remove(0);
    for (code, expected) in [
        (Some(0), tr!("成功", "ok")),
        (Some(7), tr!("失败(7)", "failed(7)")),
        (None, tr!("未知", "unknown")),
    ] {
        entry.exit_code = code;
        assert_eq!(crate::history::outcome_label(&entry), expected);
    }
    for (succeeded, failed, unknown, expected) in [
        (4, 0, 0, tr!("成功", "ok")),
        (0, 4, 0, tr!("失败", "failed")),
        (0, 0, 4, tr!("未知", "unknown")),
        (1, 2, 1, tr!("失败2 未知1 成功1", "fail:2 unknown:1 ok:1")),
        (3, 1, 0, tr!("失败1 成功3", "fail:1 ok:3")),
    ] {
        entry.grouped_outcomes = Some(Outcomes {
            succeeded,
            failed,
            unknown,
        });
        assert_eq!(crate::history::outcome_label(&entry), expected);
    }
}

#[test]
fn panel_rows_only_annotate_exceptional_kinds_and_locations() {
    use crate::history::Panel;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let store = shell(&root.path().join("history"), root.path());
    executed(&store, "echo compact", 0);
    let entry = query(&store, "", None).entries.remove(0);
    let worktree = store.scope().unwrap();
    let scope = Ok(worktree.as_deref());
    let command = Panel::entry(entry.clone(), scope, true);
    assert_eq!(command.label, tr!("成功", "ok"));
    assert!(command.source.is_empty());
    assert!(Panel::entry(entry.clone(), scope, false).label.is_empty());
    assert!(
        command
            .description
            .as_ref()
            .is_some_and(|description| description.contains(tr!("项目根目录", "project root")))
    );
    let mut earlier = entry.clone();
    earlier.session = None;
    assert_eq!(Panel::entry(earlier, scope, true).label, tr!("成功", "ok"));
    for (kind, expected) in [
        (Kind::Manual, tr!("未知 手动", "unknown manual")),
        (Kind::Question, tr!("提问·内存", "question: memory")),
        (Kind::SuggestionRequest, tr!("建议·内存", "suggest: memory")),
    ] {
        let mut exceptional = entry.clone();
        exceptional.kind = kind;
        exceptional.exit_code = None;
        assert_eq!(Panel::entry(exceptional, scope, true).label, expected);
    }
    let mut other = entry.clone();
    other.worktree = Some(root.path().join("other").to_string_lossy().into_owned());
    assert!(
        Panel::entry(other, scope, true)
            .source
            .starts_with(tr!("其它:", "other:"))
    );
    let mut unknown = entry;
    unknown.worktree = None;
    unknown.cwd = None;
    assert_eq!(
        Panel::entry(unknown, scope, true).source,
        tr!("来源未知", "unknown location")
    );
}

#[test]
fn native_history_exposes_known_exit_codes_without_treating_unknown_as_failure() {
    use reedline::History;
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    let success = executed(&store, "success", 0);
    executed(&store, "failure", 7);
    store.record("unknown", Kind::Command);
    store.sync().unwrap();
    let history = crate::history::ShellHistory { store };
    assert_eq!(
        history
            .load(reedline::HistoryItemId(success))
            .unwrap()
            .exit_status,
        Some(0)
    );
    for (successful, expected) in [(true, "success"), (false, "failure")] {
        let mut request =
            reedline::SearchQuery::everything(reedline::SearchDirection::Backward, None);
        request.filter.exit_successful = Some(successful);
        let found = history.search(request).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].command_line, expected);
    }
}

#[test]
fn outcome_callbacks_keep_identity_after_eviction_and_do_not_recreate_or_relabel_records() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    let deleted = store.record("deleted command", Kind::Command).unwrap();
    let completion = store.execution_started(deleted).unwrap().unwrap();
    store.removed(deleted).unwrap();
    completion(0).unwrap();
    store.sync().unwrap();
    assert!(query(&store, "", None).entries.is_empty());
    let changed = executed(&store, "original command", 0);
    let completion = store.execution_started(changed).unwrap().unwrap();
    store
        .updated(&brush_core::history::Item {
            id: changed,
            command_line: "rewritten command".into(),
            timestamp: None,
            dirty: true,
        })
        .unwrap();
    completion(9).unwrap();
    store.sync().unwrap();
    assert_eq!(query(&store, "rewritten", None).entries[0].exit_code, None);
    let evicted = store.record("evicted command", Kind::Command).unwrap();
    store.sync().unwrap();
    let completion = store.execution_started(evicted).unwrap().unwrap();
    lock(&store.0.model)
        .commands
        .retain(|entry| entry.legacy_id != evicted);
    completion(3).unwrap();
    store.sync().unwrap();
    assert_eq!(query(&store, "evicted", None).entries[0].exit_code, Some(3));
}

#[test]
fn execution_outcomes_preserve_memory_on_write_failure_and_reject_invalid_codes() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    let id = store.record("executed command", Kind::Command).unwrap();
    store.sync().unwrap();
    let completion = store.execution_started(id).unwrap().unwrap();
    let writer = Connection::open(database_path(&file)).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    completion(9).unwrap();
    assert!(store.sync().is_err());
    let result = query(&store, "executed", None);
    assert_eq!(result.entries[0].exit_code, Some(9));
    assert!(result.notice.is_some());
    writer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        writer
            .query_row("SELECT exit_code FROM commands", [], |row| row
                .get::<_, Option<i32>>(0))
            .unwrap(),
        None
    );
    let invalid = store.execution_started(id).unwrap().unwrap();
    assert!(invalid(300).is_err());
    assert_eq!(store.cached()[0].exit_code, Some(9));
    store.refreshed().unwrap();
    assert_eq!(
        store.cached()[0].exit_code,
        Some(9),
        "refresh must not discard a known result after its database write failed"
    );
    writer
        .execute("UPDATE commands SET exit_code=0", [])
        .unwrap();
    store.refreshed().unwrap();
    assert_eq!(store.cached()[0].exit_code, Some(0));
    writer
        .execute(
            "UPDATE commands SET command='changed command',exit_code=NULL",
            [],
        )
        .unwrap();
    store.refreshed().unwrap();
    assert_eq!(store.cached()[0].text, "changed command");
    assert_eq!(store.cached()[0].exit_code, None);
}

#[test]
fn incompatible_databases_are_rejected_without_modification() {
    for version in [2, 3] {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("history.sqlite3");
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "CREATE TABLE untouched(value TEXT); INSERT INTO untouched VALUES ('keep');",
            )
            .unwrap();
        connection
            .pragma_update(None, "user_version", version)
            .unwrap();
        drop(connection);
        let original = std::fs::read(&path).unwrap();
        let error = open_database(&path).unwrap_err();
        assert_eq!(
            error,
            format!("unsupported history database version {version}")
        );
        assert_eq!(std::fs::read(path).unwrap(), original);
    }
}

#[test]
fn execution_results_do_not_cross_changed_history_destinations() {
    let root = tempfile::tempdir().unwrap();
    let first = root.path().join("first");
    let second = root.path().join("second");
    let store = shell(&first, root.path());
    let id = store.record("original destination", Kind::Command).unwrap();
    store.sync().unwrap();
    let completion = store.execution_started(id).unwrap().unwrap();
    store.configure(Some(&second), None, None).unwrap();
    completion(0).unwrap();
    assert!(store.sync().is_err());
    let old = Connection::open(database_path(&first)).unwrap();
    let new = Connection::open(database_path(&second)).unwrap();
    assert_eq!(
        old.query_row("SELECT exit_code FROM commands", [], |row| row
            .get::<_, Option<i32>>(0))
            .unwrap(),
        None
    );
    assert_eq!(
        new.query_row("SELECT count(*) FROM commands", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn native_queries_use_record_order_after_refresh_without_losing_attempts() {
    use reedline::History;
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let first = shell(&file, root.path());
    first.record("old command", Kind::Command);
    first.sync().unwrap();
    let other = shell(&file, root.path());
    other.record("middle command", Kind::Command);
    other.sync().unwrap();
    first.record("new command", Kind::Command);
    first.record("new command", Kind::Command);
    first.record("# not a command", Kind::Question);
    first.sync().unwrap();
    first.refreshed().unwrap();
    let history = crate::history::ShellHistory {
        store: first.clone(),
    };
    let mut id = None;
    for expected in [
        "new command",
        "new command",
        "middle command",
        "old command",
    ] {
        let mut query =
            reedline::SearchQuery::everything(reedline::SearchDirection::Backward, None);
        query.start_id = id;
        query.limit = Some(1);
        let entry = history.search(query).unwrap().pop().unwrap();
        assert_eq!(entry.command_line, expected);
        id = entry.id;
    }
    for expected in ["middle command", "new command", "new command"] {
        let mut query = reedline::SearchQuery::everything(reedline::SearchDirection::Forward, None);
        query.start_id = id;
        query.limit = Some(1);
        let entry = history.search(query).unwrap().pop().unwrap();
        assert_eq!(entry.command_line, expected);
        id = entry.id;
    }
    let mut query = reedline::SearchQuery::everything(reedline::SearchDirection::Forward, None);
    query.start_id = id;
    assert!(history.search(query).unwrap().is_empty());
}

#[test]
fn a_removed_native_navigation_anchor_reports_change_without_closing_the_editor() {
    use reedline::History;
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    let id = store.record("removed command", Kind::Command).unwrap();
    store.sync().unwrap();
    let history = crate::history::ShellHistory {
        store: store.clone(),
    };
    store.removed(id).unwrap();
    let mut query = reedline::SearchQuery::everything(reedline::SearchDirection::Backward, None);
    query.start_id = Some(reedline::HistoryItemId::new(id));
    assert!(history.search(query).unwrap().is_empty());
    assert!(
        store
            .take_errors()
            .iter()
            .any(|error| error.contains("history changed"))
    );
}

#[test]
fn existing_text_history_is_never_read_or_modified() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let original = b"OLD_HISTORY_TEXT_2e896\n\xffunreadable\n";
    std::fs::write(&file, original).unwrap();
    let store = shell(&file, root.path());
    assert!(query(&store, "", None).entries.is_empty());
    executed(&store, "new command", 0);
    let reopened = shell(&file, root.path());
    let entries = query(&reopened, "", None).entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "new command");
    assert_eq!(std::fs::read(&file).unwrap(), original);
    for entry in std::fs::read_dir(root.path()).unwrap() {
        let entry = entry.unwrap();
        if entry.path() != file && entry.file_type().unwrap().is_file() {
            assert!(
                !std::fs::read(entry.path())
                    .unwrap()
                    .windows(b"OLD_HISTORY_TEXT_2e896".len())
                    .any(|bytes| bytes == b"OLD_HISTORY_TEXT_2e896")
            );
        }
    }
}

#[test]
fn changing_destination_and_retention_does_not_prune_the_previous_database() {
    const MARKER: &str = "UNREAD_HISTORY_c1b4";
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let store = shell(&a, root.path());
    store.record("keep previous database", Kind::Command);
    store.sync().unwrap();
    std::fs::write(&b, format!("printf {MARKER}\n")).unwrap();
    store.configure(Some(&b), None, Some("0")).unwrap();
    store.sync().unwrap();
    let first = Connection::open(database_path(&a)).unwrap();
    assert_eq!(
        first
            .query_row("SELECT count(*) FROM commands", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let next = Connection::open(database_path(&b)).unwrap();
    assert_eq!(
        next.query_row("SELECT count(*) FROM commands", [], |row| row
            .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert!(
        !std::fs::read(database_path(&b))
            .unwrap()
            .windows(MARKER.len())
            .any(|bytes| bytes == MARKER.as_bytes())
    );
}

#[test]
fn all_scope_reports_lookup_errors_without_claiming_current_records_are_other_projects() {
    use reedline::HistoryPanelProvider;
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".git")).unwrap();
    let store = shell(&root.path().join("history"), root.path());
    store.record("echo local project", Kind::Command);
    store.sync().unwrap();
    store.context(&root.path().join("missing"), true);
    let mut panel = crate::history::Panel::new(store);
    panel.begin();
    let result = panel.query(&reedline::HistoryPanelQuery {
        text: "echo local".into(),
        project: false,
        anchor: None,
        include_anchor: false,
        older: true,
    });
    assert!(
        result
            .notice
            .as_ref()
            .is_some_and(|notice| notice.contains("project history unavailable"))
    );
    assert_eq!(result.entries.len(), 1);
    assert!(result.entries[0].source.starts_with("worktree:"));
}

#[test]
fn commands_round_trip_but_questions_never_reach_database_files() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    let command = "printf '%s\\n' '中文' # comment\nprintf second";
    let question = "# PRIVATE_QUESTION_73d31f do not persist this";
    store.record(command, Kind::Command);
    store.record(question, Kind::Question);
    store.record(
        "PRIVATE_SUGGEST_14ad make a command",
        Kind::SuggestionRequest,
    );
    store.sync().unwrap();
    assert_eq!(query(&store, "", None).entries.len(), 3);
    let next = shell(&file, root.path());
    let entries = query(&next, "", None).entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, command);
    for entry in std::fs::read_dir(root.path()).unwrap() {
        let entry = entry.unwrap();
        if entry.file_type().unwrap().is_file() {
            let bytes = std::fs::read(entry.path()).unwrap();
            for marker in ["PRIVATE_QUESTION_73d31f", "PRIVATE_SUGGEST_14ad"] {
                assert!(
                    !bytes
                        .windows(marker.len())
                        .any(|window| window == marker.as_bytes())
                );
            }
        }
    }
}

#[test]
fn worktree_scope_includes_children_but_not_other_worktrees() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir_all(a.join("sub")).unwrap();
    std::fs::create_dir_all(&b).unwrap();
    std::fs::write(a.join(".git"), "gitdir: ../common/worktrees/a\n").unwrap();
    std::fs::write(b.join(".git"), "gitdir: ../common/worktrees/b\n").unwrap();
    let store = shell(&root.path().join("history"), &a);
    store.record("a root", Kind::Command);
    store.context(&a.join("sub"), true);
    store.record("a child", Kind::Command);
    store.record("# a question", Kind::Question);
    let scope_a = store.scope().unwrap();
    store.context(&b, true);
    store.record("b root", Kind::Command);
    store.sync().unwrap();
    let result = query(&store, "", scope_a);
    assert_eq!(
        result
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["a root", "a child", "# a question"]
    );
    assert_eq!(
        query(&store, "", store.scope().unwrap()).entries[0].text,
        "b root"
    );
}

#[test]
fn stale_sessions_do_not_resurrect_deleted_commands() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let first = shell(&file, root.path());
    first.record("remove me", Kind::Command);
    first.sync().unwrap();
    let stale = shell(&file, root.path());
    let id = first.cached()[0].legacy_id;
    first.removed(id).unwrap();
    stale.sync().unwrap();
    assert!(query(&stale, "", None).entries.is_empty());
    assert!(
        query(&shell(&file, root.path()), "", None)
            .entries
            .is_empty()
    );
}

#[test]
fn clear_preserves_commands_added_after_the_visible_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let first = shell(&file, root.path());
    first.record("old", Kind::Command);
    first.sync().unwrap();
    let other = shell(&file, root.path());
    other.record("new from another terminal", Kind::Command);
    other.sync().unwrap();
    first.record("# ephemeral", Kind::Question);
    first.cleared().unwrap();
    let result = query(&first, "", None);
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].text, "new from another terminal");
}

#[test]
fn recent_command_loading_preserves_raw_attempts_in_newest_first_order() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    for _ in 0..100 {
        store.record("same command", Kind::Command);
    }
    store.sync().unwrap();
    let connection = Connection::open(root.path().join("history.nosh.sqlite3")).unwrap();
    let (newest, snapshot) = recent_commands(&connection).unwrap();
    assert_eq!(store.cached().len(), 100);
    assert_eq!(newest.len(), 100);
    assert_eq!(snapshot, 100);
    assert_eq!(
        newest.iter().map(|entry| &entry.key).collect::<Vec<_>>(),
        store
            .cached()
            .iter()
            .rev()
            .map(|entry| &entry.key)
            .collect::<Vec<_>>(),
    );
}

#[test]
fn recent_command_loading_bounds_rows_and_bytes_without_truncating_inputs() {
    for (text, count, limit) in [
        ("short command".to_owned(), HOT_ITEMS + 1, HOT_ITEMS),
        (
            "x".repeat(INPUT_BYTES),
            HOT_BYTES / INPUT_BYTES + 1,
            HOT_BYTES / INPUT_BYTES,
        ),
    ] {
        let root = tempfile::tempdir().unwrap();
        let mut connection = open_database(&root.path().join("history.sqlite3")).unwrap();
        let transaction = connection.transaction().unwrap();
        for ordinal in 0..count {
            transaction
                .execute(
                    "INSERT INTO commands(id,ordinal,command,source) VALUES (?1,?2,?3,'command')",
                    params![ordinal.to_string(), ordinal as i64, text],
                )
                .unwrap();
        }
        transaction.commit().unwrap();
        let (entries, snapshot) = recent_commands(&connection).unwrap();
        assert_eq!(entries.len(), limit);
        assert_eq!(snapshot, count as i64);
        assert!(entries.iter().all(|entry| entry.text == text));
        assert_eq!(entries.first().unwrap().ordinal, (count - 1) as i64);
        assert_eq!(entries.last().unwrap().ordinal, (count - limit) as i64);
        connection
            .execute(
                "UPDATE commands SET command=?1 WHERE ordinal=?2",
                params!["x".repeat(INPUT_BYTES + 1), (count - 1) as i64],
            )
            .unwrap();
        assert!(recent_commands(&connection).is_err());
    }
}

#[test]
fn failed_database_loading_never_reports_a_successful_sync() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let connection = open_database(&database_path(&file)).unwrap();
    connection
        .execute(
            "INSERT INTO commands(id,ordinal,command,source) VALUES ('oversized',1,?1,'command')",
            ["x".repeat(INPUT_BYTES + 1)],
        )
        .unwrap();
    drop(connection);
    let store = Store::new(Some(&file), None, None);
    assert!(store.sync().is_err());
    let result = query(&store, "", None);
    assert!(result.failed);
    assert!(result.notice.is_some());
}

#[test]
fn consecutive_commands_fold_before_filtering_without_changing_raw_history() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    for command in [
        "echo same",
        "echo same",
        "printf gap",
        "echo same",
        "echo same",
    ] {
        store.record(command, Kind::Command);
    }
    store.sync().unwrap();
    let rows = query(&store, "", None).entries;
    assert_eq!(
        rows.iter()
            .map(|entry| (entry.text.as_str(), entry.repeat_count()))
            .collect::<Vec<_>>(),
        [("echo same", 2), ("printf gap", 1), ("echo same", 2)],
    );
    let filtered = query(&store, "echo same", None).entries;
    assert_eq!(filtered.len(), 2);
    assert!(filtered.iter().all(|entry| entry.repeat_count() == 2));
    let raw = store.cached();
    assert_eq!(raw.len(), 5);
    assert_eq!(rows[0].key, raw[1].key);
    assert_eq!(rows[2].key, raw[4].key);
    let history = crate::history::ShellHistory {
        store: store.clone(),
    };
    assert_eq!(
        reedline::History::count(
            &history,
            reedline::SearchQuery::everything(reedline::SearchDirection::Backward, None),
        )
        .unwrap(),
        5,
    );
    let connection = Connection::open(root.path().join("history.nosh.sqlite3")).unwrap();
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM commands", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        5
    );
}

#[test]
fn repeat_groups_keep_directories_sessions_and_hidden_inputs_distinct() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    std::fs::create_dir_all(a.join(".git")).unwrap();
    std::fs::create_dir_all(b.join(".git")).unwrap();
    let file = root.path().join("history");
    let store = shell(&file, &a);
    store.record("same", Kind::Command);
    store.record("same", Kind::Command);
    store.context(&b, true);
    store.record("same", Kind::Command);
    store.context(&a, true);
    store.record("same", Kind::Command);
    store.sync().unwrap();
    let scoped = query(&store, "same", store.scope().unwrap()).entries;
    assert_eq!(
        scoped.len(),
        2,
        "the hidden worktree input must break the run"
    );
    assert_eq!(
        scoped.iter().map(Entry::repeat_count).collect::<Vec<_>>(),
        [2, 1]
    );
    let all = query(&store, "same", None).entries;
    assert_eq!(all.len(), 3);
    assert_ne!(all[0].cwd, all[1].cwd);
    let sub = a.join("sub");
    std::fs::create_dir_all(&sub).unwrap();
    store.context(&sub, true);
    store.record("same", Kind::Command);
    store.record("same", Kind::Command);
    store.sync().unwrap();
    let scoped = query(&store, "same", store.scope().unwrap()).entries;
    assert_eq!(scoped.len(), 3);
    assert_eq!(scoped[1].worktree, scoped[2].worktree);
    assert_ne!(scoped[1].cwd, scoped[2].cwd);
    assert_eq!(scoped[2].repeat_count(), 2);
    store.context(&a, true);
    store.record("session command", Kind::Command);
    store.record("session command", Kind::Command);
    store.sync().unwrap();
    let other = shell(&file, &a);
    other.record("session command", Kind::Command);
    other.record("session command", Kind::Command);
    other.sync().unwrap();
    let sessions = query(&store, "session command", None).entries;
    assert_eq!(sessions.len(), 2);
    assert_ne!(sessions[0].session, sessions[1].session);
    assert!(sessions.iter().all(|entry| entry.repeat_count() == 2));
}

#[test]
fn memory_questions_break_command_runs_without_entering_sqlite() {
    const QUESTION: &str = "# PRIVATE_REPEAT_BOUNDARY";
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    store.record("same command", Kind::Command);
    store.record(QUESTION, Kind::Question);
    store.record(QUESTION, Kind::Question);
    store.record("same command", Kind::Command);
    store.sync().unwrap();
    let commands = query(&store, "same command", None).entries;
    assert_eq!(commands.len(), 2);
    assert!(commands.iter().all(|entry| entry.repeat_count() == 1));
    let questions = query(&store, "PRIVATE_REPEAT_BOUNDARY", None).entries;
    assert_eq!(questions.len(), 2);
    assert!(
        questions
            .iter()
            .all(|entry| entry.kind == Kind::Question && entry.repeat_count() == 1)
    );
    for name in ["history.nosh.sqlite3", "history.nosh.sqlite3-wal"] {
        let data = std::fs::read(root.path().join(name)).unwrap();
        assert!(
            !data
                .windows(QUESTION.len())
                .any(|bytes| bytes == QUESTION.as_bytes()),
            "{name}"
        );
    }
}

#[test]
fn repeated_memory_commands_remain_available_when_storage_cannot_open() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("history.nosh.sqlite3"), "not a database").unwrap();
    let store = Store::new(Some(&root.path().join("history")), None, None);
    store.context(root.path(), true);
    for _ in 0..3 {
        store.record("echo cached", Kind::Command);
    }
    assert!(store.sync().is_err());
    let result = query(&store, "cached", None);
    assert!(result.failed && result.notice.is_some());
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].text, "echo cached");
    assert_eq!(result.entries[0].repeat_count(), 3);
}

#[test]
fn repeat_comparison_preserves_kind_whitespace_and_full_multiline_text() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    let command = "printf 'first\\n'\nprintf 'second\\n'";
    store.record(command, Kind::Command);
    store.record(command, Kind::Command);
    store.sync().unwrap();
    let result = query(&store, "second", None);
    assert_eq!(result.entries.len(), 1);
    assert_eq!(result.entries[0].text, command);
    assert_eq!(result.entries[0].repeat_count(), 2);
    let original = store.cached()[0].clone();
    let mut changed = original.clone();
    changed.kind = Kind::Manual;
    assert!(!original.repeats(&changed));
    changed = original.clone();
    changed.text.push(' ');
    assert!(!original.repeats(&changed));
}

#[test]
fn a_repeat_run_larger_than_a_raw_page_is_counted_and_navigated_as_one_group() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    store.record("before", Kind::Command);
    let repeats = PAGE_ITEMS * 3 + 3;
    for index in 0..repeats {
        store.record("same command", Kind::Command);
        if index % 32 == 31 {
            store.sync().unwrap();
        }
    }
    store.record("after", Kind::Command);
    store.sync().unwrap();
    let all = query(&store, "", None);
    assert_eq!(all.entries.len(), 3);
    assert_eq!(all.entries[1].repeat_count(), repeats as u64);
    let group = &all.entries[1];
    for (older, expected) in [(true, "before"), (false, "after")] {
        let result = page(
            &store,
            Query {
                text: String::new(),
                worktree: None,
                before: Some((group.ordinal, group.key.clone())),
                include_anchor: false,
                older,
                snapshot: Some(all.snapshot),
            },
        );
        assert_eq!(result.entries.len(), 1);
        assert_eq!(result.entries[0].text, expected);
    }
    let first = &all.entries[0];
    let newer = page(
        &store,
        Query {
            text: String::new(),
            worktree: None,
            before: Some((first.ordinal, first.key.clone())),
            include_anchor: false,
            older: false,
            snapshot: Some(all.snapshot),
        },
    );
    assert_eq!(newer.entries.len(), 2);
    assert_eq!(newer.entries[0].repeat_count(), repeats as u64);
    assert_eq!(store.cached().len(), repeats + 2);
    let connection = Connection::open(root.path().join("history.nosh.sqlite3")).unwrap();
    let request = Query {
        text: String::new(),
        worktree: None,
        before: None,
        include_anchor: false,
        older: true,
        snapshot: None,
    };
    connection.progress_handler(1_000, Some(|| true)).unwrap();
    let error = select_repeated(&connection, &request, Vec::new(), &BTreeMap::new()).unwrap_err();
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    assert!(error.contains("interrupt"), "{error}");
    assert!(
        connection.is_autocommit(),
        "a cancelled read must not retain its transaction"
    );
    let retried = select_repeated(&connection, &request, Vec::new(), &BTreeMap::new()).unwrap();
    assert_eq!(retried.entries[1].repeat_count(), repeats as u64);
}

#[test]
fn grouped_pages_preserve_counts_and_identity_in_both_directions() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    for index in 0..(PAGE_ITEMS + 7) {
        for _ in 0..3 {
            store.record(&format!("command-{index:03}"), Kind::Command);
        }
        if index % 8 == 7 {
            store.sync().unwrap();
        }
    }
    store.sync().unwrap();
    let newest = query(&store, "", None);
    assert_eq!(newest.entries.len(), PAGE_ITEMS);
    assert!(newest.more);
    assert!(newest.entries.iter().all(|entry| entry.repeat_count() == 3));
    let first = &newest.entries[0];
    let older = page(
        &store,
        Query {
            text: String::new(),
            worktree: None,
            before: Some((first.ordinal, first.key.clone())),
            include_anchor: false,
            older: true,
            snapshot: Some(newest.snapshot),
        },
    );
    assert_eq!(older.entries.len(), 7);
    assert!(!older.more);
    assert!(older.entries.iter().all(|entry| entry.repeat_count() == 3));
    let last = older.entries.last().unwrap();
    let again = page(
        &store,
        Query {
            text: String::new(),
            worktree: None,
            before: Some((last.ordinal, last.key.clone())),
            include_anchor: false,
            older: false,
            snapshot: Some(newest.snapshot),
        },
    );
    assert_eq!(
        again
            .entries
            .iter()
            .map(|entry| (&entry.key, entry.repeat_count()))
            .collect::<Vec<_>>(),
        newest
            .entries
            .iter()
            .map(|entry| (&entry.key, entry.repeat_count()))
            .collect::<Vec<_>>(),
    );
}

#[test]
fn inclusive_selection_boundaries_agree_for_cache_database_and_memory_questions() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    store.record("first command", Kind::Command);
    store.record("# first question", Kind::Question);
    store.record("second command", Kind::Command);
    store.record("# second question", Kind::Question);
    store.sync().unwrap();
    let connection = Connection::open(root.path().join("history.nosh.sqlite3")).unwrap();
    connection
        .execute("UPDATE commands SET ordinal=1", [])
        .unwrap();
    store.refreshed().unwrap();
    let entries = query(&store, "", None).entries;
    assert_eq!(entries.len(), 4);
    for selected in &entries {
        for older in [false, true] {
            for include_anchor in [false, true] {
                let request = Query {
                    text: String::new(),
                    worktree: None,
                    before: Some((selected.ordinal, selected.key.clone())),
                    include_anchor,
                    older,
                    snapshot: None,
                };
                let cached = store.preview(&request);
                let fetched = page(&store, request);
                assert!(!fetched.failed, "{:?}", fetched.notice);
                assert_eq!(
                    cached
                        .entries
                        .iter()
                        .map(|entry| &entry.key)
                        .collect::<Vec<_>>(),
                    fetched
                        .entries
                        .iter()
                        .map(|entry| &entry.key)
                        .collect::<Vec<_>>(),
                );
                assert_eq!(
                    fetched
                        .entries
                        .iter()
                        .filter(|entry| entry.key == selected.key)
                        .count(),
                    usize::from(include_anchor),
                );
            }
        }
    }
}

#[test]
fn busy_database_reports_unsaved_commands_without_losing_memory_recall() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    let writer = Connection::open(database_path(&file)).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();
    store.record("still run this command", Kind::Command);
    assert!(store.sync().is_err());
    assert!(
        lock(&store.0.model)
            .errors
            .iter()
            .any(|error| error.contains("not saved"))
    );
    assert_eq!(
        query(&store, "still run", None).entries[0].text,
        "still run this command"
    );
    writer.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn brush_view_exports_commands_only_and_mutates_shared_records() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    store.record("printf visible", Kind::Command);
    store.record("# hidden question", Kind::Question);
    store.sync().unwrap();
    let mut view = brush_core::history::History::default();
    store.brush_view(&mut view);
    assert_eq!(view.count(), 1);
    let output = root.path().join("export");
    view.flush(&output, false, false, true).unwrap();
    let text = std::fs::read_to_string(output).unwrap();
    assert!(text.contains("printf visible"));
    assert!(!text.contains("hidden question"));
    assert!(
        view.flush(database_path(&file), false, false, false)
            .is_err()
    );
    view.remove_nth_item(0).unwrap();
    assert!(query(&store, "printf", None).entries.is_empty());
    view.add(brush_core::history::Item::new("manual text"))
        .unwrap();
    store.sync().unwrap();
    let manual = query(&store, "manual", None).entries.remove(0);
    assert_eq!(manual.kind, Kind::Manual);
    assert!(manual.cwd.is_none());
}

#[test]
fn shell_clone_history_edits_do_not_mutate_parent_storage() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    store.record("parent command", Kind::Command);
    store.sync().unwrap();
    let mut view = brush_core::history::History::default();
    store.brush_view(&mut view);
    let mut child = view.clone();
    child.clear().unwrap();
    assert_eq!(view.count(), 1);
    assert_eq!(query(&store, "", None).entries.len(), 1);
}

#[test]
fn old_query_generations_cannot_be_returned_as_new_results() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    store.record("old input", Kind::Command);
    store.record("new input", Kind::Command);
    store.sync().unwrap();
    let old = store.query(Query {
        text: "old".into(),
        worktree: None,
        before: None,
        include_anchor: false,
        older: true,
        snapshot: None,
    });
    let new = query(&store, "new", None);
    assert_eq!(new.entries[0].text, "new input");
    assert!(store.poll(old).is_none());
}

#[test]
fn explicit_limits_separate_current_view_from_persistent_retention() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    store
        .configure(Some(&root.path().join("history")), Some("1"), Some("2"))
        .unwrap();
    for text in ["first", "second", "third"] {
        store.record(text, Kind::Command);
    }
    store.sync().unwrap();
    let mut view = brush_core::history::History::default();
    store.brush_view(&mut view);
    assert_eq!(view.count(), 1);
    assert_eq!(view.get(0).unwrap().command_line, "third");
    let next = shell(&file, root.path());
    assert_eq!(
        query(&next, "", None)
            .entries
            .iter()
            .map(|entry| entry.text.as_str())
            .collect::<Vec<_>>(),
        ["second", "third"]
    );
}

#[test]
fn zero_retention_never_inserts_new_command_text_into_sqlite() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("history");
    let store = shell(&file, root.path());
    store.configure(Some(&file), None, Some("0")).unwrap();
    store.record("DO_NOT_PERSIST_COMMAND_029ab", Kind::Command);
    store.sync().unwrap();
    assert_eq!(query(&store, "", None).entries.len(), 1);
    let connection = Connection::open(database_path(&file)).unwrap();
    let count: i64 = connection
        .query_row("SELECT count(*) FROM commands", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    for entry in std::fs::read_dir(root.path()).unwrap() {
        let path = entry.unwrap().path();
        if path.is_file() {
            assert!(
                !std::fs::read(path)
                    .unwrap()
                    .windows(b"DO_NOT_PERSIST_COMMAND_029ab".len())
                    .any(|bytes| bytes == b"DO_NOT_PERSIST_COMMAND_029ab")
            );
        }
    }
}

#[test]
fn successful_text_append_is_not_repeated_when_the_brush_view_refreshes() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    store.record("printf once", Kind::Command);
    store.sync().unwrap();
    let mut view = brush_core::history::History::default();
    let export = root.path().join("exchange");
    store.brush_view(&mut view);
    view.flush(&export, true, true, false).unwrap();
    let first = std::fs::read(&export).unwrap();
    store.brush_view(&mut view);
    view.flush(&export, true, true, false).unwrap();
    assert_eq!(first, std::fs::read(export).unwrap());
}

#[test]
fn text_exports_preserve_full_inputs_and_unknown_time_markers() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    let text = "printf first\n# comment inside a program\nprintf last\n";
    store.record(text, Kind::Command);
    store
        .added(
            &mut brush_core::history::Item {
                id: 0,
                command_line: "manual".into(),
                timestamp: None,
                dirty: true,
            },
            None,
        )
        .unwrap();
    store.sync().unwrap();
    let mut view = brush_core::history::History::default();
    store.brush_view(&mut view);
    let file = root.path().join("exchange");
    view.flush(&file, false, false, false).unwrap();
    let exported = std::fs::read_to_string(file).unwrap();
    let (timestamp, commands) = exported.split_once('\n').unwrap();
    assert!(timestamp.strip_prefix('#').unwrap().parse::<i64>().unwrap() > 0);
    assert_eq!(commands, format!("{text}\n#0\nmanual\n"));
}

#[test]
fn a_changed_histfile_does_not_copy_old_commands_into_the_new_database() {
    let root = tempfile::tempdir().unwrap();
    let a = root.path().join("a");
    let b = root.path().join("b");
    let store = shell(&a, root.path());
    store.record("saved in a", Kind::Command);
    store.sync().unwrap();
    store.configure(Some(&b), None, None).unwrap();
    store.record("saved in b", Kind::Command);
    store.sync().unwrap();
    assert_eq!(
        query(&shell(&a, root.path()), "", None).entries[0].text,
        "saved in a"
    );
    let rows = query(&shell(&b, root.path()), "", None).entries;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "saved in b");
}

#[test]
fn a_clear_completing_after_timeout_does_not_remove_new_questions() {
    let root = tempfile::tempdir().unwrap();
    let store = shell(&root.path().join("history"), root.path());
    store.record("before clear", Kind::Command);
    store.record("# before clear", Kind::Question);
    store.sync().unwrap();
    let (release, wait) = mpsc::channel();
    store.0.tx.send(Job::Wait(wait)).unwrap();
    assert!(store.cleared().is_err());
    store.record("# after clear was requested", Kind::Question);
    release.send(()).unwrap();
    store.sync().unwrap();
    let remaining = query(&store, "", None);
    assert_eq!(remaining.entries.len(), 1);
    assert_eq!(remaining.entries[0].text, "# after clear was requested");
}

#[test]
fn explicit_rc_histfile_selects_storage_without_importing_text() {
    let root = tempfile::tempdir().unwrap();
    let file = root.path().join("custom-history");
    std::fs::write(&file, "printf old_text_history\n").unwrap();
    let rc = root.path().join("rc");
    std::fs::write(&rc, format!("HISTFILE='{}'\n", file.display())).unwrap();
    let backend = crate::EmbeddedShell::new(crate::ShellOptions {
        interactive: true,
        load_rc: true,
        rc_file: Some(rc),
        working_dir: Some(root.path().to_path_buf()),
        ..Default::default()
    })
    .unwrap();
    let store = backend.history_store().unwrap();
    assert!(query(&store, "", None).entries.is_empty());
    executed(&store, "new command", 0);
    let entries = query(&shell(&file, root.path()), "", None).entries;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].text, "new command");
    assert_eq!(
        std::fs::read_to_string(file).unwrap(),
        "printf old_text_history\n"
    );
}
