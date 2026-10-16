use std::io::{self, Read};
use std::os::windows::io::{AsHandle, AsRawHandle, BorrowedHandle, FromRawHandle, OwnedHandle};
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_NO_MORE_FILES, ERROR_PIPE_NOT_CONNECTED, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Security::Cryptography::{
    BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOB_OBJECT_LIMIT_PROCESS_MEMORY, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows_sys::Win32::System::Pipes::PeekNamedPipe;
use windows_sys::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_SUSPENDED, DETACHED_PROCESS, OpenThread, ResumeThread,
    THREAD_SUSPEND_RESUME,
};

/// An execution's owned process tree, not the shell or its existing background jobs.
#[derive(Clone)]
pub struct ProcessGroup(Arc<Group>);

struct Group {
    handle: OwnedHandle,
    closed: Mutex<bool>,
}

impl ProcessGroup {
    pub fn new(memory_limit: Option<usize>) -> io::Result<Self> {
        // SAFETY: default security, unnamed job; ownership is transferred once.
        let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let group = Self(Arc::new(Group {
            handle: unsafe { OwnedHandle::from_raw_handle(handle) },
            closed: Mutex::new(false),
        }));
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        if let Some(bytes) = memory_limit {
            limits.BasicLimitInformation.LimitFlags |= JOB_OBJECT_LIMIT_PROCESS_MEMORY;
            limits.ProcessMemoryLimit = bytes;
        }
        // SAFETY: the job is owned and the information buffer has the declared size.
        if unsafe {
            SetInformationJobObject(
                group.0.handle.as_raw_handle(),
                JobObjectExtendedLimitInformation,
                (&raw const limits).cast(),
                std::mem::size_of_val(&limits) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(group)
    }

    /// Suspend before user code can run, so descendants cannot escape job assignment.
    pub fn prepare(&self, command: &mut Command) -> io::Result<()> {
        if *self
            .0
            .closed
            .lock()
            .map_err(|_| io::Error::other("process ownership lock poisoned"))?
        {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "execution scope has ended",
            ));
        }
        command.creation_flags(CREATE_SUSPENDED | CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS);
        Ok(())
    }

    pub fn attach(&self, pid: u32, process: BorrowedHandle<'_>) -> io::Result<()> {
        let closed = self
            .0
            .closed
            .lock()
            .map_err(|_| io::Error::other("process ownership lock poisoned"))?;
        if *closed {
            return Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "execution scope has ended",
            ));
        }
        // SAFETY: both handles are retained for this operation.
        if unsafe {
            AssignProcessToJobObject(self.0.handle.as_raw_handle(), process.as_raw_handle())
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        // std/Tokio retain the process handle but not its primary thread handle.
        // The suspended child cannot create threads before we resume its entry thread.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }
        let snapshot = unsafe { OwnedHandle::from_raw_handle(snapshot) };
        let mut entry = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        let mut found = unsafe { Thread32First(snapshot.as_raw_handle(), &raw mut entry) };
        while found != 0 {
            if entry.th32OwnerProcessID == pid {
                let thread = unsafe { OpenThread(THREAD_SUSPEND_RESUME, 0, entry.th32ThreadID) };
                if thread.is_null() {
                    return Err(io::Error::last_os_error());
                }
                let thread = unsafe { OwnedHandle::from_raw_handle(thread) };
                if unsafe { ResumeThread(thread.as_raw_handle()) } == u32::MAX {
                    return Err(io::Error::last_os_error());
                }
                return Ok(());
            }
            found = unsafe { Thread32Next(snapshot.as_raw_handle(), &raw mut entry) };
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(ERROR_NO_MORE_FILES as i32) {
            return Err(error);
        }
        Err(io::Error::other("suspended child has no entry thread"))
    }

    pub fn spawn(&self, command: &mut Command) -> io::Result<Child> {
        self.prepare(command)?;
        let mut child = command.spawn()?;
        if let Err(error) = self.attach(child.id(), child.as_handle()) {
            let cleanup = child.kill().and_then(|()| child.wait().map(|_| ()));
            return Err(io::Error::other(match cleanup {
                Ok(()) => format!("cannot own child process: {error}"),
                Err(cleanup) => format!("cannot own child process: {error}; cleanup: {cleanup}"),
            }));
        }
        Ok(child)
    }

    pub fn terminate(&self) -> io::Result<()> {
        let mut closed = self
            .0
            .closed
            .lock()
            .map_err(|_| io::Error::other("process ownership lock poisoned"))?;
        *closed = true;
        // SAFETY: only processes assigned to this execution's retained job are stopped.
        if unsafe { TerminateJobObject(self.0.handle.as_raw_handle(), 130) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    pub fn active(&self) -> io::Result<u32> {
        let mut information = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        // SAFETY: this writable buffer matches the requested information class.
        if unsafe {
            QueryInformationJobObject(
                self.0.handle.as_raw_handle(),
                JobObjectBasicAccountingInformation,
                (&raw mut information).cast(),
                std::mem::size_of_val(&information) as u32,
                std::ptr::null_mut(),
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(information.ActiveProcesses)
    }

    pub fn finish(&self) -> io::Result<()> {
        self.terminate()?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.active()? != 0 {
            if Instant::now() >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "owned process tree is still being reclaimed",
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        Ok(())
    }
}

pub fn random_bytes(bytes: &mut [u8]) -> io::Result<()> {
    let length = u32::try_from(bytes.len())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "random buffer is too large"))?;
    // SAFETY: the system RNG fills only this writable buffer.
    let status = unsafe {
        BCryptGenRandom(
            std::ptr::null_mut(),
            bytes.as_mut_ptr(),
            length,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    if status < 0 {
        return Err(io::Error::other(format!("system RNG failed: {status:#x}")));
    }
    Ok(())
}

/// Bounded, nonblocking reads without changing a synchronous Windows pipe's mode.
pub fn drain_pipe(
    pipe: &mut (impl Read + AsRawHandle),
    bytes: &mut Vec<u8>,
    remaining: &mut usize,
) -> io::Result<bool> {
    let mut buffer = [0; 8192];
    for _ in 0..8 {
        let mut available = 0;
        // SAFETY: the handle belongs to a live retained pipe; only its byte count is read.
        if unsafe {
            PeekNamedPipe(
                pipe.as_raw_handle(),
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &raw mut available,
                std::ptr::null_mut(),
            )
        } == 0
        {
            let error = io::Error::last_os_error();
            return match error.raw_os_error() {
                Some(code)
                    if code == ERROR_BROKEN_PIPE as i32
                        || code == ERROR_PIPE_NOT_CONNECTED as i32 =>
                {
                    Ok(true)
                }
                _ => Err(error),
            };
        }
        if available == 0 || *remaining == 0 {
            return Ok(false);
        }
        let size = buffer.len().min(available as usize).min(*remaining);
        let count = pipe.read(&mut buffer[..size])?;
        if count == 0 {
            return Ok(true);
        }
        bytes.extend_from_slice(&buffer[..count]);
        *remaining -= count;
    }
    Ok(false)
}

pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub status: Option<ExitStatus>,
    pub complete: bool,
}

pub fn capture(
    command: &mut Command,
    timeout: Duration,
    limit: usize,
    cancelled: impl Fn() -> bool,
) -> io::Result<Captured> {
    if cancelled() {
        return Err(io::Error::new(
            io::ErrorKind::Interrupted,
            "command cancelled",
        ));
    }
    if timeout.is_zero() {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
    }
    if limit == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "capture limit must be positive",
        ));
    }
    let group = ProcessGroup::new(None)?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = group.spawn(command)?;
    let result = (|| {
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("missing stdout"))?;
        let mut stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("missing stderr"))?;
        let mut output = Captured {
            stdout: Vec::new(),
            stderr: Vec::new(),
            status: None,
            complete: false,
        };
        let deadline = Instant::now() + timeout;
        let mut remaining = limit;
        loop {
            if cancelled() {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "command cancelled",
                ));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "command timed out"));
            }
            let out_done = drain_pipe(&mut stdout, &mut output.stdout, &mut remaining)?;
            let err_done = drain_pipe(&mut stderr, &mut output.stderr, &mut remaining)?;
            if remaining == 0 {
                return Ok(output);
            }
            if out_done
                && err_done
                && let Some(status) = child.try_wait()?
            {
                output.status = Some(status);
                output.complete = true;
                return Ok(output);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    })();
    group.finish()?;
    child.wait()?;
    result
}
