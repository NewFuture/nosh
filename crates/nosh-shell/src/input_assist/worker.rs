use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::sync::{Condvar, Mutex, OnceLock, TryLockError};
use std::time::Instant;

use super::*;

const SOCKET_FD: i32 = 3;
#[cfg(target_os = "linux")]
const HEAP_ALLOWANCE: u64 = 128 * 1024 * 1024;
const TRANSFER_BUDGET: usize = 256 * 1024;
const WORKER_ENV: &str = "NOSH_INPUT_WORKER";
const MAX_DETACHED_HELPERS: usize = 256;

#[derive(Serialize, Deserialize)]
struct Envelope {
    response: Response,
    retire: bool,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum Kind {
    Syntax,
    Lookup,
    Completion,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Syntax => "syntax",
            Self::Lookup => "lookup",
            Self::Completion => "completion",
        }
    }

    fn completion(self) -> bool {
        matches!(self, Self::Completion)
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
        "completion" => Kind::Completion,
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
    // Do not expose the IPC channel to commands run by a completion provider.
    if unsafe { libc::fcntl(SOCKET_FD, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
        return Err(io::Error::last_os_error());
    }
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
    let mut completion = crate::completion::worker::Server::default();
    while let Some(bytes) = read_frame(stream)? {
        limit_cpu()?;
        let request: Request = serde_json::from_slice(&bytes).map_err(io::Error::other)?;
        validate_request(&request)?;
        let response = match (kind, request) {
            (
                Kind::Completion,
                Request::Complete {
                    query,
                    context,
                    install,
                },
            ) => {
                let outcome = completion.run(query, &context, install, &mut |outcome| {
                    stream.write_all(&frame(&Envelope {
                        response: Response::Completion(outcome),
                        retire: false,
                    })?)
                })?;
                Response::Completion(outcome)
            }
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
    if let Request::Complete {
        query,
        context,
        install,
    } = request
    {
        if query.text.len() > crate::completion::types::MAX_INPUT
            || !query.text.is_char_boundary(query.cursor)
        {
            return Err(invalid("invalid completion input or cursor"));
        }
        if !context.valid_for(query) {
            return Err(invalid("invalid completion context"));
        }
        write_json(
            install,
            &mut io::sink(),
            crate::completion::types::MAX_FRAME - crate::completion::types::MAX_INPUT,
        )?;
        if let Some(install) = install {
            install.validate()?;
        }
        return Ok(());
    }
    let (context, input) = match request {
        Request::Analyze(input)
        | Request::Lookup { input, .. }
        | Request::Correction { input, .. } => (&input.context, Some(input.as_ref())),
        Request::Index { context, .. } => (context, None),
        Request::Complete { .. } => return Err(invalid("unexpected completion validation path")),
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

fn frame_length(header: &[u8]) -> io::Result<usize> {
    let header = header
        .try_into()
        .map_err(|_| invalid("invalid frame header"))?;
    let length = u32::from_be_bytes(header) as usize;
    if length > MAX_FRAME {
        return Err(invalid("worker frame limit"));
    }
    Ok(length)
}

fn read_frame(reader: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut header = [0; 4];
    match reader.read_exact(&mut header[..1]) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    reader.read_exact(&mut header[1..])?;
    let length = frame_length(&header)?;
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

pub(crate) struct Worker {
    child: ChildHandle,
    stream: UnixStream,
    outgoing: Vec<u8>,
    written: usize,
    incoming: Vec<u8>,
    pub started: Instant,
    pub timeout: Duration,
    pub busy: bool,
    pub stopping: bool,
    kind: Kind,
    #[cfg(test)]
    hold_reaping: bool,
}

pub(crate) struct OwnedChild {
    child: Child,
    reaped: bool,
    marker: Option<String>,
    helpers: Vec<crate::procs::TaggedProcess>,
    uncontrolled: Option<String>,
    reported_cleanup_error: Option<String>,
}

impl OwnedChild {
    #[cfg(test)]
    pub(crate) fn id(&self) -> u32 {
        self.child.id()
    }

    #[cfg(test)]
    pub(crate) fn marker(&self) -> Option<&str> {
        self.marker.as_deref()
    }

    fn kill(&mut self) -> io::Result<()> {
        if self.reaped {
            return Ok(());
        }
        if self.marker.is_some() {
            // No consuming wait occurs until every owned descendant is gone.
            if unsafe { libc::killpg(self.child.id() as i32, libc::SIGKILL) } != 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {
                    return Err(error);
                }
            }
        } else if self.child.try_wait()?.is_some() {
            self.reaped = true;
        } else {
            self.child.kill()?;
        }
        Ok(())
    }

    fn residual_children(&self) -> io::Result<Vec<crate::procs::Proc>> {
        let Some(marker) = &self.marker else {
            return Ok(Vec::new());
        };
        if self.reaped {
            return Ok(Vec::new());
        }
        let mut children = crate::procs::owned_procs(self.child.id() as i32, marker)?;
        for helper in &self.helpers {
            if !helper.gone()? && !children.iter().any(|p| p.pid == helper.process.pid) {
                children.push(helper.process);
            }
        }
        Ok(children)
    }

    fn stop_descendants(&mut self) -> io::Result<()> {
        let Some(marker) = self.marker.clone() else {
            return Ok(());
        };
        for index in (0..self.helpers.len()).rev() {
            if self.helpers[index].gone()? {
                self.helpers.swap_remove(index);
            }
        }
        let controlled_group = self.child.id() as i32;
        let mut error = None;
        for process in self.residual_children()? {
            if process.pgid == controlled_group
                || self
                    .helpers
                    .iter()
                    .any(|helper| helper.process.pid == process.pid)
            {
                continue;
            }
            let opened = if self.helpers.len() >= MAX_DETACHED_HELPERS {
                Err(io::Error::other(
                    "completion detached-helper limit exceeded",
                ))
            } else {
                crate::procs::TaggedProcess::open_in_session(process, &marker, controlled_group)
            };
            match opened {
                Ok(Some(helper)) => self.helpers.push(helper),
                Ok(None) => {}
                Err(failure) => {
                    // Once ownership cannot be retained, later disappearance from
                    // the tree is not proof that an escaped helper was reaped.
                    self.uncontrolled
                        .get_or_insert_with(|| short_error(&failure));
                    error = Some(failure);
                }
            }
        }
        for helper in &self.helpers {
            if let Err(failure) = helper.signal(libc::SIGKILL) {
                error = Some(failure);
            }
        }
        if let Some(message) = &self.uncontrolled {
            return Err(io::Error::other(message.clone()));
        }
        error.map_or(Ok(()), Err)
    }

    fn exited_without_reaping(&self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        // SAFETY: this is our retained child. WNOWAIT preserves its identity and
        // the dedicated group id until descendant cleanup has been verified.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                self.child.id() as libc::id_t,
                &raw mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        #[cfg(target_os = "linux")]
        let pid = unsafe { status.si_pid() };
        #[cfg(not(target_os = "linux"))]
        let pid = status.si_pid;
        Ok(pid != 0)
    }

    fn stop(&mut self) -> io::Result<()> {
        let descendants = self.stop_descendants();
        let killed = self.kill();
        descendants?;
        // Darwin can return EPERM when only the zombie group leader remains.
        // Keep its identity until exit and the absence of descendants are verified.
        if cfg!(target_os = "macos")
            && self.marker.is_some()
            && killed
                .as_ref()
                .is_err_and(|error| error.raw_os_error() == Some(libc::EPERM))
            && self.exited_without_reaping()?
            && self.residual_children()?.is_empty()
        {
            return Ok(());
        }
        killed
    }

    fn retire(&mut self) -> io::Result<bool> {
        if self.reaped {
            return Ok(true);
        }
        self.stop()?;
        if self.marker.is_some()
            && (!self.exited_without_reaping()? || !self.residual_children()?.is_empty())
        {
            return Ok(false);
        }
        self.reaped = self.child.try_wait()?.is_some();
        Ok(self.reaped)
    }
}

pub(crate) type ChildHandle = Arc<Mutex<OwnedChild>>;

pub(crate) fn kill_child(handle: &ChildHandle) -> io::Result<()> {
    let mut child = handle.try_lock().map_err(|error| {
        io::Error::new(
            io::ErrorKind::WouldBlock,
            format!("worker ownership busy: {error}"),
        )
    })?;
    child.kill()
}

pub(super) fn reap_child_async(handle: ChildHandle) {
    struct Reaper {
        children: Mutex<Vec<ChildHandle>>,
        wake: Condvar,
    }
    static REAPER: OnceLock<Arc<Reaper>> = OnceLock::new();
    let reaper = REAPER.get_or_init(|| {
        let reaper = Arc::new(Reaper {
            children: Mutex::new(Vec::new()),
            wake: Condvar::new(),
        });
        let background = reaper.clone();
        if let Err(error) = std::thread::Builder::new()
            .name("nosh-input-reap".into())
            .spawn(move || {
                loop {
                    let mut children = {
                        let mut children = background
                            .children
                            .lock()
                            .unwrap_or_else(|error| error.into_inner());
                        while children.is_empty() {
                            children = background
                                .wake
                                .wait(children)
                                .unwrap_or_else(|error| error.into_inner());
                        }
                        std::mem::take(&mut *children)
                    };
                    children.retain(|handle| match handle.try_lock() {
                        Ok(mut child) => match child.retire() {
                            Ok(done) => !done,
                            Err(error) => {
                                let message = short_error(error);
                                if child.reported_cleanup_error.as_ref() != Some(&message) {
                                    eprintln!("nosh worker reaping: {message}");
                                    child.reported_cleanup_error = Some(message);
                                }
                                true
                            }
                        },
                        Err(_) => true,
                    });
                    let mut pending = background
                        .children
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    pending.extend(children);
                    if !pending.is_empty() {
                        drop(
                            background
                                .wake
                                .wait_timeout(pending, Duration::from_millis(100)),
                        );
                    }
                }
            })
        {
            // The registry still owns every handle if the sole reaper cannot start.
            eprintln!("nosh worker reaper startup: {error}");
        }
        reaper
    });
    let mut children = reaper
        .children
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    if !children.iter().any(|child| Arc::ptr_eq(child, &handle)) {
        children.push(handle);
        reaper.wake.notify_one();
    }
}

impl Worker {
    pub fn spawn(launcher: &WorkerCommand, kind: Kind) -> io::Result<Self> {
        let (parent, child) = UnixStream::pair()?;
        parent.set_nonblocking(true)?;
        let fd = child.as_raw_fd();
        let mut command = Command::new(&launcher.program);
        static RUNS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let marker = if kind.completion() {
            let mut nonce = [0u8; 16];
            // SAFETY: getentropy fills this writable buffer before any fork.
            if unsafe { libc::getentropy(nonce.as_mut_ptr().cast(), nonce.len()) } != 0 {
                return Err(io::Error::last_os_error());
            }
            Some(format!(
                "completion.{}.{}.{:032x}",
                std::process::id(),
                RUNS.fetch_add(1, std::sync::atomic::Ordering::Relaxed),
                u128::from_ne_bytes(nonce),
            ))
        } else {
            None
        };
        command
            .args(&launcher.args)
            .env_clear()
            .env(WORKER_ENV, kind.name())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        if let Some(marker) = &marker {
            command.env(crate::procs::RUN_VAR, marker);
        }
        // SAFETY: only async-signal-safe descriptor and signal/process operations
        // run between fork and exec. The socket is retained until spawn returns.
        unsafe {
            command.pre_exec(move || {
                if libc::dup2(fd, SOCKET_FD) < 0
                    || libc::fcntl(SOCKET_FD, libc::F_SETFD, 0) < 0
                    || (if kind.completion() {
                        libc::setsid() < 0
                    } else {
                        libc::setpgid(0, 0) != 0
                    })
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
            child: Arc::new(Mutex::new(OwnedChild {
                child: process,
                reaped: false,
                marker,
                helpers: Vec::new(),
                uncontrolled: None,
                reported_cleanup_error: None,
            })),
            stream: parent,
            outgoing: Vec::new(),
            written: 0,
            incoming: Vec::new(),
            started: Instant::now(),
            timeout: SYNTAX_TIMEOUT,
            busy: false,
            stopping: false,
            kind,
            #[cfg(test)]
            hold_reaping: false,
        })
    }

    pub fn start(&mut self, request: &Request) -> io::Result<()> {
        if self.busy || self.stopping {
            return Err(invalid("worker already occupied"));
        }
        let mut byte = 0u8;
        // SAFETY: peek cannot consume a previous response or block this caller.
        let pending = unsafe {
            libc::recv(
                self.stream.as_raw_fd(),
                (&raw mut byte).cast(),
                1,
                libc::MSG_PEEK | libc::MSG_DONTWAIT,
            )
        };
        if pending >= 0 {
            return Err(invalid(if pending == 0 {
                "worker exited before request"
            } else {
                "unsolicited worker output"
            }));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock {
            return Err(error);
        }
        self.timeout = match request {
            Request::Complete { .. } => LOOKUP_TIMEOUT,
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
        if let Some(response) = self.take_response()? {
            return Ok(Some(response));
        }
        if self.started.elapsed() >= self.timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                if self.kind.completion() {
                    "completion deadline exceeded"
                } else {
                    "diagnostic deadline exceeded"
                },
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
            let remaining = if self.incoming.len() < 4 {
                4 - self.incoming.len()
            } else {
                frame_length(&self.incoming[..4])? + 4 - self.incoming.len()
            };
            let size = remaining.min(chunk.len()).min(TRANSFER_BUDGET - read);
            match self.stream.read(&mut chunk[..size]) {
                Ok(0) => return Err(invalid("worker exited or closed its socket")),
                Ok(n) => {
                    read += n;
                    if self.incoming.len() + n > MAX_FRAME + 4 {
                        return Err(invalid("worker frame limit"));
                    }
                    self.incoming.extend_from_slice(&chunk[..n]);
                    if let Some(response) = self.take_response()? {
                        return Ok(Some(response));
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
        self.child
            .try_lock()
            .map_err(|error| io::Error::other(error.to_string()))?
            .stop()
    }

    pub fn reaped(&mut self) -> io::Result<bool> {
        #[cfg(test)]
        if self.hold_reaping {
            return Ok(false);
        }
        match self.child.try_lock() {
            Ok(mut child) => child.retire(),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(error) => Err(io::Error::other(format!(
                "worker ownership poisoned: {error}"
            ))),
        }
    }

    pub fn handle(&self) -> ChildHandle {
        self.child.clone()
    }

    pub fn residual_children(&self) -> io::Result<Vec<crate::procs::Proc>> {
        self.child
            .try_lock()
            .map_err(|error| io::Error::other(error.to_string()))?
            .residual_children()
    }

    fn take_response(&mut self) -> io::Result<Option<Response>> {
        let Some(header) = self.incoming.get(..4) else {
            return Ok(None);
        };
        let length = frame_length(header)?;
        if self.incoming.len() < length + 4 {
            return Ok(None);
        }
        let envelope: Envelope =
            serde_json::from_slice(&self.incoming[4..length + 4]).map_err(io::Error::other)?;
        self.incoming.drain(..length + 4);
        let progress = if let Response::Completion(crate::completion::types::Outcome::Progress(_)) =
            &envelope.response
        {
            if envelope.retire {
                return Err(invalid("completion progress cannot retire its worker"));
            }
            self.timeout = self.timeout.max(INDEX_TIMEOUT);
            true
        } else {
            false
        };
        if !progress {
            if !self.incoming.is_empty() {
                return Err(invalid("unsolicited worker output"));
            }
            self.busy = false;
        }
        self.outgoing.clear();
        self.written = 0;
        self.stopping = envelope.retire;
        Ok(Some(envelope.response))
    }

    #[cfg(test)]
    pub fn hold_reaping(&mut self, hold: bool) {
        self.hold_reaping = hold;
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if self.child.try_lock().is_ok_and(|child| child.reaped) {
            return;
        }
        if let Err(error) = kill_child(&self.child) {
            // Never wait here: even a killed process may be stuck in kernel I/O.
            eprintln!("nosh input worker cleanup: {error}");
        }
        reap_child_async(self.child.clone());
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
        for kind in [Kind::Syntax, Kind::Completion] {
            let mut worker = Worker::spawn(&launch, kind).unwrap();
            let request = if kind.completion() {
                completion_request()
            } else {
                Request::Analyze(Arc::new(fixture.input("true")))
            };
            worker.start(&request).unwrap();
            worker.timeout = Duration::from_secs(3);
            let deadline = Instant::now() + Duration::from_secs(3);
            let response = loop {
                if let Some(result) = worker.poll().unwrap() {
                    break result;
                }
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            };
            assert!(
                matches!(response, Response::Failed(text) if text.len() == TRANSFER_BUDGET * 3)
            );
            assert!(worker.stopping);
            while !worker
                .child
                .lock()
                .unwrap()
                .exited_without_reaping()
                .unwrap()
            {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            while !worker.reaped().unwrap() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
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

    fn completion_query() -> crate::completion::types::Query {
        crate::completion::types::Query {
            text: "sample ".into(),
            cursor: 7,
            session: 1,
            epoch: 1,
            trigger: crate::completion::types::Trigger::Explicit,
        }
    }

    fn completion_request() -> Request {
        Request::Complete {
            query: completion_query(),
            context: Box::new(crate::completion::context::Context {
                words: ["sample".into(), "".into()].into(),
                index: 1,
                word: String::new(),
                span: 7..7,
                command_start: 0,
                command_end: 7,
                quote: None,
                redirect: false,
                path: None,
                requires_execution: false,
            }),
            install: None,
        }
    }

    #[test]
    fn completion_context_bounds_are_validated() {
        let mut request = completion_request();
        assert!(validate_request(&request).is_ok());
        let Request::Complete { context, .. } = &mut request else {
            unreachable!()
        };
        context.span = 8..9;
        assert!(validate_request(&request).is_err());
    }

    fn completion_answer(
        state: crate::completion::types::State,
    ) -> crate::completion::types::Answer {
        crate::completion::types::Answer {
            query: completion_query(),
            candidates: Vec::new(),
            state,
        }
    }

    fn stop_completion(worker: &mut Worker) {
        worker.stop().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !worker.reaped().unwrap() {
            assert!(Instant::now() < deadline, "completion cleanup watchdog");
            std::thread::sleep(Duration::from_millis(4));
        }
    }

    #[test]
    fn completion_frames_probe() {
        use crate::completion::types::{Outcome, State};
        if std::env::var(WORKER_ENV).is_err() {
            return;
        }
        // SAFETY: Worker::spawn installed this descriptor for this test entry.
        let mut stream = unsafe { UnixStream::from_raw_fd(SOCKET_FD) };
        let capacity: libc::c_int = 1024;
        // SAFETY: the socket and the integer option value are valid for this call.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&raw const capacity).cast(),
                    std::mem::size_of_val(&capacity) as libc::socklen_t,
                )
            },
            0,
            "{}",
            io::Error::last_os_error()
        );
        read_frame(&mut stream).unwrap().unwrap();
        stream
            .write_all(
                &frame(&Envelope {
                    response: Response::Completion(Outcome::Progress(completion_answer(
                        State::Partial("indexing".into()),
                    ))),
                    retire: false,
                })
                .unwrap(),
            )
            .unwrap();
        let mut bytes = Vec::new();
        for message in ["x".repeat(MAX_FRAME - 512), "y".repeat(2048)] {
            bytes.extend(
                frame(&Envelope {
                    response: Response::Completion(Outcome::Progress(completion_answer(
                        State::Partial(message),
                    ))),
                    retire: false,
                })
                .unwrap(),
            );
        }
        bytes.extend(
            frame(&Envelope {
                response: Response::Completion(Outcome::Ready {
                    answer: completion_answer(State::Partial("terminal limit".into())),
                    snapshot: None,
                }),
                retire: true,
            })
            .unwrap(),
        );
        stream.write_all(&bytes).unwrap();
        std::process::exit(0);
    }

    #[test]
    fn completion_progress_and_final_frames_can_be_coalesced_without_shrinking_the_budget() {
        use crate::completion::types::{Outcome, State};
        let launch = crate::input_assist::tests::launcher(
            "input_assist::worker::tests::completion_frames_probe",
        );
        let mut worker = Worker::spawn(&launch, Kind::Completion).unwrap();
        worker.start(&completion_request()).unwrap();
        assert_eq!(worker.timeout, LOOKUP_TIMEOUT);
        let transport_budget = Duration::from_secs(4);
        let deadline = Instant::now() + transport_budget;
        let started = worker.started;
        let mut progress = 0;
        loop {
            let response = worker
                .poll()
                .unwrap_or_else(|error| panic!("after {progress} progress frames: {error}"));
            if let Some(response) = response {
                match response {
                    Response::Completion(Outcome::Progress(_)) => {
                        progress += 1;
                        assert!(worker.busy);
                        assert!(!worker.stopping);
                        assert_eq!(
                            worker.timeout,
                            if progress == 1 {
                                INDEX_TIMEOUT
                            } else {
                                transport_budget
                            }
                        );
                        assert_eq!(worker.started, started);
                        // Later progress must preserve the oversized transport fixture's budget.
                        worker.timeout = transport_budget;
                    }
                    Response::Completion(Outcome::Ready { answer, .. }) => {
                        assert!(matches!(answer.state, State::Partial(_)));
                        assert!(!worker.busy);
                        assert!(worker.stopping);
                        break;
                    }
                    other => panic!("unexpected completion packet: {other:?}"),
                }
            }
            assert!(Instant::now() < deadline, "coalesced frame watchdog");
            // Fixed sleeps throttle platforms with small Unix socket buffers.
            filedescriptor::poll(
                &mut [libc::pollfd {
                    fd: worker.stream.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                }],
                Some(deadline.saturating_duration_since(Instant::now())),
            )
            .unwrap();
        }
        assert_eq!(progress, 3);
        stop_completion(&mut worker);
    }

    #[test]
    fn progress_cannot_retire_and_final_frames_cannot_have_unsolicited_trailers() {
        use crate::completion::types::{Outcome, State};
        let launch = crate::input_assist::tests::launcher("input_assist::tests::worker_probe");
        let mut worker = Worker::spawn(&launch, Kind::Completion).unwrap();
        worker.incoming = frame(&Envelope {
            response: Response::Completion(Outcome::Progress(completion_answer(State::Partial(
                "querying".into(),
            )))),
            retire: true,
        })
        .unwrap();
        assert_eq!(
            worker.take_response().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        worker.incoming = frame(&Envelope {
            response: Response::Completion(Outcome::Ready {
                answer: completion_answer(State::Complete),
                snapshot: None,
            }),
            retire: false,
        })
        .unwrap();
        worker.incoming.extend(frame(&"unsolicited").unwrap());
        assert_eq!(
            worker.take_response().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        stop_completion(&mut worker);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn untagged_session_child_probe() {
        use crate::completion::types::{Outcome, State};
        if std::env::var(WORKER_ENV).is_err() {
            return;
        }
        // SAFETY: this explicit probe owns Worker::spawn's inherited socket.
        let mut stream = unsafe { UnixStream::from_raw_fd(SOCKET_FD) };
        assert!(unsafe { libc::fcntl(SOCKET_FD, libc::F_SETFD, libc::FD_CLOEXEC) } >= 0);
        read_frame(&mut stream).unwrap().unwrap();
        let mut command = Command::new("/bin/sleep");
        command
            .arg("30")
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        // SAFETY: this creates a separate group inside the inherited worker session.
        unsafe {
            command.pre_exec(|| {
                if libc::setpgid(0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut child = command.spawn().unwrap();
        stream
            .write_all(
                &frame(&Envelope {
                    response: Response::Completion(Outcome::Progress(completion_answer(
                        State::Partial("child started".into()),
                    ))),
                    retire: false,
                })
                .unwrap(),
            )
            .unwrap();
        let _ = read_frame(&mut stream);
        let _ = child.kill();
        let _ = child.wait();
        std::process::exit(0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn cleared_environment_children_remain_owned_after_the_leader_is_killed() {
        let launch = crate::input_assist::tests::launcher(
            "input_assist::worker::tests::untagged_session_child_probe",
        );
        let mut worker = Worker::spawn(&launch, Kind::Completion).unwrap();
        worker.start(&completion_request()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if worker.poll().unwrap().is_some() {
                break;
            }
            assert!(Instant::now() < deadline, "session child startup watchdog");
            std::thread::sleep(Duration::from_millis(2));
        }
        let handle = worker.handle();
        let owner = handle.lock().unwrap();
        let leader = owner.id() as i32;
        let marker = owner.marker().unwrap().to_owned();
        assert!(owner.helpers.is_empty());
        drop(owner);
        let children = worker.residual_children().unwrap();
        assert_eq!(children.len(), 1);
        let process = children[0];
        assert_ne!(process.pgid, leader);
        assert_eq!(unsafe { libc::getsid(process.pid) }, leader);
        assert!(crate::procs::TaggedProcess::open(process, &marker).is_err());
        let tracked = crate::procs::TaggedProcess::open_in_session(process, &marker, leader)
            .unwrap()
            .unwrap();
        let mut unrelated = Command::new("/bin/sleep")
            .arg("30")
            .env_clear()
            .spawn()
            .unwrap();
        assert!(
            crate::procs::TaggedProcess::open_in_session(
                crate::procs::Proc {
                    pid: unrelated.id() as i32,
                    pgid: 0
                },
                &marker,
                leader,
            )
            .is_err()
        );
        // Simulate nonblocking editor exit before the supervisor collected handles.
        kill_child(&handle).unwrap();
        stop_completion(&mut worker);
        assert!(tracked.gone().unwrap());
        assert!(unrelated.try_wait().unwrap().is_none());
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn detached_unreaped_helpers_hold_the_leader_and_unrelated_children_are_not_signalled() {
        let launch = crate::input_assist::tests::launcher("input_assist::tests::worker_probe");
        let mut worker = Worker::spawn(&launch, Kind::Completion).unwrap();
        let marker = worker.child.lock().unwrap().marker.clone().unwrap();
        let mut command = Command::new("/bin/sleep");
        command.arg("30").env(crate::procs::RUN_VAR, &marker);
        // SAFETY: setsid is async-signal-safe in this dedicated helper's pre_exec.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let mut helper = command.spawn().unwrap();
        let mut unrelated = Command::new("/bin/sleep")
            .arg("30")
            .env_remove(crate::procs::RUN_VAR)
            .spawn()
            .unwrap();
        let mut syntax = Worker::spawn(&launch, Kind::Syntax).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let children = worker.residual_children().unwrap();
            assert!(!children.iter().any(|p| p.pid == unrelated.id() as i32));
            assert!(
                !children
                    .iter()
                    .any(|p| p.pid == syntax.child.lock().unwrap().child.id() as i32)
            );
            if children.iter().any(|p| p.pid == helper.id() as i32) {
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(
            crate::procs::TaggedProcess::open(
                crate::procs::Proc {
                    pid: unrelated.id() as i32,
                    pgid: 0
                },
                &marker,
            )
            .is_err()
        );
        assert!(
            crate::procs::TaggedProcess::open_in_session(
                crate::procs::Proc {
                    pid: unrelated.id() as i32,
                    pgid: 0
                },
                &marker,
                worker.child.lock().unwrap().id() as i32,
            )
            .is_err()
        );
        worker.stop().unwrap();
        for _ in 0..3 {
            assert!(!worker.reaped().unwrap(), "exit is not descendant reaping");
            assert!(
                !worker.child.lock().unwrap().reaped,
                "keep the leader's identity"
            );
            std::thread::sleep(Duration::from_millis(4));
        }
        assert!(unrelated.try_wait().unwrap().is_none());
        assert!(
            syntax
                .child
                .lock()
                .unwrap()
                .child
                .try_wait()
                .unwrap()
                .is_none()
        );
        helper.wait().unwrap();
        stop_completion(&mut worker);
        syntax.stop().unwrap();
        while !syntax.reaped().unwrap() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(4));
        }
        unrelated.kill().unwrap();
        unrelated.wait().unwrap();
    }
}
