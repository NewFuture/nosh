use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use super::*;

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
    let mut arguments = context.words[1..context.index]
        .iter()
        .chain(std::iter::once(&context.word));
    while let Some(word) = arguments.next() {
        match word.as_str() {
            "--" => break,
            "-C" | "--directory" | "-f" | "--file" | "--makefile" | "-I" | "--include-dir" => {
                arguments.next();
            }
            _ if word.len() > 2 && matches!(word.get(..2), Some("-C" | "-f" | "-I")) => {
                return Ok(Answer::unavailable(
                    query,
                    "attached Make path options require a loaded completion definition",
                ));
            }
            _ => {}
        }
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
        return Ok(paths(query, context, snapshot, cache, directories));
    }
    if matches!(previous, Some("-j" | "--jobs")) || context.word.starts_with("--jobs=") {
        return Ok(Answer::unavailable(
            query,
            "jobs accepts an integer; no enumerated values",
        ));
    }
    let prior = &context.words[1..context.index];
    if context.word.starts_with('-') && !prior.iter().any(|word| word == "--") {
        return Ok(select(
            query,
            context,
            &entries(OPTIONS, Kind::Option),
            Source::Make,
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
    let mut flags_error = None;
    if let Some(flags) = snapshot.environment.get("MAKEFLAGS") {
        match include_flags(flags) {
            Ok(paths) => includes = paths,
            Err(error) => flags_error = Some(error),
        }
    }
    let mut arguments = prior.iter();
    while let Some(word) = arguments.next() {
        if word == "--" {
            break;
        }
        let (option, value) = if let Some((option, value)) = word.split_once('=') {
            (option, Some(value))
        } else if matches!(
            word.as_str(),
            "-C" | "--directory" | "-f" | "--file" | "--makefile" | "-I" | "--include-dir"
        ) {
            (word.as_str(), arguments.next().map(String::as_str))
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
    let set = cache.load(key, Duration::from_secs(1), || {
        Ok(targets(&cwd, &files, &includes))
    })?;
    let mut answer = select(query, context, &set, Source::Make);
    if let Some(error) = flags_error {
        answer.state = State::Partial(error.into());
    }
    Ok(answer)
}

fn include_flags(text: &str) -> Result<Vec<PathBuf>, &'static str> {
    let mut flags = words(text)?;
    if let Some(first) = flags.first_mut()
        && !first.starts_with('-')
        && !first.contains('=')
    {
        first.insert(0, '-');
    }
    let mut flags = flags.iter();
    let mut directories = Vec::new();
    while let Some(flag) = flags.next() {
        if flag == "--" {
            break;
        }
        if matches!(
            flag.as_str(),
            "-C" | "-f" | "--directory" | "--file" | "--makefile"
        ) {
            flags.next();
            continue;
        }
        let value = if matches!(flag.as_str(), "-I" | "--include-dir") {
            Some(
                flags
                    .next()
                    .ok_or("missing MAKEFLAGS include directory")?
                    .as_str(),
            )
        } else {
            flag.strip_prefix("--include-dir=")
                .or_else(|| flag.strip_prefix("-I"))
        };
        if let Some(value) = value {
            if value.is_empty() {
                return Err("empty MAKEFLAGS include directory");
            }
            if value != "-" {
                directories.push(PathBuf::from(value));
            }
        } else if flag.starts_with('-') && !flag.starts_with("--") && flag.contains('I') {
            return Err("combined MAKEFLAGS include flags are not supported");
        }
    }
    Ok(directories)
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
        let mut data = Vec::new();
        let read = fs::File::open(&canonical).and_then(|file| {
            file.take((2 * 1024 * 1024 - bytes + 1) as u64)
                .read_to_end(&mut data)
        });
        bytes += data.len();
        if bytes > 2 * 1024 * 1024 {
            reason.get_or_insert_with(|| "Make file byte budget reached".into());
            break;
        }
        let text = match read
            .map_err(|error| error.to_string())
            .and_then(|_| String::from_utf8(data).map_err(|_| "non-UTF-8 Makefile omitted".into()))
        {
            Ok(text) => text,
            Err(error) => {
                reason.get_or_insert(error);
                continue;
            }
        };
        let mut definition = 0_usize;
        let mut conditional = 0_usize;
        let mut recipe = '\t';
        let mut recipe_continued = false;
        let mut logical = String::new();
        for line in text.lines() {
            if recipe_continued || line.starts_with(recipe) {
                recipe_continued =
                    line.bytes().rev().take_while(|byte| *byte == b'\\').count() % 2 == 1;
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
            let include = matches!(directive, "include" | "-include" | "sinclude");
            let values = if include {
                &trimmed[directive.len()..]
            } else {
                let Some(colon) = delimiter(trimmed, ':') else {
                    continue;
                };
                let (left, right) = (&trimmed[..colon], &trimmed[colon + 1..]);
                if left.contains('=') || right.trim_start_matches(':').starts_with('=') {
                    continue;
                }
                if left.trim() == ".PHONY" {
                    &right[..delimiter(right, ';').unwrap_or(right.len())]
                } else {
                    left.trim_end_matches('&')
                }
            };
            let values = match words(values) {
                Ok(values) => values,
                Err(error) => {
                    reason.get_or_insert_with(|| error.into());
                    continue;
                }
            };
            for value in values {
                if include {
                    if value.contains(['*', '?', '[']) {
                        reason.get_or_insert_with(|| "dynamic include pattern omitted".into());
                        continue;
                    }
                    if queue.len() + seen.len() >= 32 {
                        reason.get_or_insert_with(|| "Make include queue budget reached".into());
                        break;
                    }
                    let path = std::iter::once(cwd.to_path_buf())
                        .chain(include_dirs.iter().map(|directory| cwd.join(directory)))
                        .map(|directory| directory.join(&value))
                        .find(|path| path.exists())
                        .unwrap_or_else(|| cwd.join(value));
                    queue.push_back((path, depth + 1, directive != "include"));
                } else if value.contains('%') {
                    reason.get_or_insert_with(|| "pattern-generated Make targets omitted".into());
                } else if value.starts_with('.') {
                    continue;
                } else if value.contains(['(', ')']) {
                    reason.get_or_insert_with(|| "archive Make target omitted".into());
                } else {
                    names.insert(value);
                    if names.len() > MAX_SET {
                        reason.get_or_insert_with(|| "Make target limit reached".into());
                        break;
                    }
                }
            }
        }
        if !logical.is_empty() || definition > 0 || conditional != 0 {
            reason.get_or_insert_with(|| "incomplete Make structure omitted".into());
        }
    }
    let mut set = Set::lines(
        names.iter().map(String::as_str),
        Kind::Target,
        Some("static Make target"),
    );
    set.reason = reason.or(set.reason);
    set
}
