#![cfg(feature = "gpui")]

use nosh_desktop::local_shell;
use nosh_desktop::terminal::{PtyEvent, TerminalBackend};
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

struct Bundle {
    _directory: tempfile::TempDir,
    home: tempfile::TempDir,
    cwd: tempfile::TempDir,
    executable: PathBuf,
}

impl Bundle {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let source = std::env::var_os("NOSH_GPUI_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_nosh-gpui")));
        let executable = directory.path().join(if cfg!(windows) {
            "standalone %NOSH_HOME%! & ^ (中文) desktop.exe"
        } else {
            "standalone-desktop"
        });
        std::fs::copy(source, &executable).unwrap();
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        Self {
            _directory: directory,
            executable,
            home: tempfile::tempdir().unwrap(),
            cwd: tempfile::tempdir().unwrap(),
        }
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(self.cwd.path())
            .env("NOSH_HOME", self.home.path())
            .env("NOSH_DISABLE_AI", "1")
            .env("PATH", "");
        command
    }

    fn terminal(&self, id: u64) -> (TerminalBackend, mpsc::Receiver<(u64, PtyEvent)>) {
        let home = self.home.path().join(id.to_string());
        std::fs::create_dir(&home).unwrap();
        let mut settings = local_shell::backend(&self.executable, self.cwd.path()).unwrap();
        settings.env.extend([
            ("NOSH_HOME".into(), home.to_string_lossy().into_owned()),
            ("NOSH_DISABLE_AI".into(), "1".into()),
            ("HOME".into(), home.to_string_lossy().into_owned()),
            ("USERPROFILE".into(), home.to_string_lossy().into_owned()),
            ("COMSPEC".into(), "must-not-run.exe".into()),
            ("ERRORLEVEL".into(), "923".into()),
            (
                "NOSH_EMBEDDED_PROGRAM".into(),
                self.executable.to_string_lossy().into_owned(),
            ),
            ("PATH".into(), String::new()),
            ("PS1".into(), "ONEFILE> ".into()),
        ]);
        let (sender, events) = mpsc::channel();
        (
            TerminalBackend::with_wake(id, Arc::new(|| {}), sender, settings).unwrap(),
            events,
        )
    }
}

fn output(mut command: Command) -> (i32, String, String) {
    let mut stdout = tempfile::tempfile().unwrap();
    let mut stderr = tempfile::tempfile().unwrap();
    let mut child = command
        .stdin(Stdio::null())
        .stdout(stdout.try_clone().unwrap())
        .stderr(stderr.try_clone().unwrap())
        .spawn()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        if Instant::now() > deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("embedded role did not exit; it may have reopened the GUI");
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    use std::io::{Seek, SeekFrom};
    stdout.seek(SeekFrom::Start(0)).unwrap();
    stderr.seek(SeekFrom::Start(0)).unwrap();
    let mut out = String::new();
    let mut err = String::new();
    assert!(stdout.metadata().unwrap().len() <= 65536);
    assert!(stderr.metadata().unwrap().len() <= 65536);
    stdout.take(65536).read_to_string(&mut out).unwrap();
    stderr.take(65536).read_to_string(&mut err).unwrap();
    (status.code().unwrap(), out, err)
}

#[test]
fn desktop_rejects_external_shell_override() {
    let bundle = Bundle::new();
    let mut help = bundle.command();
    help.env("NOSH_BIN", "must-not-run.exe").arg("--help");
    let (code, stdout, stderr) = output(help);
    assert_eq!(code, 0, "{stderr}");
    assert!(!stdout.contains("--nosh"));
    assert!(stdout.contains("--cwd"));

    let mut override_command = bundle.command();
    override_command.args(["--nosh", "must-not-run.exe"]);
    let (code, _, stderr) = output(override_command);
    assert_eq!(code, 2);
    assert!(stderr.contains("unexpected argument '--nosh'"), "{stderr}");
}

fn screen_text(terminal: &TerminalBackend) -> String {
    terminal
        .model()
        .lock()
        .grid()
        .display_iter()
        .filter(|cell| {
            !cell
                .flags
                .contains(alacritty_terminal::term::cell::Flags::WIDE_CHAR_SPACER)
        })
        .map(|cell| cell.c)
        .collect()
}

fn wait_for(terminal: &TerminalBackend, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while !screen_text(terminal).contains(needle) {
        assert!(
            Instant::now() < deadline,
            "missing {needle:?}:\n{}",
            screen_text(terminal)
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(windows)]
struct ShellProcess {
    handle: std::os::windows::io::OwnedHandle,
    pid: u32,
}

#[cfg(windows)]
impl ShellProcess {
    fn observe(terminal: &TerminalBackend) -> Self {
        terminal
            .write(b"printf 'ONEFILE_%s:%s\\n' PID \"$$\"\r".to_vec())
            .unwrap();
        wait_for(terminal, "ONEFILE_PID:");
        let text = screen_text(terminal);
        let pid: u32 = text
            .split_once("ONEFILE_PID:")
            .unwrap()
            .1
            .chars()
            .take_while(char::is_ascii_digit)
            .collect::<String>()
            .parse()
            .unwrap();
        Self::open(pid)
    }

    fn open(pid: u32) -> Self {
        use std::os::windows::io::FromRawHandle;
        // Only observe exact PIDs reported by the owned shell or its process tree.
        let handle = unsafe { OpenProcess(0x00101000, 0, pid) };
        assert!(
            !handle.is_null(),
            "cannot observe owned shell {pid}: {}",
            std::io::Error::last_os_error()
        );
        Self {
            handle: unsafe { std::os::windows::io::OwnedHandle::from_raw_handle(handle) },
            pid,
        }
    }

    fn image(&self) -> PathBuf {
        use std::os::windows::{ffi::OsStringExt, io::AsRawHandle};
        let mut path = vec![0u16; 32768];
        let mut length = path.len() as u32;
        assert_ne!(
            unsafe {
                windows_sys::Win32::System::Threading::QueryFullProcessImageNameW(
                    self.handle.as_raw_handle(),
                    0,
                    path.as_mut_ptr(),
                    &mut length,
                )
            },
            0
        );
        PathBuf::from(std::ffi::OsString::from_wide(&path[..length as usize]))
    }

    fn family(&self) -> (Self, Vec<Self>) {
        use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        assert_ne!(
            snapshot,
            windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE
        );
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = unsafe { Process32FirstW(snapshot.as_raw_handle(), &mut entry) };
        let mut parent = None;
        let mut children = Vec::new();
        while found != 0 {
            if entry.th32ProcessID == self.pid {
                parent = Some(Self::open(entry.th32ParentProcessID));
            }
            if entry.th32ParentProcessID == self.pid {
                children.push(Self::open(entry.th32ProcessID));
            }
            found = unsafe { Process32NextW(snapshot.as_raw_handle(), &mut entry) };
        }
        let parent = parent.expect("owned shell has no anchor");
        assert!(
            parent
                .image()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with("\\cmd.exe")
        );
        assert!(!children.is_empty(), "expected actual self-exec workers");
        let image = std::fs::canonicalize(self.image()).unwrap();
        for child in &children {
            assert_eq!(std::fs::canonicalize(child.image()).unwrap(), image);
        }
        (parent, children)
    }

    fn assert_exited(self) {
        use std::os::windows::io::AsRawHandle;
        assert_eq!(
            unsafe { WaitForSingleObject(self.handle.as_raw_handle(), 5000) },
            0,
            "owned shell process survived PTY teardown"
        );
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
    fn WaitForSingleObject(handle: *mut std::ffi::c_void, timeout: u32) -> u32;
}

#[test]
fn isolated_executable_dispatches_shell_worker_and_init_without_a_gui() {
    let bundle = Bundle::new();
    let mut command = bundle.command();
    command.args([
        nosh_cli::EMBEDDED_SHELL_FLAG,
        "--safe",
        "-c",
        "printf 'ONEFILE_STDOUT'; printf 'ONEFILE_STDERR' >&2; exit 37",
    ]);
    let (code, out, err) = output(command);
    assert_eq!(
        (code, out.as_str(), err.as_str()),
        (37, "ONEFILE_STDOUT", "ONEFILE_STDERR")
    );
    let mut command = bundle.command();
    command
        .arg("--__nosh_input_worker")
        .env_remove("NOSH_INPUT_WORKER");
    let (code, out, err) = output(command);
    assert_eq!(code, 2);
    assert!(out.is_empty());
    assert!(
        err.contains("missing internal input worker context"),
        "{err}"
    );
    let mut command = bundle.command();
    command.args([nosh_cli::EMBEDDED_SHELL_FLAG, "--safe", "init", "bash"]);
    let (code, script, err) = output(command);
    assert_eq!(code, 0, "{err}");
    assert!(script.contains(nosh_cli::EMBEDDED_SHELL_FLAG));
    let mut command = bundle.command();
    command.args([nosh_cli::EMBEDDED_SHELL_FLAG, "--seed", "not-a-number"]);
    assert_eq!(output(command).0, 2);
}

#[test]
fn isolated_executable_runs_independent_real_ptys_completion_and_exit_codes() {
    let bundle = Bundle::new();
    std::fs::write(bundle.cwd.path().join("bundle-completion-file.txt"), "").unwrap();
    let (first, events) = bundle.terminal(1);
    let (second, _second_events) = bundle.terminal(2);
    wait_for(&first, "ONEFILE>");
    wait_for(&second, "ONEFILE>");
    #[cfg(windows)]
    let first_process = ShellProcess::observe(&first);
    #[cfg(windows)]
    let second_process = ShellProcess::observe(&second);
    first
        .write(b"VALUE=111\rprintf 'ONEFILE_A:%s\\n' \"$VALUE\"\r".to_vec())
        .unwrap();
    second
        .write(b"VALUE=222\rprintf 'ONEFILE_B:%s\\n' \"$VALUE\"\r".to_vec())
        .unwrap();
    wait_for(&first, "ONEFILE_A:111");
    wait_for(&second, "ONEFILE_B:222");
    first
        .write(b"printf 'ONEFILE_FILE:%s\\n' bundle-completion-fi".to_vec())
        .unwrap();
    wait_for(&first, "bundle-completion-fi");
    first.write(b"\t".to_vec()).unwrap();
    wait_for(&first, "bundle-completion-file.txt");
    first.write(b"\r".to_vec()).unwrap();
    wait_for(&first, "ONEFILE_FILE:bundle-completion-file.txt");
    first
        .write("printf '中文:%s\\n' 测试\r".as_bytes().to_vec())
        .unwrap();
    wait_for(&first, "中文:测试");
    first
        .write(b"printf should-not-run > cancelled.txt".to_vec())
        .unwrap();
    wait_for(&first, "cancelled.txt");
    first.write(b"\x03".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    first
        .write(b"printf 'LOOP_%s\\n' STARTED; while :; do :; done\r".to_vec())
        .unwrap();
    wait_for(&first, "LOOP_STARTED");
    first.write(b"\x03".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(150));
    first
        .write(b"printf 'AFTER_%s\\n' INTERRUPT\r".to_vec())
        .unwrap();
    wait_for(&first, "AFTER_INTERRUPT");
    assert!(!bundle.cwd.path().join("cancelled.txt").exists());
    #[cfg(windows)]
    let (first_anchor, first_workers) = first_process.family();
    #[cfg(windows)]
    let (second_anchor, second_workers) = second_process.family();
    assert!(!screen_text(&first).contains("worker startup timed out"));
    assert!(!screen_text(&first).contains("worker allocator is required"));
    first.write(b"exit 37\r".to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Ok((_, PtyEvent::ChildExit(code))) = events.recv_timeout(Duration::from_millis(20)) {
            assert_eq!(code, 37);
            break;
        }
        assert!(Instant::now() < deadline, "PTY shell did not exit");
    }
    drop(first);
    #[cfg(windows)]
    first_process.assert_exited();
    #[cfg(windows)]
    {
        first_anchor.assert_exited();
        for worker in first_workers {
            worker.assert_exited();
        }
    }
    second
        .write(b"printf 'ONEFILE_STILL:%s\\n' \"$VALUE\"\r".to_vec())
        .unwrap();
    wait_for(&second, "ONEFILE_STILL:222");
    let before = Instant::now();
    drop(second);
    assert!(
        before.elapsed() < Duration::from_secs(3),
        "PTY teardown blocked"
    );
    #[cfg(windows)]
    second_process.assert_exited();
    #[cfg(windows)]
    {
        second_anchor.assert_exited();
        for worker in second_workers {
            worker.assert_exited();
        }
    }
}
