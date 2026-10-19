//! Host process observations and background launching, independent of inference.

use std::ffi::OsStr;
use std::process::Command;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Captured, ProcessGroup, capture, drain_pipe, random_bytes};

/// Background queries must not inherit or allocate an interactive console.
pub fn prepare_background(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(windows_sys::Win32::System::Threading::DETACHED_PROCESS);
    }
    #[cfg(not(windows))]
    let _ = command;
}

pub fn background_command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    prepare_background(&mut command);
    command
}

/// Private helpers that clear their environment still need Windows runtime services.
#[cfg(windows)]
pub fn inherit_runtime_environment(command: &mut Command) -> std::io::Result<()> {
    let root = std::env::var_os("SystemRoot")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            std::io::Error::other("SystemRoot is required by Windows runtime services")
        })?;
    command.env("SystemRoot", root);
    if let Some(extensions) = std::env::var_os("PATHEXT") {
        command.env("PATHEXT", extensions);
    }
    Ok(())
}

/// Current and peak resident memory in MiB, when the host exposes both.
pub fn rss_mb() -> Option<(f64, f64)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcess;
        let mut counters = PROCESS_MEMORY_COUNTERS {
            cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            ..Default::default()
        };
        // SAFETY: the process pseudo-handle is valid and counters is correctly sized.
        if unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) } == 0 {
            return None;
        }
        Some((
            counters.WorkingSetSize as f64 / 1048576.0,
            counters.PeakWorkingSetSize as f64 / 1048576.0,
        ))
    }
    #[cfg(not(windows))]
    {
        let s = std::fs::read_to_string("/proc/self/status").ok()?;
        let field = |name: &str| -> Option<f64> {
            s.lines()
                .find(|l| l.starts_with(name))?
                .split_whitespace()
                .nth(1)?
                .parse::<f64>()
                .ok()
                .map(|kb| kb / 1024.0)
        };
        Some((field("VmRSS:")?, field("VmHWM:")?))
    }
}

#[test]
#[cfg(windows)]
fn native_process_rss_is_observed_not_zero() {
    let (current, peak) = rss_mb().expect("Windows process memory counters");
    assert!(current > 0.0 && peak >= current);
}
