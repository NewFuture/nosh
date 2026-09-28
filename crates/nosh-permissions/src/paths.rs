//! Path resolution (lexical) and classification for write/read targets.

use std::borrow::Cow;
use std::cell::{OnceCell, RefCell};
use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

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
    resolve_literal(&expanded.to_string_lossy(), cwd)
}

/// Shell words have already undergone tilde/parameter expansion.
pub(crate) fn resolve_literal(p: &str, cwd: &Path) -> PathBuf {
    let path = Path::new(p);
    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
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
/// Also system directories on macOS (and `/Users` itself, like `/home`).
const MACOS_SYSTEM_DIRS: &[&str] = &[
    "/System",
    "/Library",
    "/Applications",
    "/Volumes",
    "/private",
];
const TEMP_DIRS: &[&str] = &["/tmp", "/var/tmp", "/dev/shm"];
const NULL_PATHS: &[&str] = &[
    "/dev/null",
    "/dev/stdout",
    "/dev/stderr",
    "/dev/tty",
    "/dev/zero",
];

pub(crate) fn protected_list(ctx: &Context) -> Vec<(PathBuf, String)> {
    let mut v: Vec<(PathBuf, String)> = vec![
        (PathBuf::from("/etc"), "/etc".into()),
        (PathBuf::from("/boot"), "/boot".into()),
    ];
    for h in ctx.user_home.iter().chain(ctx.home.iter()) {
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

/// `/etc/…`, `/tmp/…` or `/var/…` for `/private/etc/…`, `/private/tmp/…` or
/// `/private/var/…`: on macOS the short forms are symlinks to the long ones.
fn private_alias(p: &Path) -> Option<PathBuf> {
    let rest = p.strip_prefix("/private").ok()?;
    let first = rest.components().next()?.as_os_str();
    ["etc", "tmp", "var"]
        .iter()
        .any(|d| first == *d)
        .then(|| Path::new("/").join(rest))
}

/// The form paths are compared in. On macOS a path with its symlinks
/// resolved, `$TMPDIR` or a canonicalized workspace can name `/etc`, `/tmp`
/// or `/var` in the long `/private` form, so both sides use the short form.
fn short(p: &Path) -> Cow<'_, Path> {
    if cfg!(target_os = "macos")
        && let Some(s) = private_alias(p)
    {
        return Cow::Owned(s);
    }
    Cow::Borrowed(p)
}

/// Filesystem path facts reused only for one permission assessment.
type RealPathCache = Rc<RefCell<HashMap<(PathBuf, bool), Option<PathBuf>>>>;

#[derive(Clone)]
pub(crate) struct PathResolver {
    protected: Vec<(PathBuf, String)>,
    homes: Vec<PathBuf>,
    workspace: PathBuf,
    tmpdir: Option<PathBuf>,
    real_paths: RealPathCache,
}

impl PathResolver {
    pub(crate) fn new(ctx: &Context) -> Self {
        Self {
            protected: protected_list(ctx),
            homes: ctx
                .home
                .iter()
                .chain(ctx.user_home.iter())
                .cloned()
                .collect(),
            workspace: ctx.workspace.clone(),
            tmpdir: std::env::var("TMPDIR").ok().map(PathBuf::from),
            real_paths: Rc::new(RefCell::new(HashMap::new())),
        }
    }

    pub(crate) fn with_context(&self, ctx: &Context) -> Self {
        Self {
            real_paths: Rc::clone(&self.real_paths),
            ..Self::new(ctx)
        }
    }

    pub(crate) fn real_path(&self, path: &Path, follow_last: bool) -> Option<PathBuf> {
        let key = (path.to_path_buf(), follow_last);
        if let Some(cached) = self.real_paths.borrow().get(&key) {
            return cached.clone();
        }
        let resolved = real_path(path, follow_last);
        self.real_paths.borrow_mut().insert(key, resolved.clone());
        resolved
    }

    pub(crate) fn resolved_path(&self, path: &Path, follow_last: bool) -> PathBuf {
        self.real_path(path, follow_last)
            .unwrap_or_else(|| path.to_path_buf())
    }

    pub(crate) fn relative_path(&self, path: &Path, root: &Path) -> Option<PathBuf> {
        relative_path_with(path, root, |root| self.real_path(root, true))
    }

    pub(crate) fn classify(&self, path: &Path) -> PathClass {
        let path = &*short(path);
        let display = path.to_string_lossy();
        if NULL_PATHS.contains(&display.as_ref()) {
            return PathClass::Null;
        }
        for (base, label) in &self.protected {
            if self.relative_path(path, base).is_some() {
                return PathClass::Protected(label.clone());
            }
        }
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name == ".env" || name.starts_with(".env."))
        {
            return PathClass::Protected(".env".into());
        }
        if path == Path::new("/") {
            return PathClass::Root;
        }
        if self.homes.iter().any(|home| {
            self.relative_path(path, home)
                .is_some_and(|relative| relative.as_os_str().is_empty())
        }) {
            return PathClass::Home;
        }
        let workspace = short(&self.workspace);
        if !workspace.as_os_str().is_empty()
            && *workspace != *Path::new("/")
            && self.relative_path(path, &workspace).is_some()
            && self.real_path(&workspace, true).as_deref() != Some(Path::new("/"))
        {
            return PathClass::Workspace;
        }
        if TEMP_DIRS.iter().any(|temp| under(path, Path::new(temp)))
            || self.tmpdir.as_deref().is_some_and(|temp| {
                !temp.as_os_str().is_empty() && self.relative_path(path, temp).is_some()
            })
        {
            return PathClass::Temp;
        }
        let system = |dirs: &[&str]| dirs.iter().any(|dir| under(path, Path::new(dir)));
        if path == Path::new("/home")
            || system(SYSTEM_DIRS)
            || (cfg!(target_os = "macos")
                && (path == Path::new("/Users") || system(MACOS_SYSTEM_DIRS)))
        {
            return PathClass::System;
        }
        PathClass::Outside
    }

    pub(crate) fn classify_real(&self, path: &Path, follow_last: bool) -> (PathClass, PathBuf) {
        let lexical = self.classify(path);
        if matches!(lexical, PathClass::Protected(_) | PathClass::Null) {
            return (lexical, path.to_path_buf());
        }
        if let Some(real) = self.real_path(path, follow_last)
            && let class @ PathClass::Protected(_) = self.classify(&real)
        {
            return (class, real);
        }
        (lexical, path.to_path_buf())
    }

    pub(crate) fn classify_target(
        &self,
        lexical: &Path,
        resolved: &Path,
        follow_last: bool,
    ) -> PathClass {
        match self.classify_real(lexical, follow_last).0 {
            protected @ PathClass::Protected(_) => protected,
            _ => self.classify(resolved),
        }
    }

    pub(crate) fn protected(&self) -> &[(PathBuf, String)] {
        &self.protected
    }
}

/// Compare a resolved target with both spellings of a scope root. Resolve
/// the root, not the relative tail: an escaping child symlink stays outside.
fn relative_path_with(
    path: &Path,
    root: &Path,
    resolve_root: impl FnOnce(&Path) -> Option<PathBuf>,
) -> Option<PathBuf> {
    // Preserve literal ancestry before shortening aliases: /private/etc is
    // below /private even though its shorthand /etc no longer is.
    if let Ok(relative) = path.strip_prefix(root) {
        return Some(relative.to_path_buf());
    }
    let path = short(path);
    let root = short(root);
    path.strip_prefix(&root)
        .ok()
        .map(Path::to_path_buf)
        .or_else(|| {
            resolve_root(&root)
                .and_then(|real| path.strip_prefix(short(&real)).ok().map(Path::to_path_buf))
        })
}

#[cfg(test)]
fn relative_path(path: &Path, root: &Path) -> Option<PathBuf> {
    relative_path_with(path, root, |root| real_path(root, true))
}

pub(crate) fn same_workspace_target(lexical: &Path, resolved: &Path, ctx: &Context) -> bool {
    let real_workspace = OnceCell::new();
    let relative = |path| {
        relative_path_with(path, &ctx.workspace, |root| {
            real_workspace.get_or_init(|| real_path(root, true)).clone()
        })
    };
    short(lexical) == short(resolved)
        || relative(lexical).is_some_and(|lexical| Some(lexical) == relative(resolved))
}

/// `/` or a directory right under it, such as `/etc`; on macOS also the real
/// `/private/etc` and `/private/var` behind the `/etc` and `/var` symlinks.
pub(crate) fn is_top_level(p: &Path) -> bool {
    short(p).components().count() <= 2
}

pub fn classify_path(p: &Path, ctx: &Context) -> PathClass {
    PathResolver::new(ctx).classify(p)
}

/// Resolves symlinks in the existing part of an absolute, normalized path
/// (`follow_last`: also the final component, as opening a file does; `rm`
/// removes a link itself). `None` when nothing changes or it cannot be read.
pub fn real_path(p: &Path, follow_last: bool) -> Option<PathBuf> {
    #[cfg(test)]
    REAL_PATH_CALLS.with(|calls| calls.set(calls.get() + 1));
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
    PathResolver::new(ctx).classify_real(p, follow_last)
}

#[cfg(test)]
thread_local! {
    static REAL_PATH_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
fn reset_real_path_calls() {
    REAL_PATH_CALLS.with(|calls| calls.set(0));
}

#[cfg(test)]
fn real_path_calls() -> usize {
    REAL_PATH_CALLS.with(std::cell::Cell::get)
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

    #[test]
    fn private_aliases() {
        let a = |p: &str| private_alias(Path::new(p));
        assert_eq!(a("/private/etc/hosts"), Some(PathBuf::from("/etc/hosts")));
        assert_eq!(
            a("/private/var/folders/x/T"),
            Some("/var/folders/x/T".into())
        );
        assert_eq!(a("/private/tmp"), Some(PathBuf::from("/tmp")));
        assert_eq!(a("/private"), None);
        assert_eq!(a("/private/etcetera"), None);
        assert_eq!(a("/private/xarts/f"), None);
        assert_eq!(a("/etc/hosts"), None);
        assert_eq!(
            relative_path(Path::new("/private/etc"), Path::new("/private")),
            Some(PathBuf::from("etc"))
        );
        assert_eq!(
            relative_path(Path::new("/private/var/log"), Path::new("/private")),
            Some(PathBuf::from("var/log"))
        );
    }

    #[test]
    fn top_level_directories() {
        let top = |p: &str| is_top_level(Path::new(p));
        assert!(top("/") && top("/etc") && top("/private"));
        assert!(!top("/etc/ssh") && !top("/private/var/db"));
        // The real directories behind macOS's /etc and /var symlinks.
        let macos = cfg!(target_os = "macos");
        assert_eq!(top("/private/etc"), macos);
        assert_eq!(top("/private/var"), macos);
    }

    #[cfg(unix)]
    #[test]
    fn scope_root_aliases_preserve_home_and_protected_paths() {
        let dir = tempfile::tempdir().unwrap();
        let home = std::fs::canonicalize(dir.path()).unwrap().join("home");
        std::fs::create_dir_all(home.join("proj")).unwrap();
        let alias = dir.path().join("home-alias");
        std::os::unix::fs::symlink(&home, &alias).unwrap();
        let mut ctx = Context::new(alias.join("proj"), alias.join("proj")).with_home(&alias);
        ctx.protected.push(alias.join("private"));
        assert_eq!(classify_path(&home, &ctx), PathClass::Home);
        for path in [home.join(".ssh/key"), home.join("private/file")] {
            assert!(
                matches!(classify_path(&path, &ctx), PathClass::Protected(_)),
                "{path:?}"
            );
            let read = crate::assess_read("grep", &path, None, &ctx);
            assert!(read.reads_protected);
        }
        let root_alias = dir.path().join("root-alias");
        std::os::unix::fs::symlink("/", &root_alias).unwrap();
        let ctx = Context::new(&root_alias, &root_alias);
        assert_ne!(
            classify_path(Path::new("/opt/file"), &ctx),
            PathClass::Workspace
        );
    }

    #[test]
    fn scoped_resolver_reuses_root_and_target_resolutions() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        std::fs::create_dir(&workspace).unwrap();
        let first_path = workspace.join("first");
        let second_path = workspace.join("second");
        std::fs::write(&first_path, "").unwrap();
        std::fs::write(&second_path, "").unwrap();
        let mut ctx = Context::new(&workspace, &workspace);
        ctx.home = None;
        ctx.user_home = None;
        let paths = PathResolver::new(&ctx);

        reset_real_path_calls();
        assert_eq!(
            paths.classify_real(&first_path, true).0,
            PathClass::Workspace
        );
        let first_calls = real_path_calls();
        assert!(first_calls > 0);

        assert_eq!(
            paths.classify_real(&first_path, true).0,
            PathClass::Workspace
        );
        assert_eq!(real_path_calls(), first_calls);

        assert_eq!(
            paths.classify_real(&second_path, true).0,
            PathClass::Workspace
        );
        assert_eq!(real_path_calls(), first_calls + 1);
    }

    #[cfg(unix)]
    #[test]
    fn scoped_resolver_keeps_follow_last_semantics_separate() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("workspace");
        let protected = dir.path().join("protected");
        std::fs::create_dir(&workspace).unwrap();
        std::fs::write(&protected, "").unwrap();
        let link = workspace.join("link");
        std::os::unix::fs::symlink(&protected, &link).unwrap();
        let mut ctx = Context::new(&workspace, &workspace);
        ctx.home = None;
        ctx.user_home = None;
        ctx.protected.push(protected);
        let paths = PathResolver::new(&ctx);

        assert!(matches!(
            paths.classify_real(&link, true).0,
            PathClass::Protected(_)
        ));
        assert_eq!(paths.classify_real(&link, false).0, PathClass::Workspace);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn canonical_macos_home_targets_remain_workspace_paths() {
        let ctx = ctx();
        let lexical = ctx.workspace.join("new-file");
        let canonical = real_path(&lexical, true).unwrap_or_else(|| lexical.clone());
        assert_eq!(classify_path(&canonical, &ctx), PathClass::Workspace);
        assert!(same_workspace_target(&lexical, &canonical, &ctx));
        let home = ctx.home_dir().unwrap();
        let canonical = real_path(home, true).unwrap_or_else(|| home.to_path_buf());
        assert_eq!(classify_path(&canonical, &ctx), PathClass::Home);
        assert!(matches!(
            classify_path(&canonical.join(".ssh/key"), &ctx),
            PathClass::Protected(_)
        ));
    }

    /// `/etc`, `/tmp` and `/var` are symlinks into `/private` on macOS.
    #[cfg(target_os = "macos")]
    #[test]
    fn classifies_macos_private_paths() {
        let c = ctx();
        let k = |p: &str| classify_path(Path::new(p), &c);
        assert!(matches!(k("/private/etc/hosts"), PathClass::Protected(_)));
        assert_eq!(k("/private/tmp/x"), PathClass::Temp);
        assert_eq!(k("/private/var/log/x"), PathClass::System);
        assert_eq!(k("/private/xarts/x"), PathClass::System);
        assert_eq!(k("/Library/LaunchDaemons/x.plist"), PathClass::System);
        assert_eq!(k("/Users"), PathClass::System);
        assert_eq!(k("/Users/other/f"), PathClass::Outside);
        let read = crate::assess_read("grep", Path::new("/private/etc"), None, &c);
        assert!(read.reads_protected, "{:?}", read.findings);
        assert!(
            read.operations[0]
                .paths
                .iter()
                .any(|path| { path.resolved.as_deref() == Some(Path::new("/private/etc")) })
        );
        // A canonicalized workspace and home are compared in the short form.
        let t = "/private/var/folders/x/T";
        let c = Context::new(format!("{t}/proj"), format!("{t}/proj")).with_home(format!("{t}/h"));
        let k = |p: &str| classify_path(Path::new(p), &c);
        assert_eq!(k("/var/folders/x/T/proj/a"), PathClass::Workspace);
        assert!(matches!(
            k("/var/folders/x/T/h/.ssh/id"),
            PathClass::Protected(_)
        ));
        // A link to a file in /etc resolves to /private/etc.
        let dir = std::env::temp_dir().join(format!("nosh-private-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("hosts");
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink("/etc/hosts", &link).unwrap();
        let c = Context::new(&dir, &dir);
        let (class, real) = classify_path_real(&link, &c, true);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(class, PathClass::Protected(_)), "{class:?}");
        assert_eq!(real, Path::new("/private/etc/hosts"));
    }
}
