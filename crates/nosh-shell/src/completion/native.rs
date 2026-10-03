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
        filenames: matches!(entry.kind, Kind::File | Kind::Directory),
        noquote: false,
        nospace: entry.kind == Kind::Directory || entry.value.ends_with('='),
        matches,
        display: None,
    }
}

pub(crate) fn select(
    query: Query,
    context: &Context,
    set: &Set,
    source: Source,
    fuzzy: bool,
    nocase: bool,
) -> Answer {
    let mut ranked: Vec<_> = set
        .entries
        .iter()
        .filter_map(|entry| {
            matching::rank(&entry.value, &context.word, fuzzy, nocase)
                .map(|(score, indices)| (score, entry, indices))
        })
        .collect();
    if fuzzy {
        ranked.sort_by(|left, right| (&left.0, &left.1.value).cmp(&(&right.0, &right.1.value)));
    }
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
    } else if head == "~" {
        snapshot.context.home.as_ref().map(PathBuf::from)
    } else {
        Some(
            snapshot
                .context
                .cwd
                .join(if head.is_empty() { "." } else { head }),
        )
    };
    let Some(directory) = directory else {
        return Answer {
            query,
            candidates: Vec::new(),
            state: State::Unavailable("dynamic path needs an isolated expansion query".into()),
        };
    };
    let key = format!(
        "path\0{}\0{directories}\0{}",
        directory.display(),
        component.starts_with('.')
    );
    let set = cache.get(&key, Duration::from_secs(1)).unwrap_or_else(|| {
        let mut set = Set {
            entries: Vec::new(),
            reason: None,
        };
        match fs::read_dir(&directory) {
            Ok(entries) => {
                let mut bytes = 0;
                for (visited, entry) in entries.enumerate() {
                    if visited >= 65_536 || set.entries.len() >= MAX_SET || bytes >= MAX_SET_BYTES {
                        set.reason = Some("directory enumeration limit reached".into());
                        break;
                    }
                    let entry = match entry {
                        Ok(entry) => entry,
                        Err(error) => {
                            set.reason.get_or_insert_with(|| error.to_string());
                            continue;
                        }
                    };
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        set.reason
                            .get_or_insert_with(|| "non-UTF-8 filename".into());
                        continue;
                    };
                    if name.starts_with('.') && !component.starts_with('.') {
                        continue;
                    }
                    let kind = match entry.file_type() {
                        Ok(kind) if kind.is_dir() => Kind::Directory,
                        Ok(kind) if kind.is_symlink() => match fs::metadata(entry.path()) {
                            Ok(metadata) if metadata.is_dir() => Kind::Directory,
                            Ok(_) => Kind::File,
                            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                                Kind::File
                            }
                            Err(error) => {
                                set.reason.get_or_insert_with(|| error.to_string());
                                continue;
                            }
                        },
                        Ok(_) => Kind::File,
                        Err(error) => {
                            set.reason.get_or_insert_with(|| error.to_string());
                            continue;
                        }
                    };
                    if directories && kind != Kind::Directory {
                        continue;
                    }
                    let value: String = name.into();
                    if bytes + value.len() + 96 > MAX_SET_BYTES {
                        set.reason = Some("directory candidate byte limit reached".into());
                        break;
                    }
                    bytes += value.len() + 96;
                    set.entries.push(Entry {
                        value,
                        kind,
                        description: None,
                    });
                }
            }
            Err(error) => set.reason = Some(format!("{}: {error}", directory.display())),
        }
        set.entries
            .sort_by(|left, right| left.value.cmp(&right.value));
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
            filenames: false,
            noquote: true,
            nospace: definition.expansion.ends_with(char::is_whitespace),
            matches: matching::rank(name, &context.word, false, false)
                .map_or_else(Vec::new, |(_, indices)| indices),
            display: Some(name.clone()),
        });
    }
    answer.candidates.sort_by_key(|candidate| {
        let name = candidate.display.as_deref().unwrap_or(&candidate.value);
        (
            matching::rank(name, &context.word, true, false).map(|(score, _)| score),
            name.to_owned(),
            candidate.identity(),
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
    use std::os::unix::ffi::OsStrExt;
    if known(snapshot, name).is_some() {
        return Ok(true);
    }
    let mut paths = Vec::new();
    if let Some(path) = snapshot.context.hashed_commands.get(name) {
        paths.push(snapshot.context.cwd.join(path));
    } else if let Some(path) = &context.path {
        paths.extend(
            path.split(':')
                .take(128)
                .map(|directory| snapshot.context.cwd.join(directory).join(name)),
        );
    }
    for path in paths {
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                let name = std::ffi::CString::new(path.as_os_str().as_bytes())
                    .map_err(|error| error.to_string())?;
                // SAFETY: the path is NUL-terminated and access only queries this candidate.
                if unsafe { libc::access(name.as_ptr(), libc::X_OK) } == 0 {
                    return Ok(true);
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
    Ok(false)
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
        .cloned()
        .collect();
    let mut entries: Vec<_> = names
        .into_iter()
        .map(|value| {
            let description = known(snapshot, &value).map(str::to_owned);
            Entry {
                value,
                kind: Kind::Command,
                description,
            }
        })
        .collect();
    let early = Set {
        entries: entries.clone(),
        reason: Some("querying command index".into()),
    };
    let mut early_answer = select(query.clone(), context, &early, Source::Command, true, false);
    abbreviations(&mut early_answer, context, snapshot);
    progress(Outcome::Progress {
        answer: early_answer,
        budget: Budget::Index,
    })?;
    let key = format!(
        "commands\0{}\0{:?}",
        snapshot.context.cwd.display(),
        context.path
    );
    let indexed = cache.get(&key, Duration::from_secs(5)).unwrap_or_else(|| {
        let index = crate::input_assist::scan_index(&snapshot.context.cwd, context.path.as_deref());
        let entries = index
            .names
            .iter()
            .map(|value| Entry {
                value: value.clone(),
                kind: Kind::Command,
                description: Some("executable".into()),
            })
            .collect();
        cache.insert(
            key,
            Set {
                entries,
                reason: index.reason,
            },
        )
    });
    let known_names: BTreeSet<_> = entries.iter().map(|entry| entry.value.as_str()).collect();
    let mut extra: Vec<_> = indexed
        .entries
        .iter()
        .filter(|entry| !known_names.contains(entry.value.as_str()))
        .filter_map(|entry| {
            matching::rank(&entry.value, &context.word, true, false)
                .map(|(score, indices)| (score, entry, indices))
        })
        .collect();
    extra.sort_by(|left, right| (&left.0, &left.1.value).cmp(&(&right.0, &right.1.value)));
    let mut reason = indexed.reason.clone();
    let mut verified = 0;
    for (_, entry, _) in extra {
        if verified >= MAX_RESULTS {
            reason.get_or_insert_with(|| "command verification/result limit reached".into());
            break;
        }
        match executable(snapshot, context, &entry.value) {
            Ok(true) => {
                entries.push(entry.clone());
                verified += 1;
            }
            Ok(false) => {}
            Err(error) => {
                reason.get_or_insert(error);
            }
        }
    }
    for name in snapshot.context.hashed_commands.keys() {
        if !entries.iter().any(|entry| &entry.value == name)
            && matching::rank(name, &context.word, true, false).is_some()
        {
            match executable(snapshot, context, name) {
                Ok(true) => entries.push(Entry {
                    value: name.clone(),
                    kind: Kind::Command,
                    description: Some("hashed executable".into()),
                }),
                Ok(false) => {}
                Err(error) => {
                    reason.get_or_insert(error);
                }
            }
        }
    }
    let mut answer = select(
        query,
        context,
        &Set { entries, reason },
        Source::Command,
        true,
        false,
    );
    abbreviations(&mut answer, context, snapshot);
    Ok(answer)
}
