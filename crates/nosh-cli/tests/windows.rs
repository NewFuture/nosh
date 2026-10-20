#![cfg(windows)]

use std::fs;
use std::io::Write;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use nosh_core::approval::{ApprovalResponse, Scripted};
use nosh_core::{Agent, AgentConfig, Environment, RecordUi, TaskInput, TaskStatus, ToolSet};
use nosh_engine::mock::{MockChatEngine, call, text};
use nosh_permissions::ApprovalMode;
use nosh_shell::{AgentExecOpts, EmbeddedShell, NullSink, ShellOptions, Trigger};
use serde_json::json;

#[path = "support/windows.rs"]
mod console;

fn nosh(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_nosh"));
    command
        .env("NOSH_HOME", home)
        .env_remove("NOSH_MODEL_PATH")
        .env_remove("NOSH_EVAL_WORKER")
        .env_remove("NOSH_INTERNAL_PTY_FD")
        .env_remove("NOSH_EVAL_TRACE")
        .env("NOSH_DISABLE_AI", "1")
        .env("LC_ALL", "C.UTF-8")
        .stdin(Stdio::null());
    command
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.to_str().unwrap().replace('\'', "'\\''"))
}

fn shell(directory: &Path) -> EmbeddedShell {
    EmbeddedShell::new(ShellOptions {
        working_dir: Some(directory.to_owned()),
        catch_sigint: true,
        ..Default::default()
    })
    .unwrap()
}

#[test]
fn executable_is_native_and_runs_bash_without_unix_utilities() {
    let home = tempfile::tempdir().unwrap();
    assert_eq!(&fs::read(env!("CARGO_BIN_EXE_nosh")).unwrap()[..2], b"MZ");
    let output = nosh(home.path()).arg("--version").output().unwrap();
    assert!(output.status.success());
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        format!("nosh {}", env!("CARGO_PKG_VERSION"))
    );
    let system = std::env::var_os("SystemRoot").unwrap();
    let output = nosh(home.path()).env("PATH", Path::new(&system).join("System32"))
        .args(["--safe", "-c", r#"export NATIVE_STATE=ready; f() { printf '%s:%s\n' "$NATIVE_STATE" "$1"; }; f "$1"; cmd.exe /d /c "echo %NATIVE_STATE%"; exit 7"#, "native", "a b \u{4e2d}"])
        .output().unwrap();
    assert_eq!(output.status.code(), Some(7), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .replace("\r\n", "\n"),
        "ready:a b \u{4e2d}\nready\n"
    );
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn native_pipelines_redirections_and_script_paths_preserve_arguments() {
    let home = tempfile::tempdir().unwrap();
    let directory = home.path().join("space \u{4e2d}");
    fs::create_dir(&directory).unwrap();
    let script = directory.join("script.sh");
    fs::write(&script, r#"printf '%s\n' "$1" > out.txt; cmd.exe /d /c type out.txt | cmd.exe /d /c findstr native"#).unwrap();
    let output = nosh(home.path())
        .current_dir(&directory)
        .arg(&script)
        .arg("native quoted value")
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap().trim(),
        "native quoted value"
    );
    assert_eq!(
        fs::read_to_string(directory.join("out.txt")).unwrap(),
        "native quoted value\n"
    );
}

#[test]
fn stdin_commands_and_read_builtin_share_one_unbuffered_input() {
    let home = tempfile::tempdir().unwrap();
    let mut child = nosh(home.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(b"read word\nhello\nprintf 'got:%s\\n' \"$word\"\nfor i in 1 2; do\nprintf '%s\\n' \"$i\"\ndone\n").unwrap();
    let output = child.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert_eq!(output.stdout, b"got:hello\n1\n2\n");
    assert!(output.stderr.is_empty(), "{:?}", output.stderr);
}

#[test]
fn user_and_agent_keep_the_same_directory_variables_functions_and_aliases() {
    let home = tempfile::tempdir().unwrap();
    fs::create_dir(home.path().join("sub")).unwrap();
    let mut shell = shell(home.path());
    assert_eq!(shell.run_user_line("export NATIVE_STATE=user; f() { printf '%s:%s\\n' \"$NATIVE_STATE\" \"$1\"; }; alias native_alias='f alias'").exit_code, 0);
    let result = shell
        .run_agent_command(
            "cd sub; export NATIVE_STATE=agent; f 'a b'; cmd.exe /d /c \"echo %NATIVE_STATE%\"",
            &AgentExecOpts::default(),
            &mut NullSink,
        )
        .unwrap();
    assert_eq!(result.exit_code, 0, "{}", result.stderr);
    assert_eq!(result.stdout.replace("\r\n", "\n"), "agent:a b\nagent\n");
    assert_eq!(shell.cwd(), home.path().join("sub"));
    assert_eq!(shell.var("NATIVE_STATE").as_deref(), Some("agent"));
    assert!(shell.has_function("f"));
    assert!(shell.aliases().contains_key("native_alias"));
    let next = shell
        .run_agent_command("f next", &AgentExecOpts::default(), &mut NullSink)
        .unwrap();
    assert_eq!(next.stdout, "agent:next\n");
}

#[test]
fn native_agent_uses_existing_approval_and_mock_engine() {
    let home = tempfile::tempdir().unwrap();
    let mut shell = shell(home.path());
    let engine = MockChatEngine::new(vec![
        vec![call(
            "exec",
            json!({"command": "export NATIVE_AGENT=approved; printf 'agent-output'"}),
        )],
        vec![text("done")],
    ]);
    let mut agent = Agent::new(
        Box::new(engine),
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..Default::default()
        },
        Environment::detect(&shell),
        ToolSet::Full,
    );
    let mut approval = Scripted::new([ApprovalResponse::Approve]);
    let mut ui = RecordUi::default();
    let outcome = agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Cli, "set a session variable"),
        &mut approval,
        &mut ui,
    );
    assert_eq!(outcome.status, TaskStatus::Completed, "{outcome:?}");
    assert!(!approval.seen.is_empty());
    assert_eq!(shell.var("NATIVE_AGENT").as_deref(), Some("approved"));
    assert!(ui.output.contains("agent-output"), "{ui:?}");
    assert_eq!(ui.text, "done");
}

#[test]
fn read_tool_paths_preserve_drive_unc_and_verbatim_roots() {
    for (input, expected) in [
        (r"D:\work\sub\..\file.txt", r"D:\work\file.txt"),
        (
            r"\\server\share\work\..\file.txt",
            r"\\server\share\file.txt",
        ),
        (
            r"\\?\UNC\server\share\work\file.txt",
            r"\\?\UNC\server\share\work\file.txt",
        ),
        (r"\\?\D:\work \file.txt", r"\\?\D:\work \file.txt"),
    ] {
        for name in ["read_file", "grep"] {
            let args = if name == "grep" {
                json!({"path": input, "pattern": "needle"})
            } else {
                json!({"path": input})
            };
            let call = nosh_engine::ToolCall {
                name: name.into(),
                args: args.as_object().unwrap().clone(),
            };
            let path = nosh_core::tools::tool_path(&call, Path::new(r"C:\workspace"));
            assert_eq!(path, Path::new(expected), "{name}: {input}");
            assert!(path.is_absolute(), "{name}: {path:?}");
        }
    }
}

#[test]
fn native_agent_read_and_grep_authorize_and_open_the_same_drive_path() {
    let home = tempfile::tempdir().unwrap();
    let file = home.path().join("native-read.txt");
    fs::write(&file, "NATIVE-READ-FIXTURE\n").unwrap();
    let mut calls = Vec::new();
    for path in [&file, &file.canonicalize().unwrap()] {
        calls.push(call("read_file", json!({"path": path})));
        calls.push(call(
            "grep",
            json!({"path": path, "pattern": "NATIVE-READ-FIXTURE"}),
        ));
    }
    let engine = MockChatEngine::new(vec![calls, vec![text("done")]]);
    let received = engine.received();
    let mut shell = shell(home.path());
    let mut agent = Agent::new(
        Box::new(engine),
        AgentConfig {
            mode: ApprovalMode::Confirm,
            ..Default::default()
        },
        Environment::detect(&shell),
        ToolSet::ReadOnly,
    );
    let mut approval = Scripted::new([]);
    let mut ui = RecordUi::default();
    let outcome = agent.run_task(
        &mut shell,
        TaskInput::new(Trigger::Cli, "read and search the native fixture"),
        &mut approval,
        &mut ui,
    );
    assert_eq!(outcome.status, TaskStatus::Completed, "{outcome:?}");
    assert!(approval.seen.is_empty(), "{:?}", approval.seen);
    let received = received.lock().unwrap();
    let results: Vec<_> = received
        .iter()
        .flatten()
        .filter_map(|message| match message {
            nosh_engine::Message::Tool(result) => Some(result),
            _ => None,
        })
        .collect();
    assert_eq!(results.len(), 4, "{results:?}");
    for pair in results.chunks_exact(2) {
        assert!(pair[0].contains("1  NATIVE-READ-FIXTURE"), "{pair:?}");
        assert!(
            pair[1].contains("native-read.txt:1:NATIVE-READ-FIXTURE"),
            "{pair:?}"
        );
    }
}

struct OwnedProbe(Child);

impl Drop for OwnedProbe {
    fn drop(&mut self) {
        if self.0.try_wait().unwrap().is_none() {
            self.0.kill().unwrap();
        }
        self.0.wait().unwrap();
    }
}

fn helper_command() -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", "native_child_helper", "--nocapture"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    command
}

#[test]
fn native_child_helper() {
    let Ok(mode) = std::env::var("NOSH_WINDOWS_PROBE") else {
        return;
    };
    if mode == "vt" {
        use windows_sys::Win32::System::Console::{
            ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle, STD_OUTPUT_HANDLE,
            SetConsoleMode,
        };
        let mut flags = 0;
        let handle = unsafe { GetStdHandle(STD_OUTPUT_HANDLE) };
        assert_ne!(unsafe { GetConsoleMode(handle, &raw mut flags) }, 0);
        assert_ne!(
            unsafe { SetConsoleMode(handle, flags & !ENABLE_VIRTUAL_TERMINAL_PROCESSING) },
            0
        );
        let capabilities = nosh_platform::terminal::stdout();
        assert_ne!(unsafe { GetConsoleMode(handle, &raw mut flags) }, 0);
        fs::write(
            std::env::var_os("NOSH_WINDOWS_PID_FILE").unwrap(),
            serde_json::to_vec(&[
                capabilities.tty,
                capabilities.ansi,
                flags & ENABLE_VIRTUAL_TERMINAL_PROCESSING != 0,
            ])
            .unwrap(),
        )
        .unwrap();
        std::process::exit(0);
    }
    if mode == "console" {
        use std::io::IsTerminal;
        fs::write(
            std::env::var_os("NOSH_WINDOWS_PID_FILE").unwrap(),
            serde_json::to_vec(&[
                std::io::stdin().is_terminal(),
                std::io::stdout().is_terminal(),
                std::io::stderr().is_terminal(),
                fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open("CONIN$")
                    .is_ok(),
            ])
            .unwrap(),
        )
        .unwrap();
        println!("CONSOLE-PROBE");
        std::process::exit(0);
    }
    if mode == "tree" || mode == "orphan" {
        let mut command = helper_command();
        nosh_platform::process::prepare_background(&mut command);
        let child = command
            .env("NOSH_WINDOWS_PROBE", "sleep")
            .env_remove("NOSH_WINDOWS_PID_FILE")
            .spawn()
            .unwrap();
        let path = std::path::PathBuf::from(std::env::var_os("NOSH_WINDOWS_PID_FILE").unwrap());
        let pending = path.with_extension("pending");
        fs::write(&pending, format!("{} {}", std::process::id(), child.id())).unwrap();
        // Existence is the cancellation handshake; publish only a complete PID pair.
        fs::rename(pending, path).unwrap();
        if mode == "orphan" {
            std::process::exit(0);
        }
    } else {
        assert_eq!(mode, "sleep");
    }
    std::thread::sleep(Duration::from_secs(30));
    std::process::exit(0);
}

#[test]
fn conpty_standard_handles_are_native_console_handles() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("console.json");
    let mut command = helper_command();
    command
        .env("NOSH_WINDOWS_PROBE", "console")
        .env("NOSH_WINDOWS_PID_FILE", &path);
    let mut terminal = console::Terminal::spawn(command);
    terminal.expect("CONSOLE-PROBE");
    let handles: Vec<bool> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(handles, [true, true, true, true], "{}", terminal.text());
    terminal.finish();
}

#[test]
fn native_terminal_enables_vt_even_when_term_is_already_set() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("vt.json");
    let mut command = helper_command();
    command
        .env("NOSH_WINDOWS_PROBE", "vt")
        .env("NOSH_WINDOWS_PID_FILE", &path)
        .env("TERM", "xterm-256color");
    let mut terminal = console::Terminal::spawn(command);
    terminal.finish();
    let flags: Vec<bool> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(flags, [true, true, true]);
}

fn running(pid: u32) -> bool {
    use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
    use windows_sys::Win32::Foundation::ERROR_INVALID_PARAMETER;
    use windows_sys::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: this query never signals a process and owns the returned handle.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
    if handle.is_null() {
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(ERROR_INVALID_PARAMETER as i32)
        );
        return false;
    }
    let handle = unsafe { OwnedHandle::from_raw_handle(handle) };
    let mut code = 0;
    assert_ne!(
        unsafe { GetExitCodeProcess(handle.as_raw_handle(), &raw mut code) },
        0
    );
    code == 259
}

fn owned_pids(path: &Path) -> [u32; 2] {
    let pids: Vec<u32> = fs::read_to_string(path)
        .unwrap()
        .split_whitespace()
        .map(|pid| pid.parse().unwrap())
        .collect();
    let pids: [u32; 2] = pids
        .try_into()
        .expect("helper must publish both owned PIDs");
    assert!(pids.iter().all(|pid| *pid != 0));
    assert_ne!(pids[0], pids[1]);
    pids
}

fn tree_command(pid_file: &Path) -> String {
    format!(
        "NOSH_WINDOWS_PROBE=tree NOSH_WINDOWS_PID_FILE={} {} --exact native_child_helper --nocapture",
        quote(pid_file),
        quote(&std::env::current_exe().unwrap())
    )
}

#[test]
fn agent_timeout_reclaims_descendants_without_touching_an_unrelated_child() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pids");
    let mut unrelated = OwnedProbe(
        helper_command()
            .env("NOSH_WINDOWS_PROBE", "sleep")
            .spawn()
            .unwrap(),
    );
    let mut shell = shell(home.path());
    let started = Instant::now();
    let timeout = Duration::from_secs(5);
    let result = shell
        .run_agent_command(
            &tree_command(&pid_file),
            &AgentExecOpts {
                timeout,
                ..Default::default()
            },
            &mut NullSink,
        )
        .unwrap();
    assert!(result.timed_out, "{result:?}");
    assert_eq!(result.exit_code, 124);
    assert!(started.elapsed() < timeout + Duration::from_secs(4));
    for pid in owned_pids(&pid_file) {
        assert!(!running(pid), "owned helper {pid} survived timeout");
    }
    assert!(unrelated.0.try_wait().unwrap().is_none());
    assert_eq!(
        shell
            .run_agent_command("printf usable", &AgentExecOpts::default(), &mut NullSink)
            .unwrap()
            .stdout,
        "usable"
    );
}

#[test]
fn cancellation_restores_function_scopes_and_reclaims_the_owned_tree() {
    let home = tempfile::tempdir().unwrap();
    let pid_file = home.path().join("pids");
    let mut shell = shell(home.path());
    assert_eq!(shell.run_user_line("NATIVE_SCOPE=outer").exit_code, 0);
    let interrupts = shell.interrupts();
    let watched = pid_file.clone();
    let cancellation = std::thread::spawn(move || {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !watched.is_file() {
            assert!(Instant::now() < deadline, "native helper did not start");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(owned_pids(&watched).into_iter().all(running));
        interrupts.fire();
    });
    let result = shell
        .run_agent_command(
            &format!(
                "f() {{ local NATIVE_SCOPE=inner; {}; }}; f",
                tree_command(&pid_file)
            ),
            &AgentExecOpts::default(),
            &mut NullSink,
        )
        .unwrap();
    cancellation.join().unwrap();
    assert!(result.interrupted, "{result:?}");
    assert_eq!(result.exit_code, 130);
    assert_eq!(shell.var("NATIVE_SCOPE").as_deref(), Some("outer"));
    for pid in owned_pids(&pid_file) {
        assert!(!running(pid));
    }
}

#[test]
fn agent_rejects_detached_shell_tasks_before_starting_them() {
    let home = tempfile::tempdir().unwrap();
    let mut shell = shell(home.path());
    for script in [
        "printf forbidden > background.txt &",
        "printf '%s' <(printf forbidden > background.txt)",
    ] {
        let result = shell
            .run_agent_command(script, &AgentExecOpts::default(), &mut NullSink)
            .unwrap();
        assert_ne!(result.exit_code, 0, "{result:?}");
        assert!(result.stderr.contains("host context"), "{result:?}");
        assert!(!home.path().join("background.txt").exists());
    }
}

#[test]
fn a_finished_execution_scope_cannot_spawn_a_late_child() {
    let home = tempfile::tempdir().unwrap();
    let group = nosh_platform::process::ProcessGroup::new(None).unwrap();
    group.finish().unwrap();
    let mut command = nosh(home.path());
    command
        .current_dir(home.path())
        .args(["-c", "printf bad > late.txt"]);
    let error = group.spawn(&mut command).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert!(!home.path().join("late.txt").exists());
    assert_eq!(group.active().unwrap(), 0);
}

#[test]
fn agent_children_cannot_read_the_users_console_directly() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("console.json");
    let mut shell = shell(home.path());
    let command = format!(
        "NOSH_WINDOWS_PROBE=console NOSH_WINDOWS_PID_FILE={} {} --exact native_child_helper --nocapture",
        quote(&path),
        quote(&std::env::current_exe().unwrap())
    );
    let result = shell
        .run_agent_command(&command, &AgentExecOpts::default(), &mut NullSink)
        .unwrap();
    assert_eq!(result.exit_code, 0, "{result:?}");
    let handles: Vec<bool> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(handles, [false, false, false, false]);
}

#[test]
fn background_probes_do_not_allocate_a_console() {
    let home = tempfile::tempdir().unwrap();
    for compose_first in [false, true] {
        let path = home.path().join(format!("probe-{compose_first}.json"));
        let mut command = if compose_first {
            helper_command()
        } else {
            let mut command =
                nosh_platform::process::background_command(std::env::current_exe().unwrap());
            command.args(["--exact", "native_child_helper", "--nocapture"]);
            command
        };
        command
            .env_clear()
            .env("NOSH_WINDOWS_PROBE", "console")
            .env("NOSH_WINDOWS_PID_FILE", &path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        nosh_platform::process::inherit_runtime_environment(&mut command).unwrap();
        if compose_first {
            nosh_platform::process::prepare_background(&mut command);
        }
        assert!(command.status().unwrap().success());
        let handles: Vec<bool> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
        assert_eq!(handles, [false, false, false, false]);
    }
}

#[test]
fn configuration_updates_preserve_content_and_reject_shared_or_readonly_files() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("config.toml");
    let initial = "# keep this comment\n[agent]\nmax_steps = 6\n";
    fs::write(&path, initial).unwrap();
    let output = nosh(home.path())
        .args(["theme", "set", "soft-light"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    let saved = fs::read_to_string(&path).unwrap();
    assert!(saved.contains("# keep this comment"));
    assert!(saved.contains("max_steps = 6"));
    assert!(saved.contains("soft-light"));
    assert!(nosh_platform::fs::owned_by_current_user(&fs::File::open(&path).unwrap()).unwrap());
    let alias = home.path().join("shared.toml");
    fs::hard_link(&path, &alias).unwrap();
    let output = nosh(home.path())
        .args(["theme", "set", "soft-dark"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("one hard link"));
    assert_eq!(fs::read_to_string(&path).unwrap(), saved);
    fs::remove_file(alias).unwrap();
    let mut permissions = fs::metadata(&path).unwrap().permissions();
    permissions.set_readonly(true);
    fs::set_permissions(&path, permissions.clone()).unwrap();
    let output = nosh(home.path())
        .args(["theme", "set", "soft-dark"])
        .output()
        .unwrap();
    permissions.set_readonly(false);
    fs::set_permissions(&path, permissions).unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("read-only"));
    assert_eq!(fs::read_to_string(path).unwrap(), saved);
}

#[test]
fn absent_models_and_unsupported_private_transports_fail_explicitly() {
    let home = tempfile::tempdir().unwrap();
    let output = nosh(home.path())
        .env_remove("NOSH_DISABLE_AI")
        .args(["--offline", "-s", "list files"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(!String::from_utf8_lossy(&output.stderr).contains("panicked"));
    let output = nosh(home.path())
        .env("NOSH_INTERNAL_PTY_FD", "1")
        .args(["-c", "printf must-not-run"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("not available on Windows"));
    let output = nosh(home.path())
        .env_remove("NOSH_DISABLE_AI")
        .env("NOSH_EVAL_WORKER", "unused")
        .args(["--offline", "-s", "list files"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("not supported on Windows"));
}

#[test]
fn real_console_edits_completes_executes_and_cancels_without_a_model() {
    let home = tempfile::tempdir().unwrap();
    fs::write(home.path().join("completion-fixture.txt"), "").unwrap();
    let mut command = nosh(home.path());
    command
        .args(["--safe", "--offline"])
        .current_dir(home.path());
    let mut terminal = console::Terminal::spawn(command);
    terminal.wait_for_prompt();
    terminal.send(b"printf 'PTY-%s\\n' WIN\r");
    terminal.expect("PTY-WIN");
    terminal.send(b"printf 'FILE:%s\\n' completion-fixt");
    terminal.send(b"\t");
    terminal.expect("completion-fixture.txt");
    terminal.send(b"\r");
    terminal.expect("FILE:completion-fixture.txt");
    terminal.send(b"printf bad > cancelled.txt");
    terminal.expect("cancelled.txt");
    terminal.control_c();
    terminal.expect("NATIVE>");
    terminal.send(b"printf 'AFTER-%s\\n' CANCEL\r");
    terminal.expect("AFTER-CANCEL");
    terminal.send(b"exit\r");
    terminal.finish();
    assert!(!home.path().join("cancelled.txt").exists());
    assert!(
        !terminal.text().contains("worker startup timed out"),
        "{}",
        terminal.text()
    );
}

#[test]
fn quoted_native_paths_complete_without_losing_backslash_directories() {
    let home = tempfile::tempdir().unwrap();
    let folder = home.path().join("path space");
    fs::create_dir(&folder).unwrap();
    let file = folder.join("fixture.txt");
    fs::write(&file, "").unwrap();
    let mut command = nosh(home.path());
    command
        .args(["--safe", "--offline"])
        .current_dir(home.path());
    let mut terminal = console::Terminal::spawn(command);
    terminal.wait_for_prompt();
    for (prefix, expected) in [
        (
            r"path space\fi".to_string(),
            r"path space\fixture.txt".to_string(),
        ),
        (
            format!(r"{}\fi", folder.display()),
            file.to_string_lossy().into_owned(),
        ),
    ] {
        for delimiter in ['\'', '"'] {
            let input = format!("printf '%s' {delimiter}{prefix}");
            terminal.send(input.as_bytes());
            terminal.send(b"\t");
            terminal.expect("fixture.txt");
            terminal.send(b" > completed.txt; printf 'COMPLETE-%s\\n' READY\r");
            terminal.expect("COMPLETE-READY");
            assert_eq!(
                fs::read_to_string(home.path().join("completed.txt")).unwrap(),
                expected
            );
            terminal.expect("NATIVE>");
        }
    }
    terminal.send(b"exit 0\r");
    terminal.finish();
}

#[test]
fn native_completion_providers_restore_the_required_runtime_environment() {
    let home = tempfile::tempdir().unwrap();
    let bin = home.path().join("bin");
    fs::create_dir(&bin).unwrap();
    fs::copy(std::env::current_exe().unwrap(), bin.join("git.exe")).unwrap();
    fs::write(
        bin.join("git.cmd"),
        "@echo off\r\nif not defined SystemRoot exit /b 17\r\nif \"%~1\"==\"--version\" (\r\n echo git version 2.43.0\r\n) else (\r\n echo runtime-branch\r\n)\r\n",
    )
    .unwrap();
    let mut command = nosh(home.path());
    command
        .args(["--safe", "--offline"])
        .current_dir(home.path())
        .env("PATH", &bin)
        .env("PATHEXT", ".CMD;.EXE");
    let mut terminal = console::Terminal::spawn(command);
    terminal.wait_for_prompt();
    terminal.send(b"git switch runtime-\t");
    terminal.expect("runtime-branch");
    terminal.control_c();
    terminal.expect("NATIVE>");
    terminal.send(b"git --version\r");
    terminal.expect("git version 2.43.0");
    terminal.expect("NATIVE>");
    terminal.send(b"exit 0\r");
    terminal.finish();
}

#[test]
fn scripted_completion_children_do_not_allocate_a_console() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("completion-console.json");
    let mut command = nosh(home.path());
    command
        .args(["--safe", "--offline"])
        .current_dir(home.path());
    let mut terminal = console::Terminal::spawn(command);
    terminal.wait_for_prompt();
    let setup = format!(
        "__quiet_complete() {{ NOSH_WINDOWS_PROBE=console NOSH_WINDOWS_PID_FILE={} {} --exact native_child_helper --nocapture > NUL 2>&1; COMPREPLY=(quiet-candidate); }}; complete -F __quiet_complete probe; printf 'SETUP-%s\\n' DONE\r",
        quote(&path),
        quote(&std::env::current_exe().unwrap()),
    );
    terminal.send(setup.as_bytes());
    terminal.expect("SETUP-DONE");
    terminal.expect("NATIVE>");
    terminal.send(b"probe \t");
    terminal.expect("quiet-candidate");
    let handles: Vec<bool> = serde_json::from_slice(&fs::read(path).unwrap()).unwrap();
    assert_eq!(handles, [false, false, false, false]);
    terminal.control_c();
    terminal.expect("NATIVE>");
    terminal.send(b"exit 0\r");
    terminal.finish();
}

#[test]
fn scripted_completion_reclaims_orphaned_descendants() {
    let home = tempfile::tempdir().unwrap();
    let path = home.path().join("completion-pids");
    let mut command = nosh(home.path());
    command
        .args(["--safe", "--offline"])
        .current_dir(home.path());
    let mut terminal = console::Terminal::spawn(command);
    terminal.wait_for_prompt();
    let setup = format!(
        "__orphan_complete() {{ NOSH_WINDOWS_PROBE=orphan NOSH_WINDOWS_PID_FILE={} {} --exact native_child_helper --nocapture > NUL 2>&1; COMPREPLY=(must-not-accept); }}; complete -F __orphan_complete probe; printf 'SETUP-%s\\n' DONE\r",
        quote(&path),
        quote(&std::env::current_exe().unwrap()),
    );
    terminal.send(setup.as_bytes());
    terminal.expect("SETUP-DONE");
    terminal.expect("NATIVE>");
    terminal.send(b"probe \t");
    terminal.expect("provider left background processes");
    let pids = owned_pids(&path);
    let deadline = Instant::now() + Duration::from_secs(5);
    while pids.iter().copied().any(running) {
        assert!(
            Instant::now() < deadline,
            "completion descendant survived cleanup"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    terminal.control_c();
    terminal.expect("NATIVE>");
    terminal.send(b"exit 0\r");
    terminal.finish();
}
