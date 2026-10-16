//! Shared controlling terminal, nonblocking I/O and bounded child cleanup for CLI tests.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus};
use std::time::{Duration, Instant};

pub struct Terminal {
    child: Child,
    master: File,
    pub output: Vec<u8>,
    observed: usize,
    replies: usize,
}

impl Terminal {
    pub fn new(mut command: Command, rows: u16, columns: u16) -> Self {
        let mut master = -1;
        let mut slave = -1;
        let mut window = libc::winsize {
            ws_row: rows,
            ws_col: columns,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: output pointers and window are valid; optional arguments are null.
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
        );
        // SAFETY: openpty returned distinct, owned descriptors.
        let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
        // SAFETY: descriptors remain open throughout these calls.
        unsafe {
            for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
                assert_ne!(libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC), -1);
            }
            let flags = libc::fcntl(master.as_raw_fd(), libc::F_GETFL);
            assert_ne!(flags, -1);
            assert_ne!(
                libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK),
                -1,
            );
        }
        command
            .stdin(slave.try_clone().unwrap())
            .stdout(slave.try_clone().unwrap())
            .stderr(slave.try_clone().unwrap());
        // SAFETY: only async-signal-safe syscalls run between fork and exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 || libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        Self {
            child: command.spawn().unwrap(),
            master,
            output: Vec::new(),
            observed: 0,
            replies: 0,
        }
    }

    pub fn send(&mut self, bytes: &[u8]) {
        self.observed = self.output.len();
        self.master.write_all(bytes).unwrap();
    }

    #[allow(dead_code, reason = "only some CLI tests send framed Escape keys")]
    pub fn send_escape(&mut self) {
        // CSI-u keeps Escape distinct from asynchronous cursor-position replies.
        self.send(b"\x1b[27u");
    }

    #[allow(dead_code, reason = "not every CLI test target polls for process exit")]
    pub fn poll(&mut self) -> Option<ExitStatus> {
        self.pump(0);
        std::thread::sleep(Duration::from_millis(5));
        self.child.try_wait().unwrap()
    }

    fn read_available(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self.master.read(buffer) {
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) || error.raw_os_error() == Some(libc::EIO) =>
            {
                Ok(0)
            }
            result => result,
        }
    }

    pub fn pump(&mut self, milliseconds: i32) {
        let mut descriptor = libc::pollfd {
            fd: self.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: the descriptor belongs to this live terminal.
        let ready = unsafe { libc::poll(&mut descriptor, 1, milliseconds) };
        if ready < 0 {
            let error = std::io::Error::last_os_error();
            assert_eq!(error.kind(), std::io::ErrorKind::Interrupted, "{error}");
        }
        if ready <= 0 {
            return;
        }
        let mut chunk = [0; 8192];
        let count = self.read_available(&mut chunk).expect("PTY read");
        self.output.extend_from_slice(&chunk[..count]);
        assert!(self.output.len() < 2 * 1024 * 1024, "PTY output limit");
        let queries = self
            .output
            .windows(4)
            .filter(|bytes| *bytes == b"\x1b[6n")
            .count();
        for _ in self.replies..queries {
            self.master.write_all(b"\x1b[1;1R").unwrap();
        }
        self.replies = queries;
    }

    pub fn wait_for(&mut self, needle: &str) {
        self.observed = self.wait(self.observed, needle);
    }

    pub fn wait(&mut self, start: usize, needle: &str) -> usize {
        assert!(!needle.is_empty(), "PTY wait requires nonempty text");
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(offset) = self.output[start..]
                .windows(needle.len())
                .position(|bytes| bytes == needle.as_bytes())
            {
                return start + offset + needle.len();
            }
            assert!(
                Instant::now() < deadline,
                "waiting for {needle:?}: {:?}",
                String::from_utf8_lossy(&self.output),
            );
            // Do not reap the leader before consuming its pending terminal output.
            self.pump(0);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn stop(&mut self) -> std::io::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        // SAFETY: this unreaped child owns the isolated PTY process group.
        unsafe {
            libc::kill(-(self.child.id() as i32), libc::SIGKILL);
        }
        if let Err(error) = self.child.kill() {
            return if self.child.try_wait()?.is_some() {
                Ok(())
            } else {
                Err(error)
            };
        }
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut bytes = [0; 8192];
        loop {
            // Drain while the killed child closes its terminal descriptors.
            self.read_available(&mut bytes)?;
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    format!("child {} did not exit after SIGKILL", self.child.id()),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}

impl AsRawFd for Terminal {
    fn as_raw_fd(&self) -> RawFd {
        self.master.as_raw_fd()
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("PTY cleanup failed: {error}");
            if !std::thread::panicking() {
                panic!("PTY cleanup failed: {error}");
            }
        }
    }
}
