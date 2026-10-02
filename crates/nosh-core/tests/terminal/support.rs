//! Real descriptors and controlling terminals, without loading a model.

pub(super) use std::fs::File;
pub(super) use std::io::{Read, Write};
pub(super) use std::os::fd::{AsRawFd, FromRawFd};
pub(super) use std::os::unix::process::CommandExt;
pub(super) use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
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

#[derive(Clone)]
struct ScreenTracking {
    columns: Arc<AtomicUsize>,
    replies: Arc<Mutex<Vec<Vec<u8>>>>,
    acknowledged_columns: Arc<AtomicUsize>,
}

#[derive(Debug, Default)]
struct ScreenObservation {
    frames: Vec<super::screen::Frame>,
    printed: Vec<super::screen::Printed>,
}

fn read_output(
    mut reader: impl Read,
    observed: Arc<Mutex<Vec<u8>>>,
    tracking: Option<ScreenTracking>,
) -> (String, ScreenObservation) {
    let mut bytes = Vec::new();
    let mut chunk = [0; 8192];
    let mut screen = tracking.as_ref().map(|tracking| {
        super::screen::Screen::new(
            tracking.columns.load(Ordering::Acquire),
            tracking.replies.clone(),
            tracking.acknowledged_columns.clone(),
        )
    });
    let mut parser = vte::Parser::<0>::new_with_size();
    let mut columns = tracking
        .as_ref()
        .map_or(0, |tracking| tracking.columns.load(Ordering::Acquire));
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if let (Some(screen), Some(tracking)) = (&mut screen, &tracking) {
                    let current = tracking.columns.load(Ordering::Acquire);
                    if current != columns {
                        screen.resize(current);
                        columns = current;
                    }
                    parser.advance(screen, &chunk[..n]);
                }
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
    let observation = screen.map_or_else(ScreenObservation::default, |screen| ScreenObservation {
        frames: screen.frames,
        printed: screen.printed,
    });
    (
        String::from_utf8(bytes).unwrap().replace("\r\n", "\n"),
        observation,
    )
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
    pub(super) frames: Vec<super::screen::Frame>,
    pub(super) printed: Vec<super::screen::Printed>,
}

pub(super) struct Probe<'a> {
    pub(super) mode: &'a str,
    pub(super) terminal: Option<&'a str>,
    pub(super) locale: Option<&'a str>,
    pub(super) no_color: &'a str,
    pub(super) clicolor: Option<&'a str>,
    pub(super) colorterm: Option<&'a str>,
    pub(super) stdout_tty: bool,
    pub(super) stderr_tty: bool,
    pub(super) stdin_pipe: bool,
    pub(super) columns: u16,
    pub(super) initial: &'a str,
    pub(super) keys: Option<&'a [u8]>,
    pub(super) steps: &'a [KeyStep<'a>],
    pub(super) input_assist: bool,
    pub(super) track_frames: bool,
    pub(super) enhanced_keyboard: bool,
    pub(super) resizes: &'a [(usize, u16)],
}

impl Default for Probe<'_> {
    fn default() -> Self {
        Self {
            mode: "text",
            terminal: Some("xterm-256color"),
            locale: Some("C.UTF-8"),
            no_color: "",
            clicolor: None,
            colorterm: None,
            stdout_tty: false,
            stderr_tty: false,
            stdin_pipe: false,
            columns: 80,
            initial: "",
            keys: None,
            steps: &[],
            input_assist: true,
            track_frames: false,
            enhanced_keyboard: false,
            resizes: &[],
        }
    }
}

impl Probe<'_> {
    pub(super) fn run(self) -> (String, String) {
        let (out, err, _) = self.run_with_timings();
        (out, err)
    }

    pub(super) fn run_with_timings(self) -> (String, String, ProbeTimings) {
        assert!(
            self.resizes.is_empty() || self.track_frames,
            "resize testing requires screen acknowledgement"
        );
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
            .env_remove("COLORTERM")
            .env_remove("WT_SESSION")
            .env_remove("NOSH_STATS")
            .env_remove("TMUX")
            .env_remove("STY")
            .env_remove("TERM_PROGRAM")
            .env_remove("VISUAL")
            .env_remove("EDITOR")
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
        if let Some(clicolor) = self.clicolor {
            command.env("CLICOLOR", clicolor);
        }
        if let Some(colorterm) = self.colorterm {
            command.env("COLORTERM", colorterm);
        }
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
        let tracking = self.track_frames.then(|| ScreenTracking {
            columns: Arc::new(AtomicUsize::new(usize::from(self.columns))),
            replies: Arc::new(Mutex::new(Vec::new())),
            acknowledged_columns: Arc::new(AtomicUsize::new(0)),
        });
        let err_tracking = tracking.clone();
        let out = thread::spawn(move || read_output(out, out_observed, None));
        let err = thread::spawn(move || read_output(err, err_observed, err_tracking));
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
        let mut keyboard_replies = 0;
        let mut step_index = 0;
        let mut theme_revision = 0;
        let mut resize_requested = None;
        let needs_cursor_reply = self.mode.starts_with("repl")
            && self
                .terminal
                .is_some_and(|t| !matches!(t, "" | "dumb" | "unknown"));
        let status = loop {
            if let Some(input) = input.as_mut() {
                if self.enhanced_keyboard {
                    let requests = observed
                        .lock()
                        .unwrap()
                        .windows(4)
                        .filter(|bytes| *bytes == b"\x1b[?u")
                        .count();
                    for _ in keyboard_replies..requests {
                        input.write_all(b"\x1b[?1u\x1b[?1;2c").unwrap();
                    }
                    keyboard_replies = requests;
                }
                if let Some(tracking) = &tracking {
                    for reply in std::mem::take(&mut *tracking.replies.lock().unwrap()) {
                        input.write_all(&reply).unwrap();
                        cursor_replies += 1;
                    }
                } else {
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
                        let ready = if resize_requested == Some(step_index) {
                            true
                        } else if *needle == "@worker-blocked" {
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
                            if let Some((_, columns)) =
                                self.resizes.iter().find(|(index, _)| *index == step_index)
                            {
                                let tracking = tracking.as_ref().expect("resizes require tracking");
                                if resize_requested != Some(step_index) {
                                    tracking
                                        .columns
                                        .store(usize::from(*columns), Ordering::Release);
                                    resize(input.as_raw_fd(), *columns);
                                    resize_requested = Some(step_index);
                                }
                                if tracking.acknowledged_columns.load(Ordering::Acquire)
                                    != usize::from(*columns)
                                {
                                    thread::sleep(Duration::from_millis(5));
                                    continue;
                                }
                            }
                            if *bytes == b"@theme-switch" {
                                assert_eq!(self.mode, "repl-inline-theme");
                                theme_revision += 1;
                                std::fs::write(
                                    home.path().join(format!("theme-request-{theme_revision}")),
                                    "",
                                )
                                .unwrap();
                            } else {
                                input.write_all(bytes).unwrap();
                            }
                            next = steps.next();
                            step_index += 1;
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
                let (out, _) = out.join().unwrap();
                let (err, observation) = err.join().unwrap();
                panic!(
                    "terminal probe timed out: {out:?} {err:?}; observed {} frames, last cursor {:?}",
                    observation.frames.len(),
                    observation.frames.last().map(|frame| frame.cursor)
                );
            }
            thread::sleep(Duration::from_millis(5));
        };
        let exited = Instant::now();
        let (out, _) = out.join().unwrap();
        let (err, observation) = err.join().unwrap();
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
                frames: observation.frames,
                printed: observation.printed,
            },
        )
    }
}

pub(super) fn blocked_input_marker(parent: u32) -> std::path::PathBuf {
    std::path::PathBuf::from("/tmp").join(format!("nosh-pty-input-blocked-{parent}"))
}
