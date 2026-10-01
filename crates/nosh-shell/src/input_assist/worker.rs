use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Mutex, TryLockError};
use std::time::Instant;

use super::*;

const SOCKET_FD: i32 = 3;
#[cfg(target_os = "linux")]
const HEAP_ALLOWANCE: u64 = 128 * 1024 * 1024;
const TRANSFER_BUDGET: usize = 256 * 1024;
const WORKER_ENV: &str = "NOSH_INPUT_WORKER";

#[derive(Serialize, Deserialize)]
struct Envelope {
    response: Response,
    retire: bool,
}

#[derive(Debug, Clone, Copy)]
pub(super) enum Kind {
    Syntax,
    Lookup,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Lookup => "lookup",
        }
    }
}

/// Called only by the explicit internal entry point (also usable by a test host).
pub fn run_worker_from_env() -> Option<i32> {
    let kind = std::env::var(WORKER_ENV).ok()?;
    let result = worker_main(&kind);
    if let Err(error) = result {
        eprintln!("nosh input worker: {error}");
        Some(1)
    } else {
        Some(0)
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn worker_main(kind: &str) -> io::Result<()> {
    let kind = match kind {
        "syntax" => Kind::Syntax,
        "lookup" => Kind::Lookup,
        _ => return Err(invalid("invalid worker kind")),
    };
    // SAFETY: fcntl only inspects the descriptor; ownership is transferred below
    // only if the descriptor exists and is a stream socket.
    if unsafe { libc::fcntl(SOCKET_FD, libc::F_GETFD) } < 0 {
        return Err(io::Error::last_os_error());
    }
    let mut socket_type: libc::c_int = 0;
    let mut size = std::mem::size_of_val(&socket_type) as libc::socklen_t;
    // SAFETY: the output pointers have the sizes described by `size`.
    if unsafe {
        libc::getsockopt(
            SOCKET_FD,
            libc::SOL_SOCKET,
            libc::SO_TYPE,
            (&raw mut socket_type).cast(),
            &raw mut size,
        )
    } != 0
        || socket_type != libc::SOCK_STREAM
    {
        return Err(invalid("missing worker socket"));
    }
    // SAFETY: this process's explicit worker entry owns the inherited descriptor.
    let mut stream = unsafe { UnixStream::from_raw_fd(SOCKET_FD) };
    stream.set_nonblocking(false)?;
    let result = serve(&mut stream, kind);
    if let Err(error) = &result {
        let report = Envelope {
            response: Response::Failed(short_error(error)),
            retire: true,
        };
        if let Err(report_error) = frame(&report).and_then(|bytes| stream.write_all(&bytes)) {
            return Err(io::Error::other(format!(
                "{error}; cannot report worker failure: {report_error}"
            )));
        }
    }
    result
}

fn serve(stream: &mut UnixStream, kind: Kind) -> io::Result<()> {
    limit_memory()?;
    let mut lookup = lookup::Lookup::default();
    while let Some(bytes) = read_frame(stream)? {
        limit_cpu()?;
        let request: Request = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        validate_request(&request)?;
        let response = match (kind, request) {
            (Kind::Syntax, Request::Analyze(input)) => {
                Response::Analysis(analysis::analyze(&input))
            }
            (Kind::Lookup, Request::Lookup { input, queries }) => {
                let observations = lookup.run(&input, &queries);
                Response::Lookup {
                    version: input.version,
                    observations,
                    stats: lookup.stats(),
                }
            }
            (Kind::Lookup, Request::Index { context, .. }) => {
                Response::Index(lookup::scan_index(&context.cwd, context.path.as_deref()))
            }
            (Kind::Lookup, Request::Correction { input, proposal }) => {
                let accepted = lookup.confirm_correction(&input, &proposal);
                Response::Correction {
                    version: input.version,
                    accepted,
                }
            }
            _ => return Err(invalid("request sent to the wrong worker")),
        };
        let retire = allocation::recycle();
        stream.write_all(&frame(&Envelope { response, retire })?)?;
        if retire {
            return Ok(());
        }
    }
    Ok(())
}

fn validate_request(request: &Request) -> io::Result<()> {
    let (context, input) = match request {
        Request::Analyze(input)
        | Request::Lookup { input, .. }
        | Request::Correction { input, .. } => (&input.context, Some(input.as_ref())),
        Request::Index { context, .. } => (context, None),
    };
    write_json(context, &mut io::sink(), MAX_CONTEXT)?;
    if !context.cwd.is_absolute() {
        return Err(invalid("invalid cwd"));
    }
    if let Some(input) = input
        && input.text.len() > MAX_INPUT
    {
        return Err(invalid("input limit"));
    }
    if let Request::Lookup { input, queries } = request
        && (queries.len() > MAX_QUERIES
            || queries.iter().any(|q| {
                q.word.len() > MAX_CONTEXT
                    || q.word.contains('\0')
                    || input.text.get(q.range.clone()).is_none()
            }))
    {
        return Err(invalid("query limit or invalid query span"));
    }
    if let Request::Correction { input, proposal } = request
        && !proposal.matches(input)
    {
        return Err(invalid("invalid correction proposal"));
    }
    Ok(())
}

fn frame(value: &impl Serialize) -> io::Result<Vec<u8>> {
    let mut framed = vec![0; 4];
    let length = write_json(value, &mut framed, MAX_FRAME)?;
    framed[..4].copy_from_slice(&(length as u32).to_be_bytes());
    Ok(framed)
}

fn read_frame(reader: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header[..1]) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    reader.read_exact(&mut header[1..])?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME {
        return Err(invalid("worker frame limit"));
    }
    let mut buffer = vec![0; length];
    reader.read_exact(&mut buffer)?;
    Ok(Some(buffer))
}

fn set_limit(resource: Resource, maximum: u64) -> io::Result<()> {
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: these calls inspect/change limits only for this worker process.
    if unsafe { libc::getrlimit(resource, &raw mut limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    limit.rlim_cur = limit.rlim_cur.min(maximum as libc::rlim_t);
    if unsafe { libc::setrlimit(resource, &raw const limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(all(target_os = "linux", target_env = "gnu"))]
type Resource = libc::__rlimit_resource_t;
#[cfg(not(all(target_os = "linux", target_env = "gnu")))]
type Resource = libc::c_int;

#[cfg(target_os = "linux")]
fn limit_memory() -> io::Result<()> {
    let baseline = virtual_memory()?;
    set_limit(libc::RLIMIT_AS, baseline.saturating_add(HEAP_ALLOWANCE))?;
    set_limit(libc::RLIMIT_STACK, 4 * 1024 * 1024)?;
    set_limit(libc::RLIMIT_CORE, 0)?;
    // Reserve address space only, without committing/touching physical pages.
    // Darwin aliases RLIMIT_AS to an advisory RSS limit; do not assume success
    // from setrlimit means parser allocations are actually bounded.
    let size = HEAP_ALLOWANCE as usize * 2;
    let probe = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_NONE,
            libc::MAP_PRIVATE | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if probe == libc::MAP_FAILED {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOMEM) {
            return Err(error);
        }
    } else {
        // SAFETY: the address/length are exactly those returned by mmap above.
        if unsafe { libc::munmap(probe, size) } != 0 {
            return Err(io::Error::last_os_error());
        }
        if !allocation::enable() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "address-space limit is not enforced; worker allocator is required",
            ));
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn limit_memory() -> io::Result<()> {
    // Darwin's address-space/RSS limits are advisory enough that the reliable
    // bound here is the worker allocator. Stack/core limits are best-effort:
    // failing them must not disable the whole input assistant.
    let _ = set_limit(libc::RLIMIT_STACK, 4 * 1024 * 1024);
    let _ = set_limit(libc::RLIMIT_CORE, 0);
    if allocation::enable() {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "worker allocator is required on macOS",
        ))
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn limit_memory() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "worker memory limits are unsupported on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn virtual_memory() -> io::Result<u64> {
    let stat = std::fs::read_to_string("/proc/self/statm")?;
    let pages: u64 = stat
        .split_whitespace()
        .next()
        .ok_or_else(|| invalid("missing virtual memory size"))?
        .parse()
        .map_err(io::Error::other)?;
    // SAFETY: sysconf has no pointer arguments.
    let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if page_size <= 0 {
        return Err(invalid("invalid page size"));
    }
    Ok(pages.saturating_mul(page_size as u64))
}

fn limit_cpu() -> io::Result<()> {
    // The CPU limit is cumulative, so renew its soft limit for each request.
    // The parent's independent wall-clock deadline is much shorter.
    let mut usage: libc::rusage = unsafe { std::mem::zeroed() };
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: the structures are valid writable buffers for the current process.
    if unsafe { libc::getrusage(libc::RUSAGE_SELF, &raw mut usage) } != 0
        || unsafe { libc::getrlimit(libc::RLIMIT_CPU, &raw mut limit) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    let micros = (usage.ru_utime.tv_usec as libc::rlim_t)
        .saturating_add(usage.ru_stime.tv_usec as libc::rlim_t);
    let seconds = (usage.ru_utime.tv_sec as libc::rlim_t)
        .saturating_add(usage.ru_stime.tv_sec as libc::rlim_t)
        .saturating_add(micros.div_ceil(1_000_000));
    limit.rlim_cur = seconds.saturating_add(1).min(limit.rlim_max);
    // SAFETY: this changes only this worker's soft CPU limit, preserving its hard limit.
    if unsafe { libc::setrlimit(libc::RLIMIT_CPU, &raw const limit) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

pub(super) struct Worker {
    child: ChildHandle,
    stream: UnixStream,
    outgoing: Vec<u8>,
    written: usize,
    incoming: Vec<u8>,
    pub started: Instant,
    pub timeout: Duration,
    pub busy: bool,
    pub stopping: bool,
    #[cfg(test)]
    hold_reaping: bool,
}

pub(super) type ChildHandle = Arc<Mutex<Child>>;

pub(super) fn kill_child(handle: &ChildHandle) -> io::Result<()> {
    let mut child = handle.try_lock().map_err(|error| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("worker ownership busy: {error}"),
        )
    })?;
    if child.try_wait()?.is_none() {
        child.kill()?;
    }
    Ok(())
}

pub(super) fn reap_child_async(handle: ChildHandle) {
    let _ = std::thread::Builder::new()
        .name("nosh-input-reap".into())
        .spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(2);
            loop {
                let done = match handle.try_lock() {
                    Ok(mut child) => child.try_wait().ok().flatten().is_some(),
                    Err(TryLockError::WouldBlock) => false,
                    Err(_) => true,
                };
                if done || Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
}

impl Worker {
    pub fn spawn(launcher: &WorkerCommand, kind: Kind) -> io::Result<Self> {
        let (parent, child) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let fd = child.as_raw_fd();
        let mut command = Command::new(&launcher.program);
        command
            .args(&launcher.args)
            .env_clear()
            .env(WORKER_ENV, kind.name())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: only async-signal-safe descriptor and signal/process operations
        // run between fork and exec. The socket is retained until spawn returns.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, SOCKET_FD) < 0
                    || libc::fcntl(SOCKET_FD, libc::F_SETFD, 0) < 0
                    || libc::setpgid(0, 0) != 0
                {
                    return Err(io::Error::last_os_error());
                }
                #[cfg(target_os = "linux")]
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let process = command.spawn()?;
        drop(child);
        Ok(Self {
            child: Arc::new(Mutex::new(process)),
            stream: parent,
            outgoing: Vec::new(),
            written: 0,
            incoming: Vec::new(),
            started: Instant::now(),
            timeout: SYNTAX_TIMEOUT,
            busy: false,
            stopping: false,
            #[cfg(test)]
            hold_reaping: false,
        })
    }

    pub fn start(&mut self, request: &Request) -> io::Result<()> {
        if self.busy || self.stopping {
            return Err(invalid("worker already occupied"));
        }
        self.timeout = match request {
            Request::Analyze(_) => SYNTAX_TIMEOUT,
            Request::Lookup { .. } | Request::Correction { .. } => LOOKUP_TIMEOUT,
            Request::Index { .. } => INDEX_TIMEOUT,
        };
        self.outgoing = frame(request)?;
        self.written = 0;
        self.incoming.clear();
        self.started = Instant::now();
        self.busy = true;
        Ok(())
    }

    pub fn poll(&mut self) -> io::Result<Option<Response>> {
        if !self.busy || self.stopping {
            return Ok(None);
        }
        if self.started.elapsed() >= self.timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "diagnostic deadline exceeded",
            ));
        }
        let end = self.outgoing.len().min(self.written + TRANSFER_BUDGET);
        while self.written < end {
            match self.stream.write(&self.outgoing[self.written..end]) {
                Ok(0) => return Err(invalid("worker socket closed during write")),
                Ok(n) => self.written += n,
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        let mut chunk = [0; 8192];
        let mut read = 0;
        while read < TRANSFER_BUDGET {
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(invalid("worker exited or closed its socket")),
                Ok(n) => {
                    read += n;
                    if self.incoming.len() + n > MAX_FRAME + 4 {
                        return Err(invalid("worker frame limit"));
                    }
                    self.incoming.extend_from_slice(&chunk[..n]);
                    if self.incoming.len() >= 4 {
                        let header: [u8; 4] = self.incoming[..4]
                            .try_into()
                            .map_err(|_| invalid("invalid frame header"))?;
                        let length = u32::from_be_bytes(header) as usize;
                        if length > MAX_FRAME {
                            return Err(invalid("worker frame limit"));
                        }
                        if self.incoming.len() >= length + 4 {
                            if self.incoming.len() != length + 4 {
                                return Err(invalid("unsolicited worker output"));
                            }
                            let envelope: Envelope = serde_json::from_slice(&self.incoming[4..])
                                .map_err(io::Error::other)?;
                            self.busy = false;
                            self.outgoing.clear();
                            self.incoming.clear();
                            self.stopping = envelope.retire;
                            return Ok(Some(envelope.response));
                        }
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                Err(e) => return Err(e),
            }
        }
        // A retiring worker may exit just after a WouldBlock read. Drain the
        // socket on the next poll before interpreting EOF as a missing result.
        Ok(None)
    }

    pub fn stop(&mut self) -> io::Result<()> {
        self.stopping = true;
        kill_child(&self.child)
    }

    pub fn reaped(&mut self) -> io::Result<bool> {
        #[cfg(test)]
        if self.hold_reaping {
            return Ok(false);
        }
        self.exit_status().map(|status| status.is_some())
    }

    fn exit_status(&self) -> io::Result<Option<std::process::ExitStatus>> {
        match self.child.try_lock() {
            Ok(mut child) => child.try_wait(),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(error) => Err(io::Error::other(format!(
                "worker ownership poisoned: {error}"
            ))),
        }
    }

    pub fn handle(&self) -> ChildHandle {
        self.child.clone()
    }

    #[cfg(test)]
    pub fn hold_reaping(&mut self, hold: bool) {
        self.hold_reaping = hold;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            // Never wait here: even a killed process may be stuck in kernel I/O.
            eprintln!("nosh input worker cleanup: {error}");
        } else {
            reap_child_async(self.child.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retiring_worker_probe() {
        if std::env::var(WORKER_ENV).is_err() {
            return;
        }
        // SAFETY: this dedicated test entry owns the descriptor installed by Worker::spawn.
        let mut stream = unsafe { UnixStream::from_raw_fd(SOCKET_FD) };
        read_frame(&mut stream).unwrap().unwrap();
        let result = Response::Failed("x".repeat(TRANSFER_BUDGET * 3));
        stream
            .write_all(
                &frame(&Envelope {
                    response: result,
                    retire: true,
                })
                .unwrap(),
            )
            .unwrap();
        std::process::exit(0);
    }

    #[test]
    fn draining_a_retired_workers_buffer_precedes_exit_detection() {
        use crate::input_assist::tests::{Fixture, launcher};
        let fixture = Fixture::new();
        let launch = launcher("input_assist::worker::tests::retiring_worker_probe");
        let mut worker = Worker::spawn(&launch, Kind::Syntax).unwrap();
        worker
            .start(&Request::Analyze(Arc::new(fixture.input("true"))))
            .unwrap();
        worker.timeout = Duration::from_secs(3);
        let deadline = Instant::now() + Duration::from_secs(3);
        let response = loop {
            if let Some(result) = worker.poll().unwrap() {
                break result;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        };
        assert!(matches!(response, Response::Failed(text) if text.len() == TRANSFER_BUDGET * 3));
        assert!(worker.stopping);
        while !worker.reaped().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn interrupted_frame_reads_do_not_recurse() {
        struct Interrupted {
            remaining: usize,
            bytes: io::Cursor<Vec<u8>>,
        }
        impl Read for Interrupted {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                if self.remaining > 0 {
                    self.remaining -= 1;
                    return Err(io::ErrorKind::Interrupted.into());
                }
                self.bytes.read(bytes)
            }
        }
        let mut reader = Interrupted {
            remaining: 100_000,
            bytes: io::Cursor::new(frame(&"ready").unwrap()),
        };
        assert_eq!(read_frame(&mut reader).unwrap().unwrap(), br#""ready""#);
        assert!(read_frame(&mut reader).unwrap().is_none());
        assert!(read_frame(&mut io::Cursor::new([0, 0, 0])).is_err());
    }

    #[test]
    fn frame_and_snapshot_limits_apply_to_encoded_bytes() {
        assert_eq!(bounded_json(&"abc", 5).unwrap(), br#""abc""#);
        assert!(bounded_json(&"abc", 4).is_err());
        assert!(write_json(&"\n", &mut io::sink(), 3).is_err());

        let encoded = frame(&"a".repeat(MAX_FRAME - 2)).unwrap();
        assert_eq!(encoded.len(), MAX_FRAME + 4);
        assert_eq!(&encoded[..4], &(MAX_FRAME as u32).to_be_bytes());
        assert!(frame(&"a".repeat(MAX_FRAME)).is_err());
        let oversized = (MAX_FRAME as u32 + 1).to_be_bytes();
        assert_eq!(
            read_frame(&mut io::Cursor::new(oversized))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
    }
}
