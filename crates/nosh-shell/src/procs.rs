//! Tracking and signalling the processes an agent command spawns.
//!
//! brush does not expose the pids of running children, so on Linux the
//! process tree is read from `/proc`. Children that existed before an agent
//! command started (user background jobs) and their descendants are left alone.
//! A process that double-forks or calls `setsid` leaves the tree once its
//! parent exits; it is still found by the run's value of [`RUN_VAR`], which
//! it inherited in its environment.

use std::collections::{HashMap, HashSet};

/// Set, with a value unique to each run, in an agent command's environment.
pub const RUN_VAR: &str = "NOSH_AGENT_RUN";

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

/// Whether `RUN_VAR=run` is in the environment `pid` was started with.
fn has_run(pid: i32, entry: &[u8]) -> bool {
    std::fs::read(format!("/proc/{pid}/environ"))
        .is_ok_and(|env| env.split(|&b| b == 0).any(|e| e == entry))
}

/// What the agent run `run` started: children not in `before`, processes
/// carrying the run in their environment wherever they were reparented to,
/// and all their descendants.
pub fn run_procs(before: &HashSet<i32>, run: &str) -> Vec<Proc> {
    let me = std::process::id() as i32;
    let entry = format!("{RUN_VAR}={run}").into_bytes();
    let mut kids: HashMap<i32, Vec<Proc>> = HashMap::new();
    let mut stack = Vec::new();
    for (pid, ppid, pgid) in all_procs() {
        let p = Proc { pid, pgid };
        kids.entry(ppid).or_default().push(p);
        if pid != me && ((ppid == me && !before.contains(&pid)) || has_run(pid, &entry)) {
            stack.push(p);
        }
    }
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    while let Some(p) = stack.pop() {
        if !seen.insert(p.pid) {
            continue;
        }
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

pub fn new_targets(before: &HashSet<i32>, run: &str) -> Targets {
    // SAFETY: getpgrp has no preconditions.
    let own = unsafe { libc::getpgrp() };
    let mut t = Targets::default();
    for p in run_procs(before, run) {
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
