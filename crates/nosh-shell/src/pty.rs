//! Session PTY transport. Command boundaries travel over a private socket,
//! never through strings printed by programs.

use std::fs::{File, OpenOptions};
use std::io::{self, IsTerminal, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::UnixStream;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::process::{Child, Command};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

pub use crate::user_output::CapturedOutput as Snapshot;
use crate::user_output::{OUTPUT_BYTES, OutputCollector};

const CONTROL_ENV: &str = "NOSH_INTERNAL_PTY_FD";
const HELLO: &[u8; 8] = b"noshpty1";
const BLOCK: usize = 16 * 1024;
const LIMIT: usize = OUTPUT_BYTES;
const FRAME: usize = 9;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(10);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_BYTES: usize = 16 * BLOCK;

static CONTROL: OnceLock<Weak<Mutex<UnixStream>>> = OnceLock::new();

/// The shell host's end of the private command-boundary channel.
#[derive(Clone)]
pub struct Control(Arc<Mutex<UnixStream>>);

impl Control {
    fn request(&self, kind: u8, id: u64) -> io::Result<Snapshot> {
        let mut socket = self
            .0
            .lock()
            .map_err(|_| io::Error::other("PTY control lock poisoned"))?;
        let result = (|| {
            if matches!(kind, b'B' | b'E') {
                io::stdout().flush()?;
                io::stderr().flush()?;
                let tty = OpenOptions::new().write(true).open("/dev/tty")?;
                // SAFETY: tty owns an open terminal descriptor.
                cvt(unsafe { libc::tcdrain(tty.as_raw_fd()) })?;
            }
            Self::exchange(&mut socket, kind, id)
        })();
        if result.is_err() {
            // A late response must never be mistaken for the next command's reply.
            let _ = socket.shutdown(std::net::Shutdown::Both);
        }
        result
    }

    fn exchange(socket: &mut UnixStream, kind: u8, id: u64) -> io::Result<Snapshot> {
        let mut request = [0; FRAME];
        request[0] = kind;
        request[1..].copy_from_slice(&id.to_be_bytes());
        socket.write_all(&request)?;
        let mut header = [0; FRAME + 11];
        socket.read_exact(&mut header)?;
        if header[..FRAME] != request {
            return Err(protocol("mismatched PTY response"));
        }
        let observed = u64::from_be_bytes(header[FRAME..FRAME + 8].try_into().unwrap());
        let length = usize::from(u16::from_be_bytes(
            header[FRAME + 8..FRAME + 10].try_into().unwrap(),
        ));
        let flags = header[FRAME + 10];
        if length > LIMIT
            || flags & !7 != 0
            || (kind != b'E' && (length != 0 || observed != 0 || flags != 0))
        {
            return Err(protocol("oversized or unexpected PTY response"));
        }
        let mut bytes = vec![0; length];
        socket.read_exact(&mut bytes)?;
        Ok(Snapshot {
            text: String::from_utf8(bytes)
                .map_err(|_| protocol("invalid UTF-8 in PTY response"))?,
            observed,
            truncated: flags & 1 != 0,
            incomplete: flags & 2 != 0,
            full_screen: flags & 4 != 0,
        })
    }

    pub fn begin(&self, id: u64) -> io::Result<()> {
        self.request(b'B', id).map(|_| ())
    }

    pub fn finish(&self, id: u64) -> io::Result<Snapshot> {
        self.request(b'E', id)
    }
}

/// Clear typeahead at both ends before displaying an approval prompt.
pub fn flush_input() -> io::Result<()> {
    if let Some(control) = CONTROL.get().and_then(Weak::upgrade) {
        Control(control).request(b'F', 0)?;
    }
    Ok(())
}

/// Adopt the endpoint inherited from the relay, before initializing any runtime.
///
/// # Safety
/// This removes an internal environment variable and must run before threads
/// that access the environment have been started.
pub unsafe fn inherited_control() -> io::Result<Option<Control>> {
    let Some(value) = std::env::var_os(CONTROL_ENV) else {
        return Ok(None);
    };
    // SAFETY: required by this function's caller contract.
    unsafe { std::env::remove_var(CONTROL_ENV) };
    let fd: RawFd = value
        .to_str()
        .and_then(|v| v.parse().ok())
        .filter(|fd| *fd >= 3)
        .ok_or_else(|| protocol("invalid PTY control descriptor"))?;
    // Validate before taking ownership: an environment string is not a capability.
    if peer_pid(fd)? != unsafe { libc::getppid() } {
        return Err(protocol(
            "PTY control endpoint does not belong to the parent",
        ));
    }
    // SAFETY: peer_pid verified the inherited socket; ownership is transferred once.
    let mut socket = unsafe { UnixStream::from_raw_fd(fd) };
    cloexec(fd)?;
    socket.set_read_timeout(Some(CONTROL_TIMEOUT))?;
    socket.set_write_timeout(Some(CONTROL_TIMEOUT))?;
    socket.write_all(HELLO)?;
    let mut go = [0; 1];
    socket.read_exact(&mut go)?;
    if go != *b"G" {
        return Err(protocol("PTY host was not released by the relay"));
    }
    let control = Control(Arc::new(Mutex::new(socket)));
    CONTROL
        .set(Arc::downgrade(&control.0))
        .map_err(|_| protocol("PTY host already initialized"))?;
    Ok(Some(control))
}

fn peer_pid(fd: RawFd) -> io::Result<libc::pid_t> {
    #[cfg(target_os = "linux")]
    {
        let mut cred = std::mem::MaybeUninit::<libc::ucred>::zeroed();
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: getsockopt initializes the supplied credential storage.
        cvt(unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                cred.as_mut_ptr().cast(),
                &mut length,
            )
        })?;
        Ok(unsafe { cred.assume_init() }.pid)
    }
    #[cfg(target_os = "macos")]
    {
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of_val(&pid) as libc::socklen_t;
        // SAFETY: getsockopt writes one pid into valid storage.
        cvt(unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&raw mut pid).cast(),
                &mut length,
            )
        })?;
        Ok(pid)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = fd;
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "PTY host platform",
        ))
    }
}

/// The outer process owns the original terminal; the child owns one new PTY.
pub struct SessionPty {
    terminal: Terminal,
    master: File,
    socket: UnixStream,
    child: Child,
    signals: Signals,
    reaped: bool,
}

impl SessionPty {
    /// Failures here occur before the host is allowed to load rc files or run
    /// commands, so the caller may safely continue without capture.
    pub fn spawn(command: &mut Command) -> io::Result<Self> {
        let terminal = Terminal::open()?;
        let (master, slave) = open_pty(&terminal.attrs, &terminal.size()?)?;
        let (socket, child_socket) = UnixStream::pair()?;
        socket.set_read_timeout(Some(CONTROL_TIMEOUT))?;
        socket.set_write_timeout(Some(CONTROL_TIMEOUT))?;
        let signals = Signals::new()?;
        // Keep the inherited endpoint out of the range used for stdio setup.
        // SAFETY: child_socket is open, and the new descriptor is owned below.
        let inherited =
            cvt(unsafe { libc::fcntl(child_socket.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 64) })?;
        let inherited = unsafe { UnixStream::from_raw_fd(inherited) };
        let fd = inherited.as_raw_fd();
        command
            .env(CONTROL_ENV, fd.to_string())
            .stdin(slave.try_clone()?)
            .stdout(slave.try_clone()?)
            .stderr(slave.try_clone()?);
        // SAFETY: only async-signal-safe descriptor/session syscalls run pre-exec.
        unsafe {
            command.pre_exec(move || {
                cvt(libc::setsid())?;
                cvt(libc::ioctl(0, libc::TIOCSCTTY as _, 0))?;
                cvt(libc::fcntl(fd, libc::F_SETFD, 0))?;
                Ok(())
            });
        }
        let child = command.spawn()?;
        drop(inherited);
        drop(child_socket);
        drop(slave);
        let mut session = Self {
            terminal,
            master,
            socket,
            child,
            signals,
            reaped: false,
        };
        let mut hello = [0; HELLO.len()];
        session.socket.read_exact(&mut hello)?;
        if &hello != HELLO {
            return Err(protocol("invalid PTY host handshake"));
        }
        // Validate readiness support before releasing the host to run rc files.
        let mut descriptors = [
            pollfd(
                session.terminal.file.as_raw_fd(),
                libc::POLLIN | libc::POLLOUT,
            ),
            pollfd(session.master.as_raw_fd(), libc::POLLIN | libc::POLLOUT),
            pollfd(session.socket.as_raw_fd(), libc::POLLIN),
            pollfd(session.signals.reader.as_raw_fd(), libc::POLLIN),
        ];
        wait_io(&mut descriptors, Some(Duration::ZERO))?;
        session.terminal.raw()?;
        nonblocking(session.master.as_raw_fd())?;
        session.socket.set_nonblocking(true)?;
        session.socket.write_all(b"G")?;
        Ok(session)
    }

    /// Forward raw bytes with bounded queues; no model or shell is created here.
    pub fn run(mut self) -> io::Result<i32> {
        let mut to_terminal = Pending::default();
        let mut to_child = Pending::default();
        let mut frame = [0; FRAME];
        let mut frame_len = 0;
        let mut boundary = None;
        let mut boundary_started = Instant::now();
        let mut boundary_bytes = 0usize;
        let mut active: Option<(u64, OutputCollector)> = None;
        let mut last_id = 0;
        let mut control_open = true;
        let mut input_open = true;
        let mut master_open = true;
        let mut exited = None;
        loop {
            if boundary.is_some()
                && (boundary_started.elapsed() >= DRAIN_TIMEOUT || boundary_bytes >= DRAIN_BYTES)
            {
                eprintln!("nosh: user output capture unavailable: no reliable terminal boundary");
                self.socket.shutdown(std::net::Shutdown::Both)?;
                control_open = false;
                boundary = None;
                active = None;
            }
            if exited.is_none()
                && let Some(status) = self.child.try_wait()?
            {
                self.reaped = true;
                exited = Some(
                    status
                        .code()
                        .unwrap_or_else(|| 128 + status.signal().unwrap_or(1)),
                );
            }
            to_terminal.write(&mut self.terminal.file)?;
            to_child.write(&mut self.master)?;
            let mut drained = !master_open;
            if master_open && to_terminal.empty() {
                let mut block = [0; BLOCK];
                match self.master.read(&mut block) {
                    Ok(0) => {
                        drained = true;
                        master_open = false;
                    }
                    Ok(n) => {
                        if boundary.is_some() {
                            boundary_bytes += n;
                        }
                        if let Some((_, tail)) = &mut active {
                            tail.push(&block[..n]);
                        }
                        to_terminal.set(&block[..n]);
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => drained = true,
                    Err(e) if e.raw_os_error() == Some(libc::EIO) => {
                        drained = true;
                        master_open = false;
                    }
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(e) => return Err(e),
                }
            }
            if !master_open && input_open {
                input_open = false;
                to_child.clear();
                self.terminal.restore()?;
            }
            let can_reply = (drained && to_terminal.empty()) || matches!(boundary, Some((b'F', 0)));
            if can_reply && let Some((kind, id)) = boundary.take() {
                let snapshot = match kind {
                    b'B' if active.is_none() && id > last_id => {
                        last_id = id;
                        active = Some((id, OutputCollector::default()));
                        Snapshot::default()
                    }
                    b'E' if active.as_ref().is_some_and(|(current, _)| *current == id) => {
                        active.take().unwrap().1.finish()
                    }
                    b'F' if id == 0 => {
                        // The host will flush its slave after receiving this reply.
                        to_child.clear();
                        cvt(unsafe {
                            libc::tcflush(self.terminal.file.as_raw_fd(), libc::TCIFLUSH)
                        })?;
                        Snapshot::default()
                    }
                    _ => return Err(protocol("invalid PTY command transition")),
                };
                if let Err(error) = self.reply(kind, id, &snapshot) {
                    eprintln!("nosh: user output capture disconnected: {error}");
                    let _ = self.socket.shutdown(std::net::Shutdown::Both);
                    control_open = false;
                    active = None;
                }
            }
            if drained
                && to_terminal.empty()
                && let Some(code) = exited
            {
                return Ok(code);
            }
            let mut descriptors = [
                pollfd(
                    self.terminal.file.as_raw_fd(),
                    if input_open && to_child.empty() && boundary.is_none() {
                        libc::POLLIN
                    } else {
                        0
                    } | if to_terminal.empty() {
                        0
                    } else {
                        libc::POLLOUT
                    },
                ),
                pollfd(
                    if master_open {
                        self.master.as_raw_fd()
                    } else {
                        -1
                    },
                    if to_terminal.empty() { libc::POLLIN } else { 0 }
                        | if to_child.empty() { 0 } else { libc::POLLOUT },
                ),
                pollfd(
                    if control_open && boundary.is_none() {
                        self.socket.as_raw_fd()
                    } else {
                        -1
                    },
                    if control_open && boundary.is_none() {
                        libc::POLLIN
                    } else {
                        0
                    },
                ),
                pollfd(self.signals.reader.as_raw_fd(), libc::POLLIN),
            ];
            let timeout = if boundary.is_some() {
                Some(DRAIN_TIMEOUT.saturating_sub(boundary_started.elapsed()))
            } else {
                None
            };
            if let Err(error) = wait_io(&mut descriptors, timeout) {
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if descriptors[3].revents != 0 {
                for signal in self.signals.pending()? {
                    if signal == libc::SIGWINCH {
                        let size = self.terminal.size()?;
                        cvt(unsafe {
                            libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &size)
                        })?;
                    } else if signal != libc::SIGCHLD {
                        self.signal_foreground(signal)?;
                    }
                }
            }
            if descriptors[2].revents != 0 && control_open && boundary.is_none() {
                match self.socket.read(&mut frame[frame_len..]) {
                    Ok(0) => {
                        // `exec` closes the control endpoint but can keep using the PTY.
                        control_open = false;
                        active = None;
                    }
                    Ok(n) => {
                        frame_len += n;
                        if frame_len == FRAME {
                            boundary = Some((
                                frame[0],
                                u64::from_be_bytes(frame[1..].try_into().unwrap()),
                            ));
                            boundary_started = Instant::now();
                            boundary_bytes = 0;
                            frame_len = 0;
                        }
                    }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
            if descriptors[0].revents & libc::POLLIN != 0 && to_child.empty() {
                let mut block = [0; BLOCK];
                match self.terminal.file.read(&mut block) {
                    Ok(0) => {
                        input_open = false;
                        self.signal_foreground(libc::SIGHUP)?;
                    }
                    Ok(n) => to_child.set(&block[..n]),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                    Err(e) => return Err(e),
                }
            }
        }
    }

    fn reply(&mut self, kind: u8, id: u64, snapshot: &Snapshot) -> io::Result<()> {
        let mut response = Vec::with_capacity(FRAME + 11 + LIMIT);
        response.push(kind);
        response.extend(id.to_be_bytes());
        response.extend(snapshot.observed.to_be_bytes());
        response.extend((snapshot.text.len() as u16).to_be_bytes());
        response.push(
            u8::from(snapshot.truncated)
                | (u8::from(snapshot.incomplete) << 1)
                | (u8::from(snapshot.full_screen) << 2),
        );
        response.extend(snapshot.text.as_bytes());
        self.socket.set_nonblocking(false)?;
        let result = self.socket.write_all(&response);
        self.socket.set_nonblocking(true)?;
        result
    }

    fn signal_foreground(&self, signal: i32) -> io::Result<()> {
        // SAFETY: the process group comes from this session's own PTY.
        let foreground = unsafe { libc::tcgetpgrp(self.master.as_raw_fd()) };
        let group = if foreground > 0 {
            foreground
        } else if !self.reaped {
            self.child.id() as libc::pid_t
        } else {
            return Ok(());
        };
        if group > 0 {
            let result = unsafe { libc::kill(-group, signal) };
            if result < 0 && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH) {
                return Err(io::Error::last_os_error());
            }
        }
        Ok(())
    }
}

impl Drop for SessionPty {
    fn drop(&mut self) {
        if !self.reaped {
            let _ = self.signal_foreground(libc::SIGHUP);
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

#[derive(Default)]
struct Pending {
    bytes: Vec<u8>,
    offset: usize,
}

impl Pending {
    fn empty(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn clear(&mut self) {
        self.bytes.clear();
        self.offset = 0;
    }

    fn set(&mut self, bytes: &[u8]) {
        debug_assert!(self.empty() && bytes.len() <= BLOCK);
        self.clear();
        self.bytes.extend_from_slice(bytes);
    }

    fn write(&mut self, file: &mut File) -> io::Result<()> {
        if !self.empty() {
            match file.write(&self.bytes[self.offset..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(n) => self.offset += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
}

struct Terminal {
    file: File,
    attrs: libc::termios,
    changed: bool,
}

impl Terminal {
    fn open() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() || !io::stderr().is_terminal()
        {
            return Err(io::Error::other(
                "capture requires stdin, stdout and stderr on the terminal",
            ));
        }
        let mut devices = Vec::new();
        for fd in 0..=2 {
            let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
            cvt(unsafe { libc::fstat(fd, stat.as_mut_ptr()) })?;
            devices.push(unsafe { stat.assume_init() }.st_rdev);
        }
        if devices[0] != devices[1]
            || devices[0] != devices[2]
            || unsafe { libc::tcgetpgrp(0) } != unsafe { libc::getpgrp() }
        {
            return Err(io::Error::other(
                "capture requires one foreground controlling terminal",
            ));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NONBLOCK | libc::O_CLOEXEC)
            .open("/dev/tty")?;
        let mut attrs = std::mem::MaybeUninit::<libc::termios>::zeroed();
        cvt(unsafe { libc::tcgetattr(file.as_raw_fd(), attrs.as_mut_ptr()) })?;
        Ok(Self {
            file,
            attrs: unsafe { attrs.assume_init() },
            changed: false,
        })
    }

    fn raw(&mut self) -> io::Result<()> {
        let mut attrs = self.attrs;
        unsafe { libc::cfmakeraw(&mut attrs) };
        cvt(unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &attrs) })?;
        self.changed = true;
        Ok(())
    }

    fn size(&self) -> io::Result<libc::winsize> {
        let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
        cvt(unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) })?;
        Ok(unsafe { size.assume_init() })
    }

    fn restore(&mut self) -> io::Result<()> {
        if self.changed {
            cvt(unsafe { libc::tcsetattr(self.file.as_raw_fd(), libc::TCSANOW, &self.attrs) })?;
            self.changed = false;
        }
        Ok(())
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            eprintln!("nosh: restoring relay terminal: {error}");
        }
    }
}

struct Signals {
    reader: UnixStream,
    flags: Vec<(i32, Arc<AtomicBool>)>,
    registrations: Vec<signal_hook::SigId>,
}

impl Signals {
    fn new() -> io::Result<Self> {
        let (reader, writer) = UnixStream::pair()?;
        reader.set_nonblocking(true)?;
        let mut signals = Self {
            reader,
            flags: Vec::new(),
            registrations: Vec::new(),
        };
        for signal in [
            libc::SIGCHLD,
            libc::SIGWINCH,
            libc::SIGINT,
            libc::SIGQUIT,
            libc::SIGHUP,
            libc::SIGTERM,
            libc::SIGCONT,
        ] {
            let flag = Arc::new(AtomicBool::new(false));
            signals
                .registrations
                .push(signal_hook::flag::register(signal, flag.clone())?);
            signals
                .registrations
                .push(signal_hook::low_level::pipe::register(
                    signal,
                    writer.try_clone()?,
                )?);
            signals.flags.push((signal, flag));
        }
        Ok(signals)
    }

    fn pending(&mut self) -> io::Result<Vec<i32>> {
        let mut block = [0; 256];
        loop {
            match self.reader.read(&mut block) {
                Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(self
            .flags
            .iter()
            .filter_map(|(signal, flag)| flag.swap(false, Ordering::SeqCst).then_some(*signal))
            .collect())
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        for id in self.registrations.drain(..) {
            signal_hook::low_level::unregister(id);
        }
    }
}

fn open_pty(attrs: &libc::termios, size: &libc::winsize) -> io::Result<(File, File)> {
    let (mut master, mut slave) = (-1, -1);
    let mut attrs = *attrs;
    let mut size = *size;
    // SAFETY: all output pointers are valid; the optional name is unused.
    cvt(unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            &raw mut attrs,
            &raw mut size,
        )
    })?;
    // SAFETY: openpty returned two distinct, owned descriptors.
    let (master, slave) = unsafe { (File::from_raw_fd(master), File::from_raw_fd(slave)) };
    cloexec(master.as_raw_fd())?;
    cloexec(slave.as_raw_fd())?;
    Ok((master, slave))
}

fn cloexec(fd: RawFd) -> io::Result<()> {
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) }).map(|_| ())
}

fn nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = cvt(unsafe { libc::fcntl(fd, libc::F_GETFL) })?;
    cvt(unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) }).map(|_| ())
}

fn pollfd(fd: RawFd, events: i16) -> libc::pollfd {
    libc::pollfd {
        fd: if events == 0 { -1 } else { fd },
        events,
        revents: 0,
    }
}

fn wait_io(descriptors: &mut [libc::pollfd; 4], timeout: Option<Duration>) -> io::Result<()> {
    // Like crossterm's use-dev-tty backend, use select on macOS: Darwin poll
    // reports POLLNVAL for /dev/tty. Its select adapter rejects negative fds.
    let mut active = std::array::from_fn::<_, 4, _>(|_| pollfd(-1, 0));
    let mut indices = [0; 4];
    let mut count = 0;
    for (index, descriptor) in descriptors.iter_mut().enumerate() {
        descriptor.revents = 0;
        if descriptor.fd >= 0 && descriptor.events != 0 {
            active[count] = *descriptor;
            indices[count] = index;
            count += 1;
        }
    }
    filedescriptor::poll(&mut active[..count], timeout).map_err(|error| match error {
        filedescriptor::Error::Io(error) | filedescriptor::Error::Poll(error) => error,
        error => io::Error::other(error),
    })?;
    for index in 0..count {
        if active[index].revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid relay descriptor",
            ));
        }
        descriptors[indices[index]].revents = active[index].revents;
    }
    Ok(())
}

fn protocol(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn cvt(result: i32) -> io::Result<i32> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_descriptors_cannot_spin_on_hangup() {
        assert_eq!(pollfd(4, 0).fd, -1);
        assert_eq!(pollfd(4, libc::POLLIN).fd, 4);
    }

    #[test]
    fn readiness_handles_sockets_and_disabled_entries() {
        let (reader, mut writer) = UnixStream::pair().unwrap();
        writer.write_all(b"x").unwrap();
        let mut descriptors = [
            pollfd(-1, 0),
            pollfd(reader.as_raw_fd(), libc::POLLIN),
            pollfd(-1, 0),
            pollfd(-1, 0),
        ];
        wait_io(&mut descriptors, Some(Duration::from_secs(1))).unwrap();
        assert_ne!(descriptors[1].revents & libc::POLLIN, 0);
        assert_eq!(descriptors[0].revents, 0);
    }

    #[test]
    fn replies_are_bounded_and_must_match_the_requested_command() {
        for (id, length, flags) in [(7u64, LIMIT + 1, 0u8), (8, 0, 0), (7, 0, 128)] {
            let (mut client, mut server) = UnixStream::pair().unwrap();
            let writer = std::thread::spawn(move || {
                let mut request = [0; FRAME];
                server.read_exact(&mut request).unwrap();
                let mut reply = vec![b'E'];
                reply.extend(id.to_be_bytes());
                reply.extend(0u64.to_be_bytes());
                reply.extend((length as u16).to_be_bytes());
                reply.push(flags);
                server.write_all(&reply).unwrap();
            });
            assert_eq!(
                Control::exchange(&mut client, b'E', 7).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
            writer.join().unwrap();
        }
    }
}
