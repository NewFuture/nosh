//! Real controlling terminal shared by CLI editing and Bash integration tests.

use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
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

    pub fn poll(&mut self) -> Option<ExitStatus> {
        let mut chunk = [0; 8192];
        match self.master.read(&mut chunk) {
            Ok(n) => self.output.extend_from_slice(&chunk[..n]),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::Interrupted
                ) || error.raw_os_error() == Some(libc::EIO) => {}
            Err(error) => panic!("PTY read: {error}"),
        }
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
        std::thread::sleep(Duration::from_millis(5));
        self.child.try_wait().unwrap()
    }

    pub fn wait_for(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut status = None;
        loop {
            if let Some(offset) =
                String::from_utf8_lossy(&self.output[self.observed..]).find(needle)
            {
                self.observed += offset + needle.len();
                return;
            }
            assert!(
                status.is_none() && Instant::now() < deadline,
                "waiting for {needle:?}: {:?}",
                String::from_utf8_lossy(&self.output),
            );
            status = self.poll();
        }
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            // SAFETY: this unreaped child owns the isolated PTY process group.
            unsafe {
                libc::kill(-(self.child.id() as i32), libc::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}
