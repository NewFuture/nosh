//! Tracking and signalling the processes an agent command spawns.
//!
//! brush does not expose the pids of running children, so on Linux the
//! direct children of this process are read from `/proc`. Children that existed
//! before an agent command started (user background jobs) are left alone.

use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Proc {
    pub pid: i32,
    pub pgid: i32,
}

/// Direct children of this process (Linux; empty elsewhere).
pub fn children() -> Vec<Proc> {
    #[cfg(target_os = "linux")]
    {
        let me = std::process::id() as i32;
        let mut out = Vec::new();
        let Ok(dir) = std::fs::read_dir("/proc") else {
            return out;
        };
        for e in dir.flatten() {
            let name = e.file_name();
            let Some(pid) = name.to_str().and_then(|s| s.parse::<i32>().ok()) else {
                continue;
            };
            let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
                continue;
            };
            // pid (comm) state ppid pgrp ...; comm may contain spaces/parens.
            let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else {
                continue;
            };
            let fields: Vec<&str> = rest.split_whitespace().collect();
            let (Some(ppid), Some(pgid)) = (
                fields.get(1).and_then(|s| s.parse::<i32>().ok()),
                fields.get(2).and_then(|s| s.parse::<i32>().ok()),
            ) else {
                continue;
            };
            if ppid == me {
                out.push(Proc { pid, pgid });
            }
        }
        out
    }
    #[cfg(not(target_os = "linux"))]
    {
        Vec::new()
    }
}

/// Process groups of children that were not present in `before`.
pub fn new_groups(before: &HashSet<i32>) -> Vec<i32> {
    // SAFETY: getpgrp has no preconditions.
    let own = unsafe { libc::getpgrp() };
    let mut groups: Vec<i32> = children()
        .into_iter()
        .filter(|p| !before.contains(&p.pid) && p.pgid != own && p.pgid > 1)
        .map(|p| p.pgid)
        .collect();
    groups.sort_unstable();
    groups.dedup();
    groups
}

pub fn child_pids() -> HashSet<i32> {
    children().into_iter().map(|p| p.pid).collect()
}

/// Sends `sig` to every group in `groups`.
pub fn signal_groups(groups: &[i32], sig: i32) {
    for &g in groups {
        if g > 1 {
            // SAFETY: plain syscall; a stale group id just yields ESRCH.
            unsafe {
                libc::killpg(g, sig);
            }
        }
    }
}
