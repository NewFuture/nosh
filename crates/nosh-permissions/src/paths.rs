//! Path resolution (lexical) and classification for write/read targets.

use std::path::{Component, Path, PathBuf};

use crate::Context;

/// Expands `~`, makes the path absolute against `cwd` and normalizes `.`/`..`
/// lexically (symlinks are not followed).
pub fn resolve(p: &str, cwd: &Path, home: Option<&Path>) -> PathBuf {
    let expanded: PathBuf = if p == "~" {
        home.map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("/root"))
    } else if let Some(rest) = p.strip_prefix("~/") {
        home.map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from("/root").join(rest))
    } else {
        PathBuf::from(p)
    };
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let mut out = PathBuf::from("/");
    for c in abs.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::Normal(n) => out.push(n),
            Component::CurDir | Component::RootDir | Component::Prefix(_) => {}
        }
    }
    out
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PathClass {
    /// `/dev/null` and friends.
    Null,
    /// Sensitive location (keys, credentials, nosh's own config/state, /etc, /boot).
    Protected(String),
    Workspace,
    Temp,
    /// Exactly `/`.
    Root,
    /// Exactly the home directory.
    Home,
    /// Under a system directory (`/usr`, `/var`, …).
    System,
    Outside,
}

const SYSTEM_DIRS: &[&str] = &[
    "/bin", "/sbin", "/usr", "/lib", "/lib32", "/lib64", "/var", "/opt", "/sys", "/proc", "/dev",
    "/root", "/snap", "/srv", "/mnt", "/media", "/run",
];
const TEMP_DIRS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm"];
const NULL_PATHS: &[&str] = &[
    "/dev/null",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/zero",
];

fn protected_list(ctx: &Context) -> Vec<(PathBuf, String)> {
    let mut v: Vec<(PathBuf, String)> = vec![
        (PathBuf::from("/etc"), "/etc".into()),
        (PathBuf::from("/boot"), "/boot".into()),
    ];
    if let Some(h) = ctx.home_dir() {
        for rel in [
            ".ssh",
            ".gnupg",
            ".aws",
            ".kube",
            ".docker/config.json",
            ".netrc",
            ".config/nosh",
            ".local/share/nosh/state",
            ".bashrc",
            ".bash_profile",
            ".profile",
        ] {
            v.push((h.join(rel), format!("~/{rel}")));
        }
    }
    for p in &ctx.protected {
        v.push((p.clone(), p.display().to_string()));
    }
    v
}

fn under(p: &Path, base: &Path) -> bool {
    p == base || p.starts_with(base)
}

pub fn classify_path(p: &Path, ctx: &Context) -> PathClass {
    let s = p.to_string_lossy();
    if NULL_PATHS.contains(&s.as_ref()) {
        return PathClass::Null;
    }
    for (base, label) in protected_list(ctx) {
        if under(p, &base) {
            return PathClass::Protected(label);
        }
    }
    if p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n == ".env" || n.starts_with(".env."))
    {
        return PathClass::Protected(".env".into());
    }
    if p == Path::new("/") {
        return PathClass::Root;
    }
    if ctx.home_dir() == Some(p) && ctx.workspace != p {
        return PathClass::Home;
    }
    if !ctx.workspace.as_os_str().is_empty()
        && under(p, &ctx.workspace)
        && ctx.workspace != Path::new("/")
    {
        return PathClass::Workspace;
    }
    let tmpdir = std::env::var("TMPDIR").ok();
    if TEMP_DIRS.iter().any(|t| under(p, Path::new(t)))
        || tmpdir
            .as_deref()
            .is_some_and(|t| !t.is_empty() && under(p, Path::new(t)))
    {
        return PathClass::Temp;
    }
    if ctx.home_dir() == Some(p) {
        return PathClass::Home;
    }
    if p == Path::new("/home") || SYSTEM_DIRS.iter().any(|d| under(p, Path::new(d))) {
        return PathClass::System;
    }
    PathClass::Outside
}

/// Resolves symlinks in the existing part of an absolute, normalized path
/// (`follow_last`: also the final component, as opening a file does; `rm`
/// removes a link itself). `None` when nothing changes or it cannot be read.
pub fn real_path(p: &Path, follow_last: bool) -> Option<PathBuf> {
    let (dir, last) = if follow_last {
        (p, None)
    } else {
        (p.parent()?, Some(p.file_name()?))
    };
    // Longest existing prefix, then the missing tail.
    let mut existing = dir;
    let mut tail = Vec::new();
    while std::fs::symlink_metadata(existing).is_err() {
        tail.push(existing.file_name()?);
        existing = existing.parent()?;
    }
    let mut real = match std::fs::canonicalize(existing) {
        Ok(r) => r,
        // A dangling link: take its target lexically.
        Err(_) => {
            let target = std::fs::read_link(existing).ok()?;
            let base = existing.parent().unwrap_or(Path::new("/"));
            resolve(&target.to_string_lossy(), base, None)
        }
    };
    for c in tail.iter().rev() {
        real.push(c);
    }
    if let Some(l) = last {
        real.push(l);
    }
    (real != p).then_some(real)
}

/// [`classify_path`], upgraded to `Protected` when the path reaches a
/// protected location through a symlink. Returns the class and the path it
/// applies to.
pub fn classify_path_real(p: &Path, ctx: &Context, follow_last: bool) -> (PathClass, PathBuf) {
    let lexical = classify_path(p, ctx);
    if matches!(lexical, PathClass::Protected(_) | PathClass::Null) {
        return (lexical, p.to_path_buf());
    }
    if let Some(real) = real_path(p, follow_last)
        && let c @ PathClass::Protected(_) = classify_path(&real, ctx)
    {
        return (c, real);
    }
    (lexical, p.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx() -> Context {
        Context::new("/home/u/proj/src", "/home/u/proj").with_home("/home/u")
    }

    #[test]
    fn resolves_relative_and_tilde() {
        let c = ctx();
        assert_eq!(c.resolve("../x"), PathBuf::from("/home/u/proj/x"));
        assert_eq!(c.resolve("~/.ssh/id"), PathBuf::from("/home/u/.ssh/id"));
        assert_eq!(c.resolve("/a/./b/../c"), PathBuf::from("/a/c"));
        assert_eq!(c.resolve("../../../../.."), PathBuf::from("/"));
    }

    #[test]
    fn classifies() {
        let c = ctx();
        let k = |p: &str| classify_path(&c.resolve(p), &c);
        assert_eq!(k("a.txt"), PathClass::Workspace);
        assert_eq!(k("/tmp/x"), PathClass::Temp);
        assert_eq!(k("/dev/null"), PathClass::Null);
        assert!(matches!(k("~/.ssh/id_rsa"), PathClass::Protected(_)));
        assert!(matches!(k("../.env"), PathClass::Protected(_)));
        assert!(matches!(k("/etc/hosts"), PathClass::Protected(_)));
        assert_eq!(k("/"), PathClass::Root);
        assert_eq!(k("~"), PathClass::Home);
        assert_eq!(k("/usr/local/bin/x"), PathClass::System);
        assert_eq!(k("/home"), PathClass::System);
        assert_eq!(k("~/other/f"), PathClass::Outside);
        assert_eq!(k("/data/f"), PathClass::Outside);
    }
}
