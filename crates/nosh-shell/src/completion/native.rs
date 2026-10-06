use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::cache::{Cache, Entry, Set};
use super::context::Context;
use super::matching;
use super::types::*;

pub(crate) fn candidate(
    entry: &Entry,
    source: &Source,
    context: &Context,
    matches: Vec<usize>,
) -> Candidate {
    Candidate {
        source: source.clone(),
        value: if entry.kind == Kind::Directory {
            format!("{}/", entry.value)
        } else {
            entry.value.clone()
        },
        kind: entry.kind,
        description: entry.description.clone(),
        span: context.span.clone(),
        noquote: false,
        nospace: entry.kind == Kind::Directory
            || (entry.kind == Kind::Option && entry.value.ends_with('=')),
        matches,
        display: None,
    }
}

fn ranked<'a>(
    entries: impl Iterator<Item = &'a Entry>,
    word: &str,
    fuzzy: bool,
    nocase: bool,
) -> Vec<(matching::Score, &'a Entry, Vec<usize>)> {
    let mut ranked: Vec<_> = entries
        .filter_map(|entry| {
            matching::rank(&entry.value, word, fuzzy, nocase)
                .map(|(score, indices)| (score, entry, indices))
        })
        .collect();
    if fuzzy {
        ranked.sort_by(|left, right| (&left.0, &left.1.value).cmp(&(&right.0, &right.1.value)));
    }
    ranked
}

pub(crate) fn select(
    query: Query,
    context: &Context,
    set: &Set,
    source: Source,
    fuzzy: bool,
    nocase: bool,
) -> Answer {
    let ranked = ranked(set.entries.iter(), &context.word, fuzzy, nocase);
    let reason = set
        .reason
        .clone()
        .or_else(|| (ranked.len() > MAX_RESULTS).then(|| "completion result limit reached".into()));
    let candidates = ranked
        .into_iter()
        .take(MAX_RESULTS)
        .map(|(_, entry, indices)| candidate(entry, &source, context, indices))
        .collect();
    Answer {
        query,
        candidates,
        state: reason.map_or(State::Complete, State::Partial),
    }
}

pub(crate) fn paths(
    query: Query,
    context: &Context,
    snapshot: &NativeSnapshot,
    cache: &mut Cache,
    directories: bool,
) -> Answer {
    let word = &context.word;
    let split = word.rfind('/').map_or(0, |position| position + 1);
    let head = &word[..split];
    let component = &word[split..];
    let expand_home = context.quote.is_none() && head.starts_with("~/");
    let directory = if expand_home && let Some(rest) = head.strip_prefix("~/") {
        snapshot
            .context
            .home
            .as_ref()
            .map(|home| Path::new(home).join(rest))
    } else {
        Some(snapshot.context.cwd.join(head))
    };
    let Some(directory) = directory else {
        return Answer::unavailable(query, "dynamic path needs an isolated expansion query");
    };
    let key = format!(
        "path\0{}\0{directories}\0{}",
        directory.display(),
        component.starts_with('.')
    );
    let set = cache.get(&key, Duration::from_secs(1)).unwrap_or_else(|| {
        let set = match fs::read_dir(&directory) {
            Ok(entries) => Set::collect(entries.take(65_537).enumerate().filter_map(
                |(visited, entry)| {
                    if visited == 65_536 {
                        return Some(Err("directory enumeration limit reached".into()));
                    }
                    directory_entry(entry, component.starts_with('.'), directories).transpose()
                },
            )),
            Err(error) => Set {
                entries: Vec::new(),
                reason: Some(format!("{}: {error}", directory.display())),
            },
        };
        cache.insert(key, set)
    });
    let component_context = Context {
        word: component.into(),
        ..context.clone()
    };
    let mut answer = select(
        query,
        &component_context,
        &set,
        Source::Path,
        true,
        snapshot.nocase_paths,
    );
    let prefix_graphemes = unicode_segmentation::UnicodeSegmentation::graphemes(head, true).count();
    for candidate in &mut answer.candidates {
        let relative = format!("{head}{}", candidate.value);
        if expand_home {
            candidate.value = directory
                .join(&candidate.value)
                .to_string_lossy()
                .into_owned();
            candidate.display = Some(relative);
        } else {
            candidate.value = relative;
        }
        for position in &mut candidate.matches {
            *position += prefix_graphemes;
        }
    }
    answer
}

fn directory_entry(
    entry: std::io::Result<fs::DirEntry>,
    hidden: bool,
    directories: bool,
) -> Result<Option<Entry>, String> {
    let entry = entry.map_err(|error| error.to_string())?;
    let name = entry
        .file_name()
        .into_string()
        .map_err(|_| "non-UTF-8 filename")?;
    if !hidden && name.starts_with('.') {
        return Ok(None);
    }
    let kind = entry.file_type().map_err(|error| error.to_string())?;
    let directory = if kind.is_symlink() {
        match fs::metadata(entry.path()) {
            Ok(metadata) => metadata.is_dir(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => false,
            Err(error) => return Err(error.to_string()),
        }
    } else {
        kind.is_dir()
    };
    Ok((!directories || directory).then_some(Entry {
        value: name,
        kind: if directory {
            Kind::Directory
        } else {
            Kind::File
        },
        description: None,
    }))
}

fn known(snapshot: &NativeSnapshot, name: &str) -> Option<&'static str> {
    if snapshot.context.aliases.contains(name) {
        Some("alias")
    } else if snapshot.context.functions.contains(name) {
        Some("function")
    } else if snapshot.context.builtins.contains(name) {
        Some("builtin")
    } else {
        None
    }
}

fn abbreviations(answer: &mut Answer, context: &Context, snapshot: &NativeSnapshot) {
    let count = answer.candidates.len();
    for name in &snapshot.abbreviations.applicable {
        let Some(definition) = snapshot.abbreviations.definitions.get(name) else {
            continue;
        };
        if !name.starts_with(&context.word) {
            continue;
        }
        if name.len() > MAX_WORD || definition.expansion.len() > MAX_WORD {
            answer.state = State::Partial("abbreviation candidate limit reached".into());
            continue;
        }
        answer.candidates.push(Candidate {
            source: Source::Abbreviation {
                name: name.clone(),
                revision: snapshot.abbreviations.revision,
            },
            value: definition.expansion.clone(),
            kind: Kind::Abbreviation,
            description: Some(format!("{} [{}]", definition.expansion, definition.source)),
            span: context.span.clone(),
            noquote: true,
            nospace: definition.expansion.ends_with(char::is_whitespace),
            matches: matching::rank(name, &context.word, false, false)
                .map_or_else(Vec::new, |(_, indices)| indices),
            display: Some(name.clone()),
        });
    }
    if answer.candidates.len() == count {
        return;
    }
    answer.candidates.sort_by_cached_key(|candidate| {
        let name = candidate.display.as_deref().unwrap_or(&candidate.value);
        (
            matching::rank(name, &context.word, true, false).map(|(score, _)| score),
            name.to_owned(),
            matches!(candidate.source, Source::Command),
        )
    });
    if answer.candidates.len() > MAX_RESULTS {
        answer.candidates.truncate(MAX_RESULTS);
        answer.state = State::Partial("combined completion result limit reached".into());
    }
}

pub(crate) fn executable(
    snapshot: &NativeSnapshot,
    context: &Context,
    name: &str,
) -> Result<bool, String> {
    Ok(known(snapshot, name).is_some() || resolve(snapshot, context, name)?.is_some())
}

pub(super) fn resolve(
    snapshot: &NativeSnapshot,
    context: &Context,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    use std::os::unix::ffi::OsStrExt;
    let fixed = if name.contains('/') {
        Some(Path::new(name))
    } else if context.path == snapshot.context.path {
        snapshot
            .context
            .hashed_commands
            .get(name)
            .map(PathBuf::as_path)
    } else {
        None
    };
    let paths = fixed
        .into_iter()
        .map(|path| snapshot.context.cwd.join(path))
        .chain(
            context
                .path
                .as_deref()
                .filter(|_| fixed.is_none() || (snapshot.context.check_hash && !name.contains('/')))
                .into_iter()
                .flat_map(|path| path.split(':'))
                .take(128)
                .map(|directory| snapshot.context.cwd.join(directory).join(name)),
        );
    for path in paths {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                let name = std::ffi::CString::new(path.as_os_str().as_bytes())
                    .map_err(|error| error.to_string())?;
                // SAFETY: the path is NUL-terminated and access only queries this candidate.
                if unsafe { libc::access(name.as_ptr(), libc::X_OK) } == 0 {
                    return Ok(Some(path));
                }
                let error = std::io::Error::last_os_error();
                if !matches!(
                    error.raw_os_error(),
                    Some(libc::EACCES | libc::ENOENT | libc::ENOTDIR)
                ) {
                    return Err(error.to_string());
                }
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(None)
}

pub(crate) fn commands(
    query: Query,
    context: &Context,
    snapshot: &NativeSnapshot,
    cache: &mut Cache,
    progress: &mut impl FnMut(Outcome) -> std::io::Result<()>,
) -> std::io::Result<Answer> {
    let names: BTreeSet<_> = snapshot
        .context
        .builtins
        .iter()
        .chain(&snapshot.context.aliases)
        .chain(&snapshot.context.functions)
        .collect();
    let local = Set {
        entries: names
            .into_iter()
            .map(|value| {
                let description = known(snapshot, value).map(str::to_owned);
                Entry {
                    value: value.clone(),
                    kind: Kind::Command,
                    description,
                }
            })
            .collect(),
        reason: Some("querying command index".into()),
    };
    let mut early_answer = select(query.clone(), context, &local, Source::Command, true, false);
    abbreviations(&mut early_answer, context, snapshot);
    progress(Outcome::Progress(early_answer))?;
    let key = format!(
        "commands\0{}\0{:?}",
        snapshot.context.cwd.display(),
        context.path
    );
    let indexed = cache.get(&key, Duration::from_secs(5)).unwrap_or_else(|| {
        let index = crate::input_assist::scan_index(&snapshot.context.cwd, context.path.as_deref());
        let mut set = Set::lines(
            index.names.iter().map(String::as_str),
            Kind::Command,
            Some("executable"),
        );
        set.reason = index.reason.or(set.reason);
        cache.insert(key, set)
    });
    let hashed: Vec<_> = snapshot
        .context
        .hashed_commands
        .keys()
        .filter(|_| context.path == snapshot.context.path)
        .map(|name| Entry {
            value: name.clone(),
            kind: Kind::Command,
            description: Some("hashed executable".into()),
        })
        .collect();
    let mut seen = BTreeSet::new();
    let entries = local
        .entries
        .iter()
        .chain(&indexed.entries)
        .chain(&hashed)
        .filter(|entry| seen.insert(entry.value.as_str()));
    let mut answer = Answer {
        query,
        candidates: Vec::new(),
        state: indexed
            .reason
            .clone()
            .map_or(State::Complete, State::Partial),
    };
    for (_, entry, indices) in ranked(entries, &context.word, true, false) {
        if answer.candidates.len() == MAX_RESULTS {
            if answer.state.is_complete() {
                answer.state = State::Partial("command verification/result limit reached".into());
            }
            break;
        }
        match executable(snapshot, context, &entry.value) {
            Ok(true) => {
                answer
                    .candidates
                    .push(candidate(entry, &Source::Command, context, indices))
            }
            Ok(false) => {}
            Err(error) if answer.state.is_complete() => answer.state = State::Partial(error),
            Err(_) => {}
        }
    }
    abbreviations(&mut answer, context, snapshot);
    Ok(answer)
}
