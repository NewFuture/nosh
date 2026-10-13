#![cfg(unix)]

#[path = "support/terminal.rs"]
mod terminal;

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use rusqlite::{Connection, OptionalExtension};
use terminal::Terminal;

fn setup(home: &Path) {
    setup_with_status(home, false);
}

fn setup_with_status(home: &Path, status_bar: bool) {
    std::fs::create_dir_all(home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        format!(
            r#"
[shell]
edit_mode = "emacs"
capture_output = "off"
input_assist = false
status_bar = {status_bar}
command_assist = false
terminal_integration = "off"
"#
        ),
    )
    .unwrap();
}

fn launch(home: &Path, cwd: &Path) -> Terminal {
    launch_with_columns(home, cwd, 100)
}

fn shell_command(home: &Path, cwd: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .current_dir(cwd)
        .env("HOME", home)
        .env("NOSH_HOME", home)
        .env("HISTFILE", home.join("history"))
        .env("NOSH_LANG", "en")
        .env("LC_ALL", "C.UTF-8")
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .env("PS1", "history-test> ")
        .env("PS2", "history-more> ")
        .env_remove("HISTCONTROL")
        .env_remove("HISTIGNORE")
        .env_remove("HISTSIZE")
        .env_remove("HISTFILESIZE")
        .env_remove("NOSH_DISABLE_AI")
        .env_remove("VISUAL")
        .env_remove("EDITOR")
        .args(["--offline", "--no-download", "-i"]);
    command
}

fn launch_with_columns(home: &Path, cwd: &Path, columns: u16) -> Terminal {
    let mut command = shell_command(home, cwd);
    command.arg("--norc");
    let mut terminal = Terminal::new(command, 24, columns);
    terminal.wait_for("history-test> ");
    terminal
}

fn submit(terminal: &mut Terminal, input: &str, output: &str) {
    let start = terminal.output.len();
    terminal.send(format!("{input}\r").as_bytes());
    terminal.wait_for(output);
    wait_empty_prompt(terminal, start);
}

fn submit_to_prompt(terminal: &mut Terminal, input: &str) {
    let start = terminal.output.len();
    terminal.send(format!("{input}\r").as_bytes());
    wait_empty_prompt(terminal, start);
}

fn wait_empty_prompt(terminal: &mut Terminal, start: usize) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let output = String::from_utf8_lossy(&terminal.output[start..]);
        let frame = output
            .split("\x1b[?25l")
            .filter_map(|frame| frame.split_once("\x1b[?25h").map(|(frame, _)| frame))
            .last();
        if frame.is_some_and(|frame| before_cursor(frame).ends_with("history-test> ")) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "waiting for an empty prompt: {output:?}"
        );
        assert!(
            terminal.poll().is_none(),
            "terminal exited before its next prompt"
        );
    }
}

fn finish(terminal: &mut Terminal) {
    finish_with_prompt(terminal, "history-test> ");
}

fn finish_with_prompt(terminal: &mut Terminal, prompt: &str) {
    terminal.send(b"\x03");
    terminal.wait_for(prompt);
    terminal.send(b"exit 0\r");
    let until = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = terminal.poll() {
            assert!(
                status.success(),
                "{:?}",
                String::from_utf8_lossy(&terminal.output)
            );
            return;
        }
        assert!(
            Instant::now() < until,
            "history CLI did not exit: {:?}",
            String::from_utf8_lossy(&terminal.output)
        );
    }
}

fn sql(home: &Path) -> Connection {
    Connection::open(home.join("history.nosh.sqlite3")).unwrap()
}

fn wait_outcome(home: &Path, command: &str, expected: i32) {
    let connection = sql(home);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let result: Option<Option<i32>> = connection
            .query_row(
                "SELECT exit_code FROM commands WHERE command=?1 ORDER BY seq DESC LIMIT 1",
                [command],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        if result == Some(Some(expected)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "outcome for {command:?}: {result:?}, expected {expected}"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn last_raw_frame(terminal: &Terminal) -> String {
    let output = String::from_utf8_lossy(&terminal.output);
    let frame = output
        .split("\x1b[?25l")
        .filter_map(|frame| frame.split_once("\x1b[?25h").map(|(frame, _)| frame))
        .last()
        .expect("completed terminal repaint");
    frame.to_owned()
}

fn last_frame(terminal: &Terminal) -> String {
    nosh_shell::style::strip_ansi(&last_raw_frame(terminal))
}

fn before_cursor(frame: &str) -> String {
    nosh_shell::style::strip_ansi(frame.rsplit_once("\x1b7").expect("saved input cursor").0)
}

fn inline_preview(frame: &str) -> &str {
    frame
        .rsplit_once("\x1b7")
        .expect("saved input cursor")
        .1
        .split_once("History [all]")
        .expect("history menu below input")
        .0
}

fn wait_raw_frame(terminal: &mut Terminal, accepts: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let frame = last_raw_frame(terminal);
        if accepts(&frame) {
            return frame;
        }
        assert!(
            Instant::now() < deadline,
            "unexpected terminal frame: {frame:?}"
        );
        assert!(
            terminal.poll().is_none(),
            "terminal exited before the expected frame"
        );
    }
}

fn wait_frame(terminal: &mut Terminal, accepts: impl Fn(&str) -> bool) -> String {
    let frame = wait_raw_frame(terminal, |frame| {
        accepts(&nosh_shell::style::strip_ansi(frame))
    });
    nosh_shell::style::strip_ansi(&frame)
}

#[test]
fn arrow_entries_open_filterable_history_without_restoring_tab_navigation() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    submit(&mut terminal, "printf 'MENU_A\\n'", "\nMENU_A");
    submit(&mut terminal, "printf 'MENU_B\\n'", "\nMENU_B");
    submit(&mut terminal, "printf 'MENU_B\\n'", "\nMENU_B");
    terminal.send(b"\x1b[A");
    let frame = wait_raw_frame(&mut terminal, |frame| frame.contains("History [all]"));
    let input = before_cursor(&frame);
    assert!(
        !input.contains("MENU_A") && !input.contains("MENU_B"),
        "{frame:?}"
    );
    terminal.send(b"MENU_");
    wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]") && before_cursor(frame).ends_with("MENU_")
    });
    terminal.send(b"\x1b[CA");
    let frame = wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]") && before_cursor(frame).ends_with("MENU_A")
    });
    assert!(!frame.contains("Project scope unavailable"), "{frame:?}");
    terminal.send(b"\x1b");
    wait_frame(&mut terminal, |frame| !frame.contains("History [all]"));
    terminal.send(b"\x1b[BMENU_");
    wait_frame(&mut terminal, |frame| frame.contains("History [all]"));
    terminal.send(b"\t\t\x1b[Z\t\r");
    let frame = wait_frame(&mut terminal, |frame| {
        frame.contains("printf 'MENU_B\\n'") && !frame.contains("History [all]")
    });
    assert!(!frame.contains("Project scope unavailable"), "{frame:?}");
    terminal.send(b"\x15\x1b[AMENU_");
    let frame = wait_frame(&mut terminal, |frame| frame.contains("History [all]"));
    assert!(
        frame.contains("Down newer") && frame.contains("Up older"),
        "{frame:?}"
    );
    assert!(!frame.contains("Tab"), "{frame:?}");
    terminal.send(b"\x1b[A\x1b[A\x1b[B\x1b[A\r");
    let frame = wait_frame(&mut terminal, |frame| {
        frame.contains("printf 'MENU_A\\n'") && !frame.contains("History [all]")
    });
    assert!(!frame.contains("Project scope unavailable"), "{frame:?}");
    let count: i64 = sql(&home)
        .query_row(
            "SELECT count(*) FROM commands WHERE command = ?1",
            ["printf 'MENU_A\\n'"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1, "menu acceptance must not execute the command");
    submit(&mut terminal, "; printf 'EDITED_MENU\\n'", "\nEDITED_MENU");
    finish(&mut terminal);
}

#[test]
fn selected_history_is_a_styled_inline_preview_until_explicitly_adopted() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut command = shell_command(&home, root.path());
    command
        .arg("--norc")
        .env_remove("NO_COLOR")
        .env("COLORTERM", "truecolor");
    let mut terminal = Terminal::new(command, 24, 100);
    terminal.wait_for("history-test> ");
    submit(&mut terminal, "printf 'GHOST_OLD\\n'", "\nGHOST_OLD");
    submit(&mut terminal, "printf 'GHOST_NEW\\n'", "\nGHOST_NEW");
    terminal.send(b"\x1b[A");
    let frame = wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]") && inline_preview(frame).contains("GHOST_NEW")
    });
    assert!(!before_cursor(&frame).contains("GHOST_"), "{frame:?}");
    assert!(
        inline_preview(&frame).contains("\x1b[3;"),
        "preview must use the themed hint style: {frame:?}"
    );
    terminal.send(b"\x1b[A");
    wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]") && inline_preview(frame).contains("GHOST_OLD")
    });
    terminal.send(b"GHOST_");
    let frame = wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]")
            && before_cursor(frame).ends_with("GHOST_")
            && inline_preview(frame).contains(" -> ")
    });
    assert!(inline_preview(&frame).contains("GHOST_OLD"), "{frame:?}");
    terminal.send(b"\r");
    wait_raw_frame(&mut terminal, |frame| {
        !frame.contains("History [all]") && before_cursor(frame).ends_with("printf 'GHOST_OLD\\n'")
    });
    assert_eq!(
        sql(&home)
            .query_row(
                "SELECT count(*) FROM commands WHERE command=?1",
                ["printf 'GHOST_OLD\\n'"],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        1,
        "a preview and its adoption must not execute or record the selected command",
    );
    terminal.send(b"\x15printf '\x12");
    wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]")
            && before_cursor(frame).ends_with("printf '")
            && inline_preview(frame).contains("GHOST_NEW")
    });
    terminal.send(b"\x1b");
    wait_raw_frame(&mut terminal, |frame| {
        !frame.contains("History [all]") && before_cursor(frame).ends_with("printf '")
    });
    finish(&mut terminal);
}

#[test]
fn repeated_commands_are_folded_for_navigation_but_not_for_recording_or_export() {
    const REPEATED: &str = "printf 'REPEAT_COLLAPSE\\n'";
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    for _ in 0..3 {
        submit(&mut terminal, REPEATED, "\nREPEAT_COLLAPSE");
    }
    submit(&mut terminal, "printf 'REPEAT_GAP\\n'", "\nREPEAT_GAP");
    for _ in 0..2 {
        submit(&mut terminal, REPEATED, "\nREPEAT_COLLAPSE");
    }
    terminal.send(b"\x1b[A");
    let frame = wait_frame(&mut terminal, |frame| {
        frame.contains("History [all]") && frame.contains("x3") && frame.contains("x2")
    });
    assert_eq!(
        frame
            .lines()
            .filter(|line| line.contains(REPEATED) && line.contains(" ok"))
            .count(),
        2,
        "{frame}"
    );
    terminal.send(b"\x1b[A");
    wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]") && inline_preview(frame).contains("REPEAT_GAP")
    });
    terminal.send(b"\x1b[A");
    wait_raw_frame(&mut terminal, |frame| {
        frame.contains("History [all]") && inline_preview(frame).contains("REPEAT_COLLAPSE")
    });
    terminal.send(b"REPEAT_COLLAPSE");
    let frame = wait_frame(&mut terminal, |frame| {
        frame.contains("x3 ok") && frame.contains("x2 ok") && !frame.contains("REPEAT_GAP")
    });
    assert!(
        !frame.contains("[cmd") && !frame.contains(": session"),
        "{frame}"
    );
    assert!(
        !frame.contains("ok:3") && !frame.contains("ok:2"),
        "{frame}"
    );
    assert_eq!(
        frame
            .lines()
            .filter(|line| line.contains(REPEATED) && line.contains(" ok"))
            .count(),
        2,
        "{frame}"
    );
    terminal.send(b"\r");
    wait_raw_frame(&mut terminal, |frame| {
        !frame.contains("History [all]") && before_cursor(frame).ends_with(REPEATED)
    });
    assert_eq!(
        sql(&home)
            .query_row(
                "SELECT count(*) FROM commands WHERE command=?1",
                [REPEATED],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        5,
        "folding and adoption must not alter or execute the raw history",
    );
    terminal.send(b"\x15history -w \"$HISTFILE.export\"\r");
    terminal.wait_for("history-test> ");
    let exported = std::fs::read_to_string(home.join("history.export")).unwrap();
    assert_eq!(exported.lines().filter(|line| *line == REPEATED).count(), 5);
    finish(&mut terminal);
}

#[test]
fn execution_outcomes_persist_and_mixed_repeats_show_honest_counts() {
    const COMMAND: &str = "test -f outcome-flag";
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let flag = root.path().join("outcome-flag");
    let mut terminal = launch(&home, root.path());
    submit_to_prompt(&mut terminal, COMMAND);
    wait_outcome(&home, COMMAND, 1);
    std::fs::write(&flag, "").unwrap();
    submit_to_prompt(&mut terminal, COMMAND);
    wait_outcome(&home, COMMAND, 0);
    std::fs::remove_file(&flag).unwrap();
    submit_to_prompt(&mut terminal, COMMAND);
    wait_outcome(&home, COMMAND, 1);
    terminal.send(b"\x12outcome-flag");
    let frame = wait_frame(&mut terminal, |frame| {
        frame.contains("x3") && frame.contains("fail:2") && frame.contains("ok:1")
    });
    assert_eq!(
        frame
            .lines()
            .filter(|line| line.contains(COMMAND) && line.contains("x3 fail:2 ok:1"))
            .count(),
        1,
        "{frame}"
    );
    terminal.send(b"\r");
    wait_raw_frame(&mut terminal, |frame| {
        !frame.contains("History [all]") && before_cursor(frame).ends_with(COMMAND)
    });
    assert_eq!(
        sql(&home)
            .query_row(
                "SELECT count(*) FROM commands WHERE command=?1",
                [COMMAND],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        3
    );
    submit_to_prompt(&mut terminal, "\x15history -s 'echo OUTCOME_MANUAL'");
    wait_outcome(&home, "history -s 'echo OUTCOME_MANUAL'", 0);
    assert_eq!(
        sql(&home)
            .query_row(
                "SELECT exit_code FROM commands WHERE command='echo OUTCOME_MANUAL'",
                [],
                |row| row.get::<_, Option<i32>>(0)
            )
            .unwrap(),
        None
    );
    finish(&mut terminal);
    let mut terminal = launch(&home, root.path());
    terminal.send(b"\x12outcome-flag");
    wait_frame(&mut terminal, |frame| {
        frame.contains("x3") && frame.contains("fail:2") && frame.contains("ok:1")
    });
    terminal.send(b"\x15OUTCOME_MANUAL");
    wait_frame(&mut terminal, |frame| frame.contains("unknown manual"));
    finish(&mut terminal);
}

#[test]
fn fc_replays_get_their_own_outcome_and_ignored_or_cleared_inputs_do_not_relabel_history() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    submit_to_prompt(&mut terminal, "false");
    wait_outcome(&home, "false", 1);
    submit_to_prompt(&mut terminal, "fc -s false=true");
    wait_outcome(&home, "true", 0);
    assert_eq!(
        sql(&home)
            .query_row(
                "SELECT count(*) FROM commands WHERE command='fc -s false=true'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    submit_to_prompt(&mut terminal, "HISTCONTROL=ignorespace");
    wait_outcome(&home, "HISTCONTROL=ignorespace", 0);
    submit_to_prompt(&mut terminal, " false");
    assert_eq!(
        sql(&home)
            .query_row(
                "SELECT count(*) FROM commands WHERE command=' false'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    wait_outcome(&home, "HISTCONTROL=ignorespace", 0);
    submit_to_prompt(&mut terminal, "history -c");
    assert_eq!(
        sql(&home)
            .query_row("SELECT count(*) FROM commands", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    finish(&mut terminal);
}

#[test]
fn interrupted_commands_and_shell_exit_keep_their_actual_exit_codes() {
    const COMMAND: &str = "printf 'OUTCOME_READY\\n'; sleep 30";
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    terminal.send(format!("{COMMAND}\r").as_bytes());
    terminal.wait_for("\nOUTCOME_READY");
    terminal.send(b"\x03");
    terminal.wait_for("history-test> ");
    wait_outcome(&home, COMMAND, 130);
    terminal.send(b"exit 7\r");
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(status) = terminal.poll() {
            assert_eq!(status.code(), Some(7));
            break;
        }
        assert!(Instant::now() < deadline, "shell did not exit");
    }
    wait_outcome(&home, "exit 7", 7);
}

#[test]
fn unavailable_database_still_allows_recalling_current_memory_commands() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    std::fs::write(home.join("history.nosh.sqlite3"), "not a database").unwrap();
    let mut terminal = launch(&home, root.path());
    submit(
        &mut terminal,
        "printf 'MEMORY_HISTORY_OK\\n'",
        "\nMEMORY_HISTORY_OK",
    );
    terminal.send(b"\x12MEMORY_HISTORY_OK");
    wait_frame(&mut terminal, |frame| {
        frame.contains("History [all]")
            && frame.contains("file is not a database")
            && frame.contains("printf 'MEMORY_HISTORY_OK\\n'")
    });
    terminal.send(b"\r");
    wait_frame(&mut terminal, |frame| {
        frame.contains("printf 'MEMORY_HISTORY_OK\\n'") && !frame.contains("History [all]")
    });
    finish(&mut terminal);
}

#[test]
fn invalid_fc_indices_do_not_replay_an_unrelated_history_entry() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    submit(
        &mut terminal,
        "printf 'REPLAY_INDEX_SENTINEL\\n'",
        "\nREPLAY_INDEX_SENTINEL",
    );
    for index in ["999999", "-999999", "-9223372036854775808"] {
        submit(
            &mut terminal,
            &format!("fc -s {index}; printf 'BAD_INDEX_HANDLED\\n'"),
            "\nBAD_INDEX_HANDLED",
        );
    }
    finish(&mut terminal);
    assert_eq!(
        String::from_utf8_lossy(&terminal.output)
            .matches("\nREPLAY_INDEX_SENTINEL\r\n")
            .count(),
        1
    );
}

#[test]
fn history_status_bar_owns_scope_progress_and_keys_without_repeated_menu_captions() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup_with_status(&home, true);
    let mut terminal = launch_with_columns(&home, root.path(), 219);
    submit(
        &mut terminal,
        "printf 'STATUS_REUSE_MARKER\\n'",
        "\nSTATUS_REUSE_MARKER",
    );
    terminal.send(b"\x12STATUS_REUSE_MARKER");
    terminal.wait_for("history-test> ");
    terminal.wait_for("\x1b[?25h");
    let frame = last_frame(&terminal);
    let input = frame.find("history-test> ").unwrap();
    for text in [
        "History [all]",
        "Input directory:",
        "Ctrl+R scope",
        "Enter edit",
        "Esc cancel",
    ] {
        assert_eq!(frame.matches(text).count(), 1, "{text}: {frame:?}");
        assert!(frame[..input].contains(text), "{text}: {frame:?}");
    }
    terminal.send(b"\x15NO_SUCH_HISTORY_STATUS_ROW");
    terminal.wait_for("No matches");
    terminal.wait_for("\x1b[?25h");
    let frame = last_frame(&terminal);
    let input = frame.find("history-test> ").unwrap();
    assert_eq!(frame.matches("No matches").count(), 1, "{frame:?}");
    assert!(frame[..input].contains("No matches"), "{frame:?}");
    assert!(
        !frame.contains("Enter edit") && !frame.contains("Enter submit"),
        "{frame:?}"
    );
    assert_eq!(frame.matches("Enter close").count(), 1, "{frame:?}");
    finish(&mut terminal);
}

#[test]
fn enter_on_no_matches_closes_history_keeps_input_and_does_not_execute_it() {
    const COMMAND: &str = "printf 'NO_MATCH_ACCEPT_READY\\n'";
    for status_bar in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        setup_with_status(&home, status_bar);
        let mut terminal = launch_with_columns(&home, root.path(), 219);
        terminal.send(format!("printf ORIGINAL_UNSENT\x1b[A\x15{COMMAND}").as_bytes());
        let frame = wait_frame(&mut terminal, |frame| {
            frame.contains("No matches") && frame.contains(COMMAND) && frame.contains("Enter close")
        });
        assert!(!frame.contains("Enter submit"), "{frame:?}");
        terminal.send(b"\r");
        wait_frame(&mut terminal, |frame| {
            frame.contains(COMMAND)
                && !frame.contains("History [all]")
                && !frame.contains("No matches")
        });
        let count: i64 = sql(&home)
            .query_row(
                "SELECT count(*) FROM commands WHERE command = ?1",
                [COMMAND],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "leaving search must not execute the input");
        assert!(!String::from_utf8_lossy(&terminal.output).contains("\nNO_MATCH_ACCEPT_READY\r\n"));
        submit(&mut terminal, "", "\nNO_MATCH_ACCEPT_READY");
        finish(&mut terminal);
        assert_eq!(
            sql(&home)
                .query_row(
                    "SELECT count(*) FROM commands WHERE command = ?1",
                    [COMMAND],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1
        );
    }
}

#[test]
fn history_filter_keeps_cursor_editing_undo_redo_and_the_original_draft() {
    for status_bar in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        setup_with_status(&home, status_bar);
        let mut terminal = launch_with_columns(&home, root.path(), 219);
        terminal.send(b"ORIGINAL_UNSENT\x12\x15LEFT RIGHT");
        wait_frame(&mut terminal, |frame| {
            frame.contains("LEFT RIGHT") && frame.contains("No matches")
        });
        terminal.send(b"\x1b[D\x1b[D\x1b[D\x1b[D\x1b[D\x15");
        wait_frame(&mut terminal, |frame| {
            frame.contains("RIGHT") && !frame.contains("LEFT") && frame.contains("No matches")
        });
        terminal.send(b"\x1a");
        wait_frame(&mut terminal, |frame| {
            frame.contains("LEFT RIGHT") && frame.contains("No matches")
        });
        terminal.send(b"\x19");
        wait_frame(&mut terminal, |frame| {
            frame.contains("RIGHT") && !frame.contains("LEFT") && frame.contains("No matches")
        });
        terminal.send(b"\x01KEPT_\x05_END");
        wait_frame(&mut terminal, |frame| {
            frame.contains("KEPT_RIGHT_END") && frame.contains("No matches")
        });
        terminal.send(b"\x07");
        wait_frame(&mut terminal, |frame| {
            frame.contains("ORIGINAL_UNSENT") && !frame.contains("History [all]")
        });
        assert_eq!(
            sql(&home)
                .query_row("SELECT count(*) FROM commands", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0,
            "query editing and cancellation must not submit or record a command",
        );
        finish(&mut terminal);
    }
}

#[test]
fn default_shell_keeps_its_environment_bar_and_restores_input_feedback_after_history() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let project = root.path().join("ux-project");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    std::fs::write(home.join(".bashrc"), "unset PS1\n").unwrap();
    let mut command = shell_command(&home, &project);
    command.env_remove("PS1").env_remove("PS2");
    let mut terminal = Terminal::new(command, 24, 100);
    terminal.wait_for("Ctrl+R history");
    terminal.wait_for("\x1b[?25h");
    let frame = last_frame(&terminal);
    assert!(
        frame.find("ux-project").unwrap() < frame.find("Ctrl+R history").unwrap(),
        "the environment must appear in the status row, not a custom PS1: {frame:?}",
    );
    terminal.send(b"printf 'UX_DEFAULT_UNSENT");
    wait_frame(&mut terminal, |frame| {
        frame.contains("UX_DEFAULT_UNSENT")
            && frame.contains("Incomplete")
            && frame.contains("unterminated")
    });
    terminal.send(b"\x1b[A");
    let frame = wait_frame(&mut terminal, |frame| {
        frame.contains("History [all]")
            && frame.contains("No matches")
            && frame.contains("Enter close")
    });
    assert!(
        !frame.contains("Incomplete") && !frame.contains("unterminated"),
        "{frame:?}"
    );
    terminal.send(b"\r");
    wait_frame(&mut terminal, |frame| {
        frame.contains("UX_DEFAULT_UNSENT")
            && frame.contains("Incomplete")
            && frame.contains("unterminated")
            && !frame.contains("History [all]")
    });
    assert_eq!(
        sql(&home)
            .query_row("SELECT count(*) FROM commands", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0,
    );
    finish_with_prompt(&mut terminal, "Ctrl+R history");
}

fn git(directory: &Path, args: &[&str]) {
    let status = Command::new("git")
        .current_dir(directory)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "commit.gpgsign=false",
            "-c",
            "user.name=nosh fixture",
            "-c",
            "user.email=fixture@example.invalid",
        ])
        .args(args)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn history_panel_filters_real_worktrees_restores_draft_and_edits_before_submit() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    let a = root.path().join("a");
    let b = root.path().join("b");
    setup(&home);
    std::fs::create_dir(&a).unwrap();
    git(&a, &["init", "--quiet", "--initial-branch=fixture"]);
    git(&a, &["commit", "--quiet", "--allow-empty", "-m", "fixture"]);
    git(
        &a,
        &[
            "worktree",
            "add",
            "--quiet",
            "--detach",
            b.to_str().unwrap(),
            "HEAD",
        ],
    );
    let mut terminal = launch(&home, &a);
    submit(&mut terminal, "printf 'A_ONE\\n'", "\nA_ONE");
    submit(
        &mut terminal,
        &format!("cd '{}'; printf 'DIRECTORY_READY\\n'", b.display()),
        "\nDIRECTORY_READY",
    );
    submit(&mut terminal, "printf 'B_TWO\\n'", "\nB_TWO");
    terminal.send(b"printf ORIGINAL_DRAFT");
    terminal.wait_for("ORIGINAL_DRAFT");
    terminal.send(b"\x12\x15A_ONE");
    terminal.wait_for("history-test> ");
    terminal.wait_for("A_ONE");
    terminal.wait_for("History [all]");
    terminal.send(b"\x12");
    terminal.wait_for("History [project]");
    terminal.wait_for("No matches");
    terminal.send(b"\x1b");
    terminal.wait_for("ORIGINAL_DRAFT");
    terminal.send(b"\x15\x12A_ONE");
    terminal.wait_for("History [all]");
    terminal.send(b"\r");
    terminal.wait_for("history-test> ");
    submit(&mut terminal, "; printf 'EDITED\\n'", "\nEDITED");
    finish(&mut terminal);
    let connection = sql(&home);
    let edited: (String, String) = connection
        .query_row(
            "SELECT command,worktree FROM commands WHERE command LIKE '%EDITED%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(edited.0, "printf 'A_ONE\\n'; printf 'EDITED\\n'");
    assert_eq!(edited.1, b.canonicalize().unwrap().to_str().unwrap());
    let unsubmitted: i64 = connection
        .query_row(
            "SELECT count(*) FROM commands WHERE command LIKE '%ORIGINAL_DRAFT%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(unsubmitted, 0);
    let output = String::from_utf8_lossy(&terminal.output);
    assert!(output.contains("History [all]\r\n"), "{output}");
    assert!(!output.contains("Search: "), "{output}");
}

#[test]
fn history_filtering_reuses_the_command_input_and_multiline_continuation_prompt() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    terminal.send(b"\x12\x1b[200~FIRST_FILTER_LINE\nSECOND_FILTER_LINE\x1b[201~");
    terminal.wait_for("history-test> ");
    terminal.wait_for("FIRST_FILTER_LINE");
    terminal.wait_for("history-more> SECOND_FILTER_LINE");
    terminal.send(b"\x1b");
    terminal.wait_for("history-test> ");
    finish(&mut terminal);
    let count: i64 = sql(&home)
        .query_row(
            "SELECT count(*) FROM commands WHERE command LIKE '%FILTER_LINE%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    assert!(!String::from_utf8_lossy(&terminal.output).contains("Search: "));
}

#[test]
fn questions_are_recallable_without_a_model_but_not_exported_or_restored() {
    const QUESTION: &str = "PRIVATE_QUESTION_9c5721";
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    submit(&mut terminal, "printf 'PERSISTED\\n'", "\nPERSISTED");
    submit(
        &mut terminal,
        &format!("# {QUESTION} explain only"),
        "not installed",
    );
    terminal.send(b"\x1b[A");
    terminal.wait_for("question: memory");
    terminal.send(b"\x1b");
    terminal.wait_for("history-test> ");
    submit(
        &mut terminal,
        "history -w \"$HISTFILE.export\"; printf 'EXPORT_DONE\\n'",
        "\nEXPORT_DONE",
    );
    finish(&mut terminal);
    assert!(
        std::fs::read_to_string(home.join("history.export"))
            .unwrap()
            .contains("PERSISTED")
    );
    let connection = sql(&home);
    let count: i64 = connection
        .query_row(
            "SELECT count(*) FROM commands WHERE instr(command,?1)>0",
            [QUESTION],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
    for file in std::fs::read_dir(&home).unwrap() {
        let file = file.unwrap();
        if file.file_type().unwrap().is_file() {
            assert!(
                !std::fs::read(file.path())
                    .unwrap()
                    .windows(QUESTION.len())
                    .any(|bytes| bytes == QUESTION.as_bytes()),
                "{:?}",
                file.path()
            );
        }
    }
    let mut next = launch(&home, root.path());
    next.send(format!("\x12{QUESTION}").as_bytes());
    next.wait_for("No matches");
    next.send(b"\x1b");
    next.wait_for("history-test> ");
    next.send(b"\x12PERSISTED");
    next.wait_for("PERSISTED");
    next.send(b"\x1b");
    next.wait_for("history-test> ");
    finish(&mut next);
}

#[test]
fn history_fc_and_live_disable_use_the_same_command_authority() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    submit(&mut terminal, "printf 'FIRST\\n'", "\nFIRST");
    submit(&mut terminal, "fc -s FIRST=SECOND", "\nSECOND");
    submit(
        &mut terminal,
        "HISTCONTROL=ignorespace; printf 'POLICY_READY\\n'",
        "\nPOLICY_READY",
    );
    submit(
        &mut terminal,
        " printf 'PRIVATE_SPACE_0821\\n'",
        "\nPRIVATE_SPACE_0821",
    );
    submit(
        &mut terminal,
        "unset HISTFILE; printf 'PERSISTENCE_DISABLED\\n'",
        "\nPERSISTENCE_DISABLED",
    );
    submit(
        &mut terminal,
        "printf 'PRIVATE_DISABLED_0731\\n'",
        "\nPRIVATE_DISABLED_0731",
    );
    finish(&mut terminal);
    let connection = sql(&home);
    let statements = connection
        .prepare("SELECT command FROM commands")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert!(statements.iter().any(|text| text == "printf 'SECOND\\n'"));
    assert!(!statements.iter().any(|text| text.starts_with("fc -s")));
    assert!(
        !statements
            .iter()
            .any(|text| text.contains("PRIVATE_SPACE_0821"))
    );
    assert!(
        !statements
            .iter()
            .any(|text| text.contains("PRIVATE_DISABLED_0731"))
    );
}

#[test]
fn empty_and_recursive_fc_fail_without_hanging_and_private_replay_is_not_recorded() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    setup(&home);
    let mut terminal = launch(&home, root.path());
    submit(
        &mut terminal,
        "fc -s; printf 'AFTER_EMPTY_FC\\n'",
        "\nAFTER_EMPTY_FC",
    );
    submit(
        &mut terminal,
        "fc -s; printf 'AFTER_RECURSIVE_FC\\n'",
        "\nAFTER_RECURSIVE_FC",
    );
    submit(
        &mut terminal,
        "HISTCONTROL=ignorespace; printf 'POLICY_READY\\n'",
        "\nPOLICY_READY",
    );
    submit(
        &mut terminal,
        "printf 'PUBLIC_REPLAY\\n'",
        "\nPUBLIC_REPLAY",
    );
    submit(
        &mut terminal,
        " fc -s PUBLIC_REPLAY=PRIVATE_REPLAY_8832",
        "\nPRIVATE_REPLAY_8832",
    );
    finish(&mut terminal);
    let count: i64 = sql(&home)
        .query_row(
            "SELECT count(*) FROM commands WHERE command LIKE '%PRIVATE_REPLAY_8832%'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 0);
}
