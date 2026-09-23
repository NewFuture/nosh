//! Tracking and signalling the processes an agent command spawns.
//!
//! brush does not expose the pids of running children, so on Linux the
//! process tree is read from `/proc`. Children that existed before an agent
//! command started (user background jobs) and their descendants are left alone.

use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Proc {
    pub pid: i32,
    pub pgid: i32,
}

/// `(pid, ppid, pgid)` of every visible process (Linux; empty elsewhere).
fn all_procs() -> Vec<(i32, i32, i32)> {
    let mut out = Vec::new();
    #[cfg(target_os = "linux")]
    {
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
            if let (Some(ppid), Some(pgid)) = (
                fields.get(1).and_then(|s| s.parse::<i32>().ok()),
                fields.get(2).and_then(|s| s.parse::<i32>().ok()),
            ) {
                out.push((pid, ppid, pgid));
            }
        }
    }
    out
}

/// Direct children of this process.
pub fn children() -> Vec<Proc> {
    let me = std::process::id() as i32;
    all_procs()
        .into_iter()
        .filter(|&(_, ppid, _)| ppid == me)
        .map(|(pid, _, pgid)| Proc { pid, pgid })
        .collect()
}

pub fn child_pids() -> HashSet<i32> {
    children().into_iter().map(|p| p.pid).collect()
}

/// Children not in `before`, and all their descendants.
pub fn new_descendants(before: &HashSet<i32>) -> Vec<Proc> {
    let me = std::process::id() as i32;
    let mut kids: HashMap<i32, Vec<Proc>> = HashMap::new();
    for (pid, ppid, pgid) in all_procs() {
        kids.entry(ppid).or_default().push(Proc { pid, pgid });
    }
    let mut out = Vec::new();
    let mut stack: Vec<Proc> = kids
        .get(&me)
        .map(|v| {
            v.iter()
                .filter(|p| !before.contains(&p.pid))
                .copied()
                .collect()
        })
        .unwrap_or_default();
    while let Some(p) = stack.pop() {
        if let Some(k) = kids.get(&p.pid) {
            stack.extend(k.iter().copied());
        }
        out.push(p);
    }
    out
}

/// What to signal to stop an agent command: its own process groups, plus
/// processes that stayed in nosh's group (command substitutions, pipeline
/// stages after a builtin), which must not get a group-wide signal.
#[derive(Debug, Clone, Default)]
pub struct Targets {
    pub groups: Vec<i32>,
    pub pids: Vec<i32>,
}

impl Targets {
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.pids.is_empty()
    }
}

pub fn new_targets(before: &HashSet<i32>) -> Targets {
    // SAFETY: getpgrp has no preconditions.
    let own = unsafe { libc::getpgrp() };
    let mut t = Targets::default();
    for p in new_descendants(before) {
        if p.pgid == own {
            t.pids.push(p.pid);
        } else if p.pgid > 1 {
            t.groups.push(p.pgid);
        }
    }
    t.groups.sort_unstable();
    t.groups.dedup();
    t
}

pub fn signal(t: &Targets, sig: i32) {
    signal_groups(&t.groups, sig);
    for &pid in &t.pids {
        if pid > 1 {
            // SAFETY: plain syscall; a stale pid just yields ESRCH.
            unsafe {
                libc::kill(pid, sig);
            }
        }
    }
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
