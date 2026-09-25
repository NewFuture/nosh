//! Tracking and signalling the processes an agent command spawns.
//!
//! brush does not expose the pids of running children, so the process tree is
//! read from the system: `/proc` on Linux, libproc on macOS (elsewhere no
//! process is found and nothing is signalled). Children that existed before
//! an agent command started (user background jobs) and their descendants are
//! left alone. A process that double-forks or calls `setsid` leaves the tree
//! once its parent exits; it is still found by the run's value of
//! [`RUN_VAR`], which it inherited in its environment (readable for the
//! user's own processes).

use std::collections::{HashMap, HashSet};

/// Set, with a value unique to each run, in an agent command's environment.
pub const RUN_VAR: &str = "NOSH_AGENT_RUN";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Proc {
    pub pid: i32,
    pub pgid: i32,
}

/// `(pid, ppid, pgid)` of every visible process.
#[cfg(target_os = "linux")]
fn all_procs() -> Vec<(i32, i32, i32)> {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let mut out = Vec::new();
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
    out
}

/// `(pid, ppid, pgid)` of every visible process.
#[cfg(target_os = "macos")]
fn all_procs() -> Vec<(i32, i32, i32)> {
    // SAFETY: with a null buffer libproc only reports the number of pids.
    let n = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(n) = usize::try_from(n) else {
        return Vec::new();
    };
    // Room for processes started since the count.
    let mut pids: Vec<libc::pid_t> = vec![0; n + 64];
    let bytes = libc::c_int::try_from(pids.len() * size_of::<libc::pid_t>()).unwrap_or(0);
    // SAFETY: `pids` is writable for `bytes` bytes.
    let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(n).unwrap_or(0));
    let size = size_of::<libc::proc_bsdinfo>() as libc::c_int;
    let mut out = Vec::with_capacity(pids.len());
    for pid in pids {
        // SAFETY: integers and byte arrays only; all zeros is a valid value.
        let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
        // SAFETY: `info` is a writable `proc_bsdinfo` of `size` bytes.
        let got = unsafe {
            libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size)
        };
        // Processes that exited meanwhile (and zombies) have no BSD info.
        if got == size {
            out.push((pid, info.pbi_ppid as i32, info.pbi_pgid as i32));
        }
    }
    out
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn all_procs() -> Vec<(i32, i32, i32)> {
    Vec::new()
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
#[cfg(target_os = "linux")]
fn has_run(pid: i32, entry: &[u8]) -> bool {
    std::fs::read(format!("/proc/{pid}/environ"))
        .is_ok_and(|env| env.split(|&b| b == 0).any(|e| e == entry))
}

/// Whether `RUN_VAR=run` is in the environment `pid` was started with.
#[cfg(target_os = "macos")]
fn has_run(pid: i32, entry: &[u8]) -> bool {
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    let mut size: libc::size_t = 0;
    // SAFETY: with a null buffer sysctl only reports the size into `size`.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            std::ptr::null_mut(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return false;
    }
    let mut buf = vec![0u8; size];
    // SAFETY: `buf` is writable for `size` bytes; sysctl stores the length used.
    let r = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if r != 0 {
        return false;
    }
    buf.truncate(size);
    procargs_env_has(&buf, entry)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn has_run(_: i32, _: &[u8]) -> bool {
    false
}

/// Whether `entry` is one of the environment strings in a `KERN_PROCARGS2`
/// buffer: argc, the executable path, NUL padding, argc arguments, then the
/// environment up to an empty string (other strings follow it).
#[cfg(any(target_os = "macos", test))]
fn procargs_env_has(buf: &[u8], entry: &[u8]) -> bool {
    let Some((argc, rest)) = buf.split_first_chunk::<4>() else {
        return false;
    };
    let argc = usize::try_from(i32::from_ne_bytes(*argc)).unwrap_or(usize::MAX);
    let mut parts = rest.split(|&b| b == 0);
    // The executable path.
    parts.next();
    parts
        .skip_while(|s| s.is_empty())
        .skip(argc)
        .take_while(|s| !s.is_empty())
        .any(|e| e == entry)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn procargs_environment() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(
            b"/bin/sleep\0\0\0\0sleep\0NOSH_AGENT_RUN=1.2\0HOME=/h\0NOSH_AGENT_RUN=7.8\0\0\
              executable_path=/bin/sleep\0",
        );
        let has = |e: &str| procargs_env_has(&buf, e.as_bytes());
        assert!(has("NOSH_AGENT_RUN=7.8"));
        assert!(has("HOME=/h"));
        assert!(
            !has("NOSH_AGENT_RUN=1.2"),
            "an argument, not the environment"
        );
        assert!(!has("executable_path=/bin/sleep"), "after the environment");
        assert!(!procargs_env_has(&[1, 0], b"HOME=/h"));
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn finds_children_and_their_environment() {
        use std::time::{Duration, Instant};
        let entry = |run: &str| format!("{RUN_VAR}={run}").into_bytes();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .env(RUN_VAR, "test.1")
            .spawn()
            .unwrap();
        let pid = child.id() as i32;
        // Until it has exec'd, the child still shows this process's environment.
        let deadline = Instant::now() + Duration::from_secs(5);
        while !has_run(pid, &entry("test.1")) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let found = has_run(pid, &entry("test.1"));
        let other = has_run(pid, &entry("test.2"));
        let kid = children().into_iter().find(|p| p.pid == pid);
        let _ = child.kill();
        let _ = child.wait();
        assert!(found, "{RUN_VAR} not found in the child's environment");
        assert!(!other);
        // SAFETY: getpgrp has no preconditions.
        let own = unsafe { libc::getpgrp() };
        assert_eq!(kid, Some(Proc { pid, pgid: own }));
    }
}
