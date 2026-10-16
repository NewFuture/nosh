use std::fs::File;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::backend::Interrupts;

pub(crate) fn stdin_file() -> io::Result<File> {
    #[cfg(unix)]
    {
        use std::os::fd::AsFd;
        std::io::stdin()
            .as_fd()
            .try_clone_to_owned()
            .map(File::from)
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsHandle;
        std::io::stdin()
            .as_handle()
            .try_clone_to_owned()
            .map(File::from)
    }
}

pub(crate) fn resolve_executable(path: &Path) -> Option<PathBuf> {
    brush_core::sys::fs::resolve_executable(path.to_owned()).filter(|path| path.is_file())
}

pub(crate) fn contains_separator(name: &str) -> bool {
    brush_core::sys::fs::contains_path_separator(name)
}

pub(crate) enum Access {
    Read,
    Write,
    Execute,
}

pub(crate) fn check_access(path: &Path, access: Access) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
        let mode = match access {
            Access::Read => libc::R_OK,
            Access::Write => libc::W_OK,
            Access::Execute => libc::X_OK,
        };
        // SAFETY: access inspects a NUL-terminated path using real-user credentials.
        if unsafe { libc::access(path.as_ptr(), mode) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
    #[cfg(windows)]
    {
        match access {
            Access::Read => File::open(path).map(|_| ()),
            Access::Write => File::options().write(true).open(path).map(|_| ()),
            Access::Execute if path.is_dir() || resolve_executable(path).is_some() => {
                use std::os::windows::fs::OpenOptionsExt;
                use windows_sys::Win32::Storage::FileSystem::{
                    FILE_EXECUTE, FILE_FLAG_BACKUP_SEMANTICS,
                };
                let result = File::options()
                    .access_mode(FILE_EXECUTE)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS)
                    .open(path)
                    .map(|_| ());
                match result {
                    // Traversal can be allowed by SeChangeNotifyPrivilege even when
                    // opening a directory handle is denied; that evidence is unknown.
                    Err(error)
                        if path.is_dir() && error.kind() == io::ErrorKind::PermissionDenied =>
                    {
                        Err(io::Error::other(format!(
                            "cannot inspect directory traversal access: {error}"
                        )))
                    }
                    result => result,
                }
            }
            Access::Execute => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "not an executable file",
            )),
        }
    }
}

pub(crate) fn install_interrupts(
    runtime: &tokio::runtime::Runtime,
    interrupts: Arc<Interrupts>,
    interactive: bool,
) -> io::Result<()> {
    let _guard = runtime.enter();
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut input = signal(SignalKind::interrupt())?;
        runtime.spawn(async move {
            while input.recv().await.is_some() {
                interrupts.fire();
            }
        });
        if interactive {
            for kind in [
                SignalKind::quit(),
                SignalKind::terminate(),
                SignalKind::from_raw(libc::SIGTSTP),
            ] {
                let mut input = signal(kind)?;
                runtime.spawn(async move { while input.recv().await.is_some() {} });
            }
        }
    }
    #[cfg(windows)]
    {
        let _ = interactive;
        let mut input = tokio::signal::windows::ctrl_c()?;
        let mut breaks = tokio::signal::windows::ctrl_break()?;
        runtime.spawn(async move {
            loop {
                tokio::select! {
                    event = input.recv() => if event.is_none() { break },
                    event = breaks.recv() => if event.is_none() { break },
                }
                interrupts.fire();
            }
        });
    }
    Ok(())
}

#[derive(Clone, Copy)]
pub(crate) enum Stop {
    Interrupt,
    Timeout,
}

pub(crate) struct Execution {
    #[cfg(unix)]
    before: std::collections::HashSet<i32>,
    #[cfg(unix)]
    marker: String,
    #[cfg(windows)]
    group: nosh_platform::process::ProcessGroup,
}

impl Execution {
    pub fn new(marker: &str) -> io::Result<Self> {
        #[cfg(unix)]
        {
            Ok(Self {
                before: crate::procs::child_pids(),
                marker: marker.into(),
            })
        }
        #[cfg(windows)]
        {
            let _ = marker;
            Ok(Self {
                group: nosh_platform::process::ProcessGroup::new(None)?,
            })
        }
    }

    pub fn configure(&self, parameters: &mut brush_core::ExecutionParameters) {
        #[cfg(unix)]
        let _ = parameters;
        #[cfg(windows)]
        {
            use std::os::windows::io::BorrowedHandle;
            let group = self.group.clone();
            parameters.reject_background_jobs = true;
            parameters.process_spawner = Some(Arc::new(move |mut command| {
                group.prepare(&mut command)?;
                let mut command = tokio::process::Command::from(command);
                command.kill_on_drop(true);
                let mut child = command.spawn()?;
                let pid = child
                    .id()
                    .ok_or_else(|| io::Error::other("missing child PID"))?;
                let handle = child
                    .raw_handle()
                    .ok_or_else(|| io::Error::other("missing child handle"))?;
                // SAFETY: child owns this process handle throughout job assignment.
                let result = group.attach(pid, unsafe { BorrowedHandle::borrow_raw(handle) });
                if let Err(error) = result {
                    let killed = child.start_kill();
                    tokio::spawn(async move {
                        if let Err(error) = child.wait().await {
                            eprintln!("nosh: failed spawn cleanup: {error}");
                        }
                    });
                    return Err(io::Error::other(match killed {
                        Ok(()) => format!("cannot own child process: {error}"),
                        Err(cleanup) => {
                            format!("cannot own child process: {error}; cleanup: {cleanup}")
                        }
                    }));
                }
                Ok(child)
            }));
        }
    }

    pub fn stop(&self, reason: Stop) -> io::Result<()> {
        #[cfg(unix)]
        {
            use crate::procs;
            let targets = procs::new_targets(&self.before, &self.marker);
            procs::signal(
                &targets,
                match reason {
                    Stop::Interrupt => libc::SIGINT,
                    Stop::Timeout => libc::SIGTERM,
                },
            );
            if !targets.is_empty() {
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    procs::signal(&targets, libc::SIGKILL);
                });
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            let _ = reason;
            self.group.terminate()
        }
    }

    pub fn finish(&self) -> io::Result<bool> {
        #[cfg(unix)]
        {
            Ok(false)
        }
        #[cfg(windows)]
        {
            let stopped_background = self.group.active()? != 0;
            self.group.finish()?;
            Ok(stopped_background)
        }
    }
}

pub(crate) fn stop_groups(groups: &[i32]) -> io::Result<()> {
    #[cfg(unix)]
    {
        crate::procs::signal_groups(groups, libc::SIGKILL);
        crate::procs::signal_groups(groups, libc::SIGCONT);
        Ok(())
    }
    #[cfg(windows)]
    {
        if groups.is_empty() {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "POSIX stopped jobs are unavailable on Windows",
            ))
        }
    }
}

#[cfg(unix)]
pub(crate) struct SuspendGuard(Option<libc::termios>);

#[cfg(unix)]
impl SuspendGuard {
    pub fn new(active: bool) -> Self {
        if !active {
            return Self(None);
        }
        // SAFETY: only the current controlling terminal's suspend character is changed.
        unsafe {
            if libc::isatty(0) != 1 {
                return Self(None);
            }
            let mut attributes = std::mem::zeroed();
            if libc::tcgetattr(0, &mut attributes) != 0 {
                return Self(None);
            }
            let saved = attributes;
            attributes.c_cc[libc::VSUSP] = 0;
            if libc::tcsetattr(0, libc::TCSANOW, &attributes) != 0 {
                return Self(None);
            }
            Self(Some(saved))
        }
    }
}

#[cfg(unix)]
impl Drop for SuspendGuard {
    fn drop(&mut self) {
        if let Some(attributes) = &self.0 {
            // SAFETY: restore the attributes retained from this same terminal.
            unsafe { libc::tcsetattr(0, libc::TCSANOW, attributes) };
        }
    }
}

#[cfg(windows)]
pub(crate) struct SuspendGuard;

#[cfg(windows)]
impl SuspendGuard {
    pub fn new(_: bool) -> Self {
        Self
    }
}
