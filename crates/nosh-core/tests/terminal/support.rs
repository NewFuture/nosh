//! Real descriptors and controlling terminals, without loading a model.

pub(super) use std::fs::File;
pub(super) use std::io::{Read, Write};
pub(super) use std::os::fd::{AsRawFd, FromRawFd};
pub(super) use std::os::unix::process::CommandExt;
pub(super) use std::process::{Command, Stdio};
pub(super) use std::sync::{Arc, Mutex};
pub(super) use std::thread;
pub(super) use std::time::{Duration, Instant};

pub(super) use nosh_core::{AgentUi, JsonUi, TermUi};
pub(super) use nosh_hub::{BarProgress, Progress};
pub(super) use nosh_permissions::Risk;
pub(super) use nosh_shell::{style, term};

pub(super) const BEGIN: &str = "nosh-terminal-probe-begin\n";
pub(super) const END: &str = "nosh-terminal-probe-end";
pub(super) const TOOL_LABELS: [(Risk, &str); 4] = [
    (Risk::Safe, "SAFE \u{b7} auto"),
    (Risk::Mutating, "MUTATING \u{b7} approved"),
    (Risk::Dangerous, "DANGEROUS \u{b7} allowed (yolo)"),
    (Risk::Forbidden, "FORBIDDEN \u{b7} denied"),
];

pub(super) struct Pty {
    master: File,
    slave: File,
}

pub(super) fn size(columns: u16) -> libc::winsize {
    libc::winsize {
        ws_row: 24,
        ws_col: columns,
        ws_xpixel: 0,
        ws_ypixel: 0,
    }
}

impl Pty {
    fn new(columns: u16) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut window = size(columns);
        // SAFETY: the output pointers and window size are valid; optional
        // name and termios arguments are null.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &raw mut window,
                )
            },
            0,
            "{}",
            std::io::Error::last_os_error()
        );
        for fd in [master, slave] {
            // SAFETY: openpty returned valid descriptors.
            assert_ne!(
                unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) },
                -1
            );
        }
        // SAFETY: each descriptor has exactly one owner after openpty.
        unsafe {
            Self {
                master: File::from_raw_fd(master),
                slave: File::from_raw_fd(slave),
            }
        }
    }
}

pub(super) fn resize(fd: i32, columns: u16) {
    // SAFETY: callers supply an open PTY descriptor and a valid winsize.
    assert_eq!(
        unsafe { libc::ioctl(fd, libc::TIOCSWINSZ, &size(columns)) },
        0
    );
}

pub(super) fn read_output(mut reader: impl Read, observed: Arc<Mutex<Vec<u8>>>) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                let mut observed = observed.lock().unwrap();
                assert!(
                    observed.len() + n < 32 * 1024 * 1024,
                    "terminal capture limit"
                );
                observed.extend_from_slice(&chunk[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                // Linux PTYs report EIO rather than EOF when the slave is closed.
                assert_eq!(e.raw_os_error(), Some(libc::EIO), "{e}");
                break;
            }
        }
    }
    String::from_utf8(bytes).unwrap().replace("\r\n", "\n")
}

pub(super) fn framed(output: &str) -> String {
    output
        .split_once(BEGIN)
        .unwrap_or_else(|| panic!("{output:?}"))
        .1
        .split_once(END)
        .unwrap_or_else(|| panic!("{output:?}"))
        .0
        .to_string()
}

pub(super) type KeyStep<'a> = (&'a str, &'a [u8]);

pub(super) struct ProbeTimings {
    pub(super) startup: Duration,
    pub(super) input: Vec<Duration>,
}

pub(super) struct Probe<'a> {
    pub(super) mode: &'a str,
    pub(super) terminal: Option<&'a str>,
    pub(super) locale: Option<&'a str>,
    pub(super) no_color: &'a str,
    pub(super) stdout_tty: bool,
    pub(super) stderr_tty: bool,
    pub(super) stdin_pipe: bool,
    pub(super) columns: u16,
    pub(super) initial: &'a str,
    pub(super) keys: Option<&'a [u8]>,
    pub(super) steps: &'a [KeyStep<'a>],
    pub(super) input_assist: bool,
}

impl Default for Probe<'_> {
    fn default() -> Self {
        Self {
            mode: "text",
            terminal: Some("xterm-256color"),
            locale: Some("C.UTF-8"),
            no_color: "",
            stdout_tty: false,
            stderr_tty: false,
            stdin_pipe: false,
            columns: 80,
            initial: "",
            keys: None,
            steps: &[],
            input_assist: true,
        }
    }
}

impl Probe<'_> {
    pub(super) fn run(self) -> (String, String) {
        let (out, err, _) = self.run_with_timings();
        (out, err)
    }

    pub(super) fn run_with_timings(self) -> (String, String, ProbeTimings) {
        let home = tempfile::tempdir().unwrap();
        let stdout = self.stdout_tty.then(|| Pty::new(80));
        let stderr = self.stderr_tty.then(|| Pty::new(self.columns));
        let input = stderr.as_ref().or(stdout.as_ref());
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "terminal_probe", "--nocapture"])
            .env("NOSH_TERMINAL_PROBE", self.mode)
            .env("NOSH_TERMINAL_INITIAL", self.initial)
            .env(
                "NOSH_TERMINAL_INPUT_ASSIST",
                if self.input_assist { "1" } else { "0" },
            )
            .env_remove("LC_ALL")
            .env_remove("LC_CTYPE")
            .env_remove("LANG")
            .env("NO_COLOR", self.no_color)
            .env("NOSH_LANG", "en")
            .env("NOSH_HOME", home.path())
            .env("HOME", home.path())
            .env_remove("CLICOLOR")
            .env_remove("NOSH_STATS")
            .stdin(if self.stdin_pipe {
                Stdio::piped()
            } else {
                input.map_or_else(Stdio::null, |t| t.slave.try_clone().unwrap().into())
            })
            .stdout(
                stdout
                    .as_ref()
                    .map_or_else(Stdio::piped, |t| t.slave.try_clone().unwrap().into()),
            )
            .stderr(
                stderr
                    .as_ref()
                    .map_or_else(Stdio::piped, |t| t.slave.try_clone().unwrap().into()),
            );
        match self.terminal {
            Some(term) => {
                command.env("TERM", term);
            }
            None => {
                command.env_remove("TERM");
            }
        }
        if let Some(locale) = self.locale {
            command.env("LC_ALL", locale);
        }
        if input.is_some() {
            let controlling_fd = if self.stderr_tty { 2 } else { 1 };
            // SAFETY: only async-signal-safe syscalls run between fork/exec.
            unsafe {
                command.pre_exec(move || {
                    if libc::setsid() == -1
                        || libc::ioctl(controlling_fd, libc::TIOCSCTTY as _, 0) == -1
                    {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        let mut input = input.map(|t| t.master.try_clone().unwrap());
        let launched = Instant::now();
        let mut child = command.spawn().unwrap();
        drop(command);
        if self.stdin_pipe {
            child
                .stdin
                .take()
                .unwrap()
                .write_all(b"attachment\n")
                .unwrap();
        }
        let out: Box<dyn Read + Send> = match stdout {
            Some(t) => {
                drop(t.slave);
                Box::new(t.master)
            }
            None => Box::new(child.stdout.take().unwrap()),
        };
        let err: Box<dyn Read + Send> = match stderr {
            Some(t) => {
                drop(t.slave);
                Box::new(t.master)
            }
            None => Box::new(child.stderr.take().unwrap()),
        };
        let observed = Arc::new(Mutex::new(Vec::new()));
        let out_observed = observed.clone();
        let err_observed = observed.clone();
        let out = thread::spawn(move || read_output(out, out_observed));
        let err = thread::spawn(move || read_output(err, err_observed));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut keys = self.keys;
        let mut steps = self.steps.iter();
        let mut next = steps.next();
        let mut observed_start = 0;
        let mut sent = Instant::now();
        let mut timings = Vec::new();
        let mut startup = None;
        let mut blocked_at = None;
        let marker = blocked_input_marker(child.id());
        let mut terminal_scan = 0;
        let mut cursor_replies = 0;
        let needs_cursor_reply = self.mode.starts_with("repl")
            && self
                .terminal
                .is_some_and(|t| !matches!(t, "" | "dumb" | "unknown"));
        let status = loop {
            if let Some(input) = input.as_mut() {
                let requests = {
                    let bytes = observed.lock().unwrap();
                    let requests = bytes[terminal_scan..]
                        .windows(4)
                        .filter(|s| *s == b"\x1b[6n")
                        .count();
                    terminal_scan = bytes.len().saturating_sub(3);
                    requests
                };
                for _ in 0..requests {
                    input.write_all(b"\x1b[1;1R").unwrap();
                    cursor_replies += 1;
                }
            }
            if (keys.is_some() || next.is_some()) && (!needs_cursor_reply || cursor_replies > 0) {
                let input = input.as_mut().unwrap();
                let mut attrs = std::mem::MaybeUninit::<libc::termios>::uninit();
                // SAFETY: tcgetattr writes to valid storage, read only on success.
                if unsafe { libc::tcgetattr(input.as_raw_fd(), attrs.as_mut_ptr()) } == 0
                    && unsafe { attrs.assume_init() }.c_lflag & libc::ICANON == 0
                {
                    startup.get_or_insert_with(|| launched.elapsed());
                    if let Some(bytes) = keys.take() {
                        observed_start = observed.lock().unwrap().len();
                        sent = Instant::now();
                        input.write_all(bytes).unwrap();
                    } else if let Some((needle, bytes)) = next {
                        let ready = if *needle == "@worker-blocked" {
                            marker.exists()
                        } else {
                            String::from_utf8_lossy(&observed.lock().unwrap()[observed_start..])
                                .contains(needle)
                        };
                        if ready {
                            if *needle == "@worker-blocked" {
                                blocked_at = Some(Instant::now());
                            } else {
                                timings.push(sent.elapsed());
                            }
                            observed_start = observed.lock().unwrap().len();
                            sent = Instant::now();
                            input.write_all(bytes).unwrap();
                            next = steps.next();
                        }
                    }
                }
            }
            if let Some(status) = child.try_wait().unwrap() {
                break status;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                if marker.exists() {
                    std::fs::remove_file(&marker).unwrap();
                }
                panic!(
                    "terminal probe timed out: {:?} {:?}",
                    out.join(),
                    err.join()
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        let exited = Instant::now();
        let out = out.join().unwrap();
        let err = err.join().unwrap();
        if let Some(blocked_at) = blocked_at {
            std::fs::remove_file(marker).unwrap();
            assert!(
                exited.duration_since(blocked_at) < Duration::from_millis(500),
                "editing/cancel/exit waited for a diagnostic timeout: {:?}",
                exited.duration_since(blocked_at)
            );
            println!(
                "input_assist_blocked_edit_cancel_exit_us={}",
                exited.duration_since(blocked_at).as_micros()
            );
        }
        assert!(status.success(), "{status}: {out:?}\n{err:?}");
        assert!(next.is_none(), "probe exited before staged input completed");
        (
            framed(&out),
            framed(&err),
            ProbeTimings {
                startup: startup.unwrap_or_default(),
                input: timings,
            },
        )
    }
}

pub(super) fn blocked_input_marker(parent: u32) -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp").join(format!("nosh-pty-input-blocked-{parent}"))
}
