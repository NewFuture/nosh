use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;
use crate::completion::native;

const OPTIONS: &[(&str, &str)] = &[
    ("--file=", "Read a specified makefile"),
    ("--directory=", "Change the working directory"),
    (
        "--include-dir=",
        "Search this directory for included makefiles",
    ),
    ("--jobs=", "Limit parallel jobs"),
    ("--dry-run", "Print recipes without executing them"),
    ("--keep-going", "Continue independent work after an error"),
    ("--silent", "Do not echo recipes"),
    ("--always-make", "Consider every target out of date"),
    ("--no-builtin-rules", "Disable built-in implicit rules"),
    (
        "--no-builtin-variables",
        "Disable built-in variable definitions",
    ),
    ("--question", "Query whether targets are up to date"),
    ("--touch", "Touch targets instead of rebuilding"),
    ("--version", "Print the GNU Make version"),
];

pub(super) fn generate(
    query: Query,
    context: &Context,
    snapshot: &NativeSnapshot,
    cache: &mut Cache,
) -> Result<Answer, String> {
    let (_, version) = version(context, snapshot, cache)?;
    if !version.starts_with("GNU Make ") {
        return Err("built-in target completion supports GNU Make; load a definition for this implementation".into());
    }
    let previous = context
        .index
        .checked_sub(1)
        .and_then(|index| context.words.get(index))
        .map(String::as_str);
    if matches!(
        previous,
        Some("-C" | "--directory" | "-I" | "--include-dir" | "-f" | "--file" | "--makefile")
    ) || ["--directory=", "--include-dir=", "--file=", "--makefile="]
        .iter()
        .any(|prefix| context.word.starts_with(prefix))
    {
        let directories = matches!(
            previous,
            Some("-C" | "--directory" | "-I" | "--include-dir")
        ) || context.word.starts_with("--directory=")
            || context.word.starts_with("--include-dir=");
        return Ok(native::paths(
            query.clone(),
            &value_context(context, &query),
            snapshot,
            cache,
            directories,
        ));
    }
    if matches!(previous, Some("-j" | "--jobs")) || context.word.starts_with("--jobs=") {
        return Ok(Answer::unavailable(
            query,
            "jobs accepts an integer; no enumerated values",
        ));
    }
    let prior = &context.words[1..context.index];
    if context.word.starts_with('-') && !prior.iter().any(|word| word == "--") {
        return Ok(native::select(
            query,
            context,
            &entries(OPTIONS, Kind::Option),
            Source::Make,
            false,
            false,
        ));
    }
    if context.word.contains('=') {
        return Ok(Answer::unavailable(
            query,
            "Make variable values are not inferred",
        ));
    }
    let mut cwd = snapshot.context.cwd.clone();
    let mut files = Vec::new();
    let mut includes = Vec::new();
    let mut index = 0;
    while index < prior.len() {
        let word = &prior[index];
        let (option, value) = if let Some((option, value)) = word.split_once('=') {
            (option, Some(value))
        } else if matches!(
            word.as_str(),
            "-C" | "--directory" | "-f" | "--file" | "--makefile" | "-I" | "--include-dir"
        ) {
            index += 1;
            (word.as_str(), prior.get(index).map(String::as_str))
        } else {
            (word.as_str(), None)
        };
        if let Some(value) = value {
            if value.contains(['$', '`']) {
                return Err("dynamic Make paths need a loaded completion definition".into());
            }
            match option {
                "-C" | "--directory" => cwd = cwd.join(value),
                "-f" | "--file" | "--makefile" => files.push(PathBuf::from(value)),
                "-I" | "--include-dir" => includes.push(PathBuf::from(value)),
                _ => {}
            }
        }
        index += 1;
    }
    if files.is_empty() {
        for name in ["GNUmakefile", "makefile", "Makefile"] {
            if cwd.join(name).is_file() {
                files.push(name.into());
                break;
            }
        }
    }
    if let Some(extra) = snapshot.environment.get("MAKEFILES") {
        if extra.contains(['$', '\\']) {
            return Err("dynamic MAKEFILES is unsupported by static target completion".into());
        }
        files.extend(extra.split_whitespace().map(PathBuf::from));
    }
    let key = format!("make-targets\0{:?}\0{:?}\0{:?}", cwd, files, includes);
    let set = if let Some(set) = cache.get(&key, Duration::from_secs(1)) {
        set
    } else {
        cache.insert(key, targets(&cwd, &files, &includes))
    };
    Ok(native::select(
        query,
        context,
        &set,
        Source::Make,
        false,
        false,
    ))
}

fn words(text: &str) -> Result<Vec<String>, &'static str> {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut escaped = false;
    for character in text.chars() {
        if escaped {
            word.push(character);
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character.is_whitespace() {
            if !word.is_empty() {
                result.push(std::mem::take(&mut word));
            }
        } else if character == '#' {
            break;
        } else if character == '$' {
            return Err("dynamic Make expression omitted");
        } else {
            word.push(character);
        }
    }
    if escaped {
        return Err("incomplete Make escape omitted");
    }
    if !word.is_empty() {
        result.push(word);
    }
    Ok(result)
}

fn delimiter(text: &str, wanted: char) -> Option<usize> {
    let mut escaped = false;
    for (offset, character) in text.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' {
            escaped = true;
        } else if character == wanted {
            return Some(offset);
        }
    }
    None
}

pub(crate) fn targets(cwd: &Path, files: &[PathBuf], include_dirs: &[PathBuf]) -> Set {
    let mut queue: VecDeque<_> = files
        .iter()
        .map(|file| (cwd.join(file), 0, false))
        .collect();
    let mut seen = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut reason = None;
    let mut bytes = 0;
    while let Some((path, depth, optional)) = queue.pop_front() {
        if depth > 8 || seen.len() >= 32 {
            reason.get_or_insert_with(|| "Make include budget reached".into());
            break;
        }
        let canonical = match fs::canonicalize(&path) {
            Ok(path) => path,
            Err(error) if optional && error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                reason.get_or_insert_with(|| format!("{}: {error}", path.display()));
                continue;
            }
        };
        if !seen.insert(canonical.clone()) {
            reason.get_or_insert_with(|| "Make include cycle or repeated include omitted".into());
            continue;
        }
        let file = match fs::File::open(&canonical) {
            Ok(file) => file,
            Err(error) => {
                reason.get_or_insert_with(|| error.to_string());
                continue;
            }
        };
        let mut data = Vec::new();
        if let Err(error) = file
            .take((2 * 1024 * 1024 - bytes + 1) as u64)
            .read_to_end(&mut data)
        {
            reason.get_or_insert_with(|| error.to_string());
            continue;
        }
        bytes += data.len();
        if bytes > 2 * 1024 * 1024 {
            reason.get_or_insert_with(|| "Make file byte budget reached".into());
            break;
        }
        let text = match String::from_utf8(data) {
            Ok(text) => text,
            Err(_) => {
                reason.get_or_insert_with(|| "non-UTF-8 Makefile omitted".into());
                continue;
            }
        };
        let mut definition = 0_usize;
        let mut conditional = 0_usize;
        let mut recipe = '\t';
        let mut logical = String::new();
        for line in text.lines() {
            if line.starts_with(recipe) {
                continue;
            }
            logical.push_str(line);
            if logical.ends_with('\\') {
                logical.pop();
                logical.push(' ');
                continue;
            }
            let line = std::mem::take(&mut logical);
            let trimmed = line.trim_start();
            let trimmed = &trimmed[..delimiter(trimmed, '#').unwrap_or(trimmed.len())];
            let directive = trimmed.split_whitespace().next().unwrap_or("");
            if directive == "define" {
                definition += 1;
                continue;
            }
            if directive == "endef" {
                definition = definition.saturating_sub(1);
                continue;
            }
            if definition > 0 {
                continue;
            }
            if ["ifeq", "ifneq", "ifdef", "ifndef"].iter().any(|prefix| {
                trimmed
                    .strip_prefix(prefix)
                    .is_some_and(|rest| rest.starts_with([' ', '\t', '(']))
            }) {
                conditional += 1;
                reason.get_or_insert_with(|| "conditional Make targets omitted".into());
                continue;
            }
            if directive == "endif" {
                conditional = conditional.saturating_sub(1);
                continue;
            }
            if conditional > 0 || directive == "else" {
                continue;
            }
            if trimmed
                .strip_prefix(".RECIPEPREFIX")
                .is_some_and(|rest| rest.starts_with([' ', '\t', ':', '=']))
            {
                if let Some((_, value)) = trimmed.split_once('=') {
                    if value.contains('$') {
                        reason.get_or_insert_with(|| "dynamic recipe prefix omitted".into());
                        break;
                    }
                    recipe = value.trim_start().chars().next().unwrap_or('\t');
                }
                continue;
            }
            let include = matches!(directive, "include" | "-include" | "sinclude")
                .then(|| (&trimmed[directive.len()..], directive != "include"));
            if let Some((rest, optional)) = include {
                match words(rest) {
                    Ok(files) => {
                        for file in files {
                            if file.contains(['*', '?', '[']) {
                                reason.get_or_insert_with(|| {
                                    "dynamic include pattern omitted".into()
                                });
                                continue;
                            }
                            let direct = cwd.join(&file);
                            let path = if direct.exists() {
                                direct
                            } else {
                                include_dirs
                                    .iter()
                                    .map(|directory| cwd.join(directory).join(&file))
                                    .find(|path| path.exists())
                                    .unwrap_or(direct)
                            };
                            if queue.len() + seen.len() >= 32 {
                                reason.get_or_insert_with(|| {
                                    "Make include queue budget reached".into()
                                });
                                break;
                            }
                            queue.push_back((path, depth + 1, optional));
                        }
                    }
                    Err(error) => {
                        reason.get_or_insert_with(|| error.into());
                    }
                }
                continue;
            }
            let Some(colon) = delimiter(trimmed, ':') else {
                continue;
            };
            let (left, right) = (&trimmed[..colon], &trimmed[colon + 1..]);
            if left.contains('=')
                || right.starts_with('=')
                || right.starts_with(":=")
                || right.starts_with("::=")
            {
                continue;
            }
            let targets = if left.trim() == ".PHONY" {
                &right[..delimiter(right, ';').unwrap_or(right.len())]
            } else {
                left.trim_end_matches('&')
            };
            match words(targets) {
                Ok(targets) => {
                    for target in targets {
                        if target.contains('%') {
                            reason.get_or_insert_with(|| {
                                "pattern-generated Make targets omitted".into()
                            });
                            continue;
                        }
                        if target.starts_with('.') {
                            continue;
                        }
                        if target.contains(['(', ')']) {
                            reason.get_or_insert_with(|| "archive Make target omitted".into());
                            continue;
                        }
                        names.insert(target);
                        if names.len() > MAX_SET {
                            reason.get_or_insert_with(|| "Make target limit reached".into());
                            break;
                        }
                    }
                }
                Err(error) => {
                    reason.get_or_insert_with(|| error.into());
                }
            }
        }
        if !logical.is_empty() || definition > 0 || conditional != 0 {
            reason.get_or_insert_with(|| "incomplete Make structure omitted".into());
        }
    }
    let entries = names
        .into_iter()
        .take(MAX_SET)
        .map(|value| Entry {
            value,
            kind: Kind::Target,
            description: Some("static Make target".into()),
        })
        .collect();
    Set { entries, reason }
}
