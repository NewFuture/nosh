use std::collections::{BTreeSet, HashMap, VecDeque};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

use super::*;

const MAX_CACHE: usize = 512;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024;
const MAX_PATH_DIRS: usize = 128;
const CACHE_AGE: Duration = Duration::from_secs(1);

#[derive(Clone)]
enum FileState {
    File { executable: bool },
    Directory { searchable: bool },
    Other,
    Missing,
    Denied(String),
    Unreadable(String),
}

impl FileState {
    fn bytes(&self) -> usize {
        match self {
            Self::Denied(error) | Self::Unreadable(error) => error.len(),
            _ => 0,
        }
    }

    fn search_denial(&self) -> Option<&str> {
        match self {
            Self::Denied(error) => Some(error),
            Self::Directory { searchable: false } => Some("directory is not searchable"),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(super) struct Lookup {
    session: Option<u64>,
    cache: HashMap<PathBuf, (Instant, FileState, usize)>,
    order: VecDeque<PathBuf>,
    bytes: usize,
    calls: u64,
}

impl Lookup {
    fn cached(&self, path: &Path) -> Option<&FileState> {
        self.cache
            .get(path)
            .and_then(|(time, result, _)| (time.elapsed() < CACHE_AGE).then_some(result))
    }

    fn stat(&mut self, path: &Path) -> FileState {
        if let Some(result) = self.cached(path) {
            return result.clone();
        }
        self.calls = self.calls.saturating_add(1);
        let result = match fs::metadata(path) {
            Ok(metadata) if metadata.is_dir() => match execute_access(path) {
                Ok(()) => FileState::Directory { searchable: true },
                Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                    FileState::Directory { searchable: false }
                }
                Err(error) => FileState::Unreadable(short_error(error)),
            },
            Ok(metadata) if metadata.is_file() => {
                if metadata.permissions().mode() & 0o111 == 0 {
                    FileState::File { executable: false }
                } else {
                    match execute_access(path) {
                        Ok(()) => FileState::File { executable: true },
                        Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                            FileState::File { executable: false }
                        }
                        Err(error) => FileState::Unreadable(short_error(error)),
                    }
                }
            }
            Ok(_) => FileState::Other,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => FileState::Missing,
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
                if execute_access(path)
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    FileState::Denied(short_error(error))
                } else {
                    FileState::Unreadable(short_error(error))
                }
            }
            Err(error) => FileState::Unreadable(short_error(error)),
        };
        if let Some((_, _, size)) = self.cache.remove(path) {
            self.bytes -= size;
            self.order.retain(|p| p != path);
        }
        let bytes = path.as_os_str().len() * 2 + result.bytes() + 128;
        if bytes <= MAX_CACHE_BYTES {
            while self.cache.len() >= MAX_CACHE || self.bytes + bytes > MAX_CACHE_BYTES {
                let Some(old) = self.order.pop_front() else {
                    break;
                };
                if let Some((_, _, size)) = self.cache.remove(&old) {
                    self.bytes -= size;
                }
            }
            self.cache
                .insert(path.to_owned(), (Instant::now(), result.clone(), bytes));
            self.order.push_back(path.to_owned());
            self.bytes += bytes;
        }
        result
    }

    pub(super) fn run(&mut self, input: &Input, queries: &[Query]) -> Vec<Observation> {
        if self.session != Some(input.version.session) {
            self.cache.clear();
            self.order.clear();
            self.bytes = 0;
            self.session = Some(input.version.session);
        }
        queries
            .iter()
            .take(MAX_QUERIES)
            .map(|query| self.one(&input.context, query))
            .collect()
    }

    pub(super) fn stats(&self) -> LookupStats {
        LookupStats {
            metadata_calls: self.calls,
            cache_entries: self.cache.len(),
            cache_bytes: self.bytes,
        }
    }

    fn one(&mut self, context: &Context, query: &Query) -> Observation {
        let result = match &query.kind {
            QueryKind::Command {
                path,
                ai_on_missing,
            } => self.command(context, &query.word, path.as_deref(), *ai_on_missing),
            QueryKind::Path(use_) => {
                let path = absolute(&context.cwd, &query.word);
                if *use_ == PathUse::Directory
                    && context.cdpath
                    && !Path::new(&query.word).is_absolute()
                    && !query.word.starts_with('.')
                {
                    (None, Some((State::Unknown, Reason::Dynamic)))
                } else {
                    match (self.stat(&path), use_) {
                        (FileState::Missing, PathUse::Write) => {
                            match path.parent().map(|p| self.stat(p)) {
                                Some(FileState::Directory { searchable: true }) => {
                                    (None, Some((State::Known, Reason::NewTarget)))
                                }
                                Some(FileState::Directory { searchable: false }) => (
                                    Some(Role::Error),
                                    Some((
                                        State::Error,
                                        Reason::AccessDenied(short_error(path.display())),
                                    )),
                                ),
                                Some(FileState::Missing) => {
                                    (Some(Role::Error), Some((State::Error, Reason::MissingPath)))
                                }
                                Some(FileState::Denied(_)) => (
                                    Some(Role::Error),
                                    Some((
                                        State::Error,
                                        Reason::AccessDenied(short_error(path.display())),
                                    )),
                                ),
                                Some(FileState::Unreadable(error)) => {
                                    (None, Some((State::Unknown, path_error(&path, &error))))
                                }
                                _ => (
                                    Some(Role::Error),
                                    Some((State::Error, Reason::NotDirectory)),
                                ),
                            }
                        }
                        (
                            FileState::Missing | FileState::Denied(_) | FileState::Unreadable(_),
                            PathUse::Argument,
                        ) => (None, None),
                        (FileState::Missing, PathUse::Explicit) => {
                            (None, Some((State::Unknown, Reason::MissingPath)))
                        }
                        (FileState::Missing, _) => {
                            (Some(Role::Error), Some((State::Error, Reason::MissingPath)))
                        }
                        (FileState::Denied(error), _) => {
                            let mode = match use_ {
                                PathUse::Read => Some(libc::R_OK),
                                PathUse::Write => Some(libc::W_OK),
                                PathUse::Directory => Some(libc::X_OK),
                                _ => None,
                            };
                            if mode.is_some_and(|mode| {
                                check_access(&path, mode).is_err_and(|e| {
                                    e.kind() == std::io::ErrorKind::PermissionDenied
                                })
                            }) {
                                (
                                    Some(Role::Error),
                                    Some((
                                        State::Error,
                                        Reason::AccessDenied(short_error(path.display())),
                                    )),
                                )
                            } else {
                                (None, Some((State::Unknown, path_error(&path, &error))))
                            }
                        }
                        (FileState::Directory { searchable: false }, PathUse::Directory) => (
                            Some(Role::Error),
                            Some((
                                State::Error,
                                Reason::AccessDenied(short_error(path.display())),
                            )),
                        ),
                        (FileState::Unreadable(error), _) => {
                            (None, Some((State::Unknown, path_error(&path, &error))))
                        }
                        (FileState::Directory { .. }, PathUse::Write) => (
                            Some(Role::Error),
                            Some((State::Error, Reason::DirectoryOutput)),
                        ),
                        (FileState::File { .. } | FileState::Other, PathUse::Directory) => (
                            Some(Role::Error),
                            Some((State::Error, Reason::NotDirectory)),
                        ),
                        (_, _) => (Some(Role::Path), None),
                    }
                }
            }
        };
        let (mut role, mut finding) = result;
        if !query.definite {
            let stale = role.is_some()
                || finding
                    .as_ref()
                    .is_some_and(|(state, _)| matches!(state, State::Known | State::Error));
            if stale {
                role = None;
                finding = Some((State::Unknown, Reason::Snapshot));
            }
        }
        Observation {
            range: query.range.clone(),
            role,
            finding: finding.map(|(state, reason)| Finding {
                range: query.range.clone(),
                state,
                reason,
            }),
        }
    }

    fn command(
        &mut self,
        context: &Context,
        name: &str,
        path: Option<&str>,
        ai: bool,
    ) -> (Option<Role>, Option<(State, Reason)>) {
        if name.contains('/') {
            let target = absolute(&context.cwd, name);
            return match self.stat(&target) {
                FileState::File { executable: true } => (Some(Role::External), None),
                FileState::Missing if ai => (Some(Role::Ai), Some((State::Known, Reason::Ai))),
                FileState::Missing => (
                    Some(Role::Error),
                    Some((State::Error, Reason::MissingCommand)),
                ),
                FileState::Denied(_) => (
                    Some(Role::Error),
                    Some((State::Error, Reason::NotExecutable)),
                ),
                FileState::Unreadable(error) => {
                    (None, Some((State::Unknown, path_error(&target, &error))))
                }
                _ if ai => (Some(Role::Ai), Some((State::Known, Reason::Ai))),
                _ => (
                    Some(Role::Error),
                    Some((State::Error, Reason::NotExecutable)),
                ),
            };
        }
        if path.is_some()
            && path == context.path.as_deref()
            && let Some(cached) = context.hashed_commands.get(name)
        {
            match self.stat(&context.cwd.join(cached)) {
                FileState::File { executable: true } => return (Some(Role::External), None),
                FileState::Denied(_) if !context.check_hash => {
                    return (
                        Some(Role::Error),
                        Some((State::Error, Reason::NotExecutable)),
                    );
                }
                FileState::Unreadable(error) => {
                    return (None, Some((State::Unknown, path_error(cached, &error))));
                }
                _ if !context.check_hash => {
                    return (
                        Some(Role::Error),
                        Some((State::Error, Reason::NotExecutable)),
                    );
                }
                _ => {}
            }
        }
        let Some(path) = path else {
            return if ai {
                (Some(Role::Ai), Some((State::Known, Reason::Ai)))
            } else {
                (
                    Some(Role::Error),
                    Some((State::Error, Reason::MissingCommand)),
                )
            };
        };
        let mut unreadable = None;
        let mut dirs = path.split(':');
        for dir in dirs.by_ref().take(MAX_PATH_DIRS) {
            let directory = absolute(&context.cwd, dir);
            if self
                .cached(&directory)
                .and_then(FileState::search_denial)
                .is_some()
            {
                continue;
            }
            let candidate = directory.join(name);
            match self.stat(&candidate) {
                FileState::File { executable: true } => {
                    return (Some(Role::External), None);
                }
                FileState::Denied(_) => {
                    // Cache a failed directory search, not each newly typed name.
                    // A per-file denial must not blacklist a searchable directory.
                    let directory_state = self.stat(&directory);
                    if directory_state.search_denial().is_none() {
                        // The directory is searchable, so this is just one
                        // confirmed non-executable candidate. Continue through
                        // the rest of PATH instead of making the whole lookup
                        // unknown.
                    }
                }
                FileState::Unreadable(error) => unreadable = Some(path_error(&candidate, &error)),
                _ => {}
            }
        }
        if dirs.next().is_some() {
            return (None, Some((State::Unavailable, Reason::Limit)));
        }
        if let Some(reason) = unreadable {
            return (None, Some((State::Unknown, reason)));
        }
        if ai {
            (Some(Role::Ai), Some((State::Known, Reason::Ai)))
        } else {
            (
                Some(Role::Error),
                Some((State::Error, Reason::MissingCommand)),
            )
        }
    }
}

fn path_error(path: &Path, error: &str) -> Reason {
    Reason::Io(short_error(format!("{}: {error}", path.display())))
}

fn execute_access(path: &Path) -> std::io::Result<()> {
    check_access(path, libc::X_OK)
}

fn check_access(path: &Path, mode: libc::c_int) -> std::io::Result<()> {
    let path = CString::new(path.as_os_str().as_bytes()).map_err(std::io::Error::other)?;
    // SAFETY: access reads a valid NUL-terminated path using the same real-user
    // credentials as brush, retaining the errno rather than collapsing it.
    if unsafe { libc::access(path.as_ptr(), mode) } == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn absolute(cwd: &Path, word: &str) -> PathBuf {
    let path = Path::new(word);
    if path.is_absolute() {
        path.to_owned()
    } else {
        cwd.join(path)
    }
}

pub(crate) fn scan_index(cwd: &Path, path: Option<&str>) -> Index {
    let mut names = BTreeSet::new();
    let mut bytes = 0;
    let mut visited = 0;
    let mut reason = None;
    let Some(path) = path else {
        return Index {
            cwd: cwd.to_owned(),
            path: None,
            names: Arc::new(Vec::new()),
            complete: true,
            reason: None,
        };
    };
    let mut directories = path.split(':');
    'directories: for directory in directories.by_ref().take(MAX_PATH_DIRS) {
        let entries = match fs::read_dir(absolute(cwd, directory)) {
            Ok(entries) => entries,
            Err(error) => {
                if error.kind() == std::io::ErrorKind::NotFound {
                    continue;
                }

                reason.get_or_insert_with(|| short_error(error));
                continue;
            }
        };
        for entry in entries {
            visited += 1;
            if visited > MAX_NAMES * 4 {
                reason = Some("command index scan limit".into());
                break 'directories;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    reason.get_or_insert_with(|| short_error(error));
                    continue;
                }
            };
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                reason.get_or_insert_with(|| "non-UTF-8 command name".into());
                continue;
            };
            if name.starts_with('.') || names.contains(name) {
                continue;
            }
            if names.len() >= MAX_NAMES || bytes + name.len() > MAX_INDEX_BYTES {
                reason = Some("command index limit".into());
                break 'directories;
            }
            bytes += name.len();
            names.insert(name.to_owned());
        }
    }
    if directories.next().is_some() {
        reason.get_or_insert_with(|| "PATH directory limit".into());
    }
    Index {
        cwd: cwd.to_owned(),
        path: Some(path.to_owned()),
        names: Arc::new(names.into_iter().collect()),
        complete: reason.is_none(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input_assist::tests::Fixture;
    use brush_core::sys::fs::PathExt;

    #[test]
    fn metadata_cache_is_bounded_and_new_prompt_snapshots_invalidate_it() {
        let fixture = Fixture::new();
        let cwd = fixture.context().cwd;
        let mut lookup = Lookup::default();
        for i in 0..MAX_CACHE + 100 {
            lookup.stat(&cwd.join(format!("missing-{i}")));
        }
        assert_eq!(lookup.cache.len(), MAX_CACHE);
        assert!(lookup.bytes <= MAX_CACHE_BYTES);
        let mut input = fixture.input("cat < absent");
        let analysis = analysis::analyze(&input);
        lookup.run(&input, &analysis.queries);
        let calls = lookup.calls;
        lookup.run(&input, &analysis.queries);
        assert_eq!(lookup.calls, calls);
        input.version.session += 1;
        lookup.run(&input, &analysis.queries);
        assert!(lookup.calls > calls);
    }

    #[test]
    fn inaccessible_path_directory_is_cached_without_hiding_other_commands() {
        let fixture = Fixture::new();
        let mut input = fixture.input("c");
        let original_path = input.context.path.clone();
        let directory = input.context.cwd.join("blocked");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o0)).unwrap();
        if directory.executable() {
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
            eprintln!("directory denial requires a non-privileged test user");
            return;
        }
        Arc::make_mut(&mut input.context).path = Some(directory.to_string_lossy().into_owned());
        let mut lookup = Lookup::default();
        let first = lookup.run(&input, &analysis::analyze(&input).queries);
        let first_calls = lookup.calls;
        input.text = "ca".into();
        input.version.input += 1;
        let second = lookup.run(&input, &analysis::analyze(&input).queries);
        let cached_calls = lookup.calls;
        Arc::make_mut(&mut input.context).path = Some(format!(
            "{}:{}",
            directory.display(),
            original_path.unwrap()
        ));
        input.text = "git".into();
        input.version.input += 1;
        let found = lookup.run(&input, &analysis::analyze(&input).queries);
        input.text = format!("{}/tool", directory.display());
        let explicit = lookup.run(&input, &analysis::analyze(&input).queries);

        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        let recovered = directory.join("recovered");
        fs::write(&recovered, "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(&recovered, fs::Permissions::from_mode(0o700)).unwrap();
        input.version.session += 1;
        input.text = "recovered".into();
        let refreshed = lookup.run(&input, &analysis::analyze(&input).queries);

        for observations in [first, second] {
            assert!(observations.iter().any(|o| {
                o.finding.as_ref().is_some_and(|f| {
                    f.state == State::Error && matches!(f.reason, Reason::MissingCommand)
                })
            }));
        }
        assert_eq!(
            cached_calls, first_calls,
            "new prefixes must reuse the directory result"
        );
        assert!(found.iter().any(|o| o.role == Some(Role::External)));
        assert!(explicit.iter().any(|o| {
            o.finding.as_ref().is_some_and(|f| {
                f.state == State::Error && matches!(f.reason, Reason::NotExecutable)
            })
        }));
        assert!(refreshed.iter().any(|o| o.role == Some(Role::External)));
    }

    #[test]
    fn inaccessible_is_not_unknown_but_io_failures_and_unchecked_paths_are() {
        let fixture = Fixture::new();
        let mut input = fixture.input("tool");
        let cwd = input.context.cwd.clone();
        Arc::make_mut(&mut input.context).path = Some(cwd.to_string_lossy().into_owned());
        let queries = analysis::analyze(&input).queries;
        let mut lookup = Lookup {
            session: Some(input.version.session),
            ..Lookup::default()
        };
        lookup.cache.insert(
            cwd.join("tool"),
            (
                Instant::now(),
                FileState::Unreadable("I/O failure".into()),
                0,
            ),
        );
        let result = lookup.run(&input, &queries);
        assert!(
            result[0]
                .finding
                .as_ref()
                .is_some_and(|f| f.state == State::Unknown)
        );
        assert_ne!(result[0].role, Some(Role::Error));

        input.text = "ai".into();
        let queries = analysis::analyze(&input).queries;
        lookup.cache.insert(
            cwd.join("ai"),
            (
                Instant::now(),
                FileState::Unreadable("I/O failure".into()),
                0,
            ),
        );
        assert_ne!(lookup.run(&input, &queries)[0].role, Some(Role::Ai));

        input.text = "tool".into();
        Arc::make_mut(&mut input.context).path = Some(
            std::iter::repeat_n("/missing_nosh_path", MAX_PATH_DIRS + 1)
                .collect::<Vec<_>>()
                .join(":"),
        );
        let queries = analysis::analyze(&input).queries;
        let result = Lookup::default().run(&input, &queries);
        assert!(
            result[0]
                .finding
                .as_ref()
                .is_some_and(|f| f.state == State::Unavailable)
        );
    }

    #[test]
    fn metadata_denial_is_not_proof_of_read_denial() {
        let fixture = Fixture::new();
        let input = fixture.input("cat < readable");
        let path = input.context.cwd.join("readable");
        fs::write(&path, "data").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let mut lookup = Lookup {
            session: Some(input.version.session),
            ..Lookup::default()
        };
        // Some filesystems distinguish attribute access from opening for reading.
        lookup.cache.insert(
            path,
            (
                Instant::now(),
                FileState::Denied("attributes denied".into()),
                0,
            ),
        );
        let result = lookup.run(&input, &analysis::analyze(&input).queries);
        assert!(
            !result
                .iter()
                .any(|o| o.finding.as_ref().is_some_and(|f| f.state == State::Error))
        );
        assert!(result.iter().any(|o| {
            o.finding
                .as_ref()
                .is_some_and(|f| f.state == State::Unknown)
        }));
    }

    #[test]
    fn a_directory_need_not_be_listable_to_resolve_a_known_command() {
        let fixture = Fixture::new();
        let mut input = fixture.input("known");
        let directory = input.context.cwd.join("search-only");
        fs::create_dir(&directory).unwrap();
        let executable = directory.join("known");
        fs::write(&executable, "#!/bin/sh\nexit 99\n").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o100)).unwrap();
        Arc::make_mut(&mut input.context).path = Some(directory.to_string_lossy().into_owned());
        let found = Lookup::default().run(&input, &analysis::analyze(&input).queries);
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(found.iter().any(|o| o.role == Some(Role::External)));
    }

    #[test]
    fn explicit_hash_entries_are_not_mistaken_for_missing_path_commands() {
        let fixture = Fixture::new();
        let mut input = fixture.input("cached");
        let executable = input.context.cwd.join("bin/git");
        Arc::make_mut(&mut input.context)
            .hashed_commands
            .insert("cached".into(), executable);
        let analysis = analysis::analyze(&input);
        let observations = Lookup::default().run(&input, &analysis.queries);
        assert!(observations.iter().any(|o| o.role == Some(Role::External)));
    }

    #[test]
    fn explicit_ai_hash_entry_wins_over_ai_fallback() {
        let fixture = Fixture::new();
        let mut input = fixture.input("ai explain");
        let executable = input.context.cwd.join("bin/git");
        Arc::make_mut(&mut input.context)
            .hashed_commands
            .insert("ai".into(), executable);
        let analysis = analysis::analyze(&input);
        let observations = Lookup::default().run(&input, &analysis.queries);
        assert!(observations.iter().any(|o| o.role == Some(Role::External)));
        assert!(!observations.iter().any(|o| o.role == Some(Role::Ai)));
    }
}
