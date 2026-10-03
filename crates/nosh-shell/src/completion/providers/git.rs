use std::time::Duration;

use super::*;
use crate::completion::native;

const SWITCH_OPTIONS: &[(&str, &str)] = &[
    ("--create", "Create a new branch"),
    ("--force-create", "Create or reset a branch"),
    ("--detach", "Detach HEAD at a starting point"),
    ("--guess", "Guess an unambiguous remote branch"),
    ("--no-guess", "Do not guess a remote branch"),
    (
        "--discard-changes",
        "Discard conflicting working tree changes",
    ),
    ("--merge", "Merge local changes while switching"),
    ("--conflict=", "Choose conflict presentation"),
    ("--quiet", "Suppress feedback"),
    ("--progress", "Report progress"),
    ("--no-progress", "Suppress progress"),
    ("--track", "Set upstream tracking"),
    ("--no-track", "Do not set upstream tracking"),
    ("--orphan", "Create a new orphan branch"),
    (
        "--ignore-other-worktrees",
        "Allow a branch used by another worktree",
    ),
    ("--recurse-submodules", "Update submodules"),
    ("--no-recurse-submodules", "Do not update submodules"),
];

pub(super) fn generate(
    query: Query,
    context: &Context,
    snapshot: &NativeSnapshot,
    cache: &mut Cache,
) -> Result<Answer, String> {
    let (executable, version) = version(context, snapshot, cache)?;
    let number = version
        .trim()
        .strip_prefix("git version ")
        .ok_or("unsupported Git implementation")?;
    let numbers: Vec<_> = number
        .split('.')
        .take(2)
        .map(str::parse::<u32>)
        .collect::<Result<_, _>>()
        .map_err(|_| "cannot confirm Git version")?;
    if numbers.len() != 2 || (numbers[0], numbers[1]) < (2, 23) {
        return Err("built-in switch completion requires Git 2.23 or newer".into());
    }
    let mut prefix = vec!["--no-pager".into()];
    let mut command_index = 1;
    while command_index < context.index {
        let word = &context.words[command_index];
        if ["-C", "-c", "--git-dir", "--work-tree", "--namespace"].contains(&word.as_str()) {
            let value = context
                .words
                .get(command_index + 1)
                .ok_or("missing Git global option value")?;
            if value.contains(['$', '`']) {
                return Err(
                    "dynamic Git global option needs a loaded completion definition".into(),
                );
            }
            prefix.extend([word.clone(), value.clone()]);
            command_index += 2;
        } else if word.starts_with("--git-dir=")
            || word.starts_with("--work-tree=")
            || word.starts_with("--namespace=")
            || matches!(
                word.as_str(),
                "--bare" | "--no-optional-locks" | "--no-replace-objects"
            )
        {
            prefix.push(word.clone());
            command_index += 1;
        } else {
            break;
        }
    }
    let previous = context
        .index
        .checked_sub(1)
        .and_then(|index| context.words.get(index))
        .map(String::as_str);
    if matches!(previous, Some("-C" | "--git-dir" | "--work-tree"))
        || context.word.starts_with("--git-dir=")
        || context.word.starts_with("--work-tree=")
    {
        return Ok(native::paths(
            query.clone(),
            &value_context(context, &query),
            snapshot,
            cache,
            true,
        ));
    }
    if context.index == command_index {
        let key = format!(
            "git-commands\0{}\0{:?}\0{:?}",
            executable.display(),
            snapshot.context.cwd,
            prefix
        );
        let set = if let Some(set) = cache.get(&key, Duration::from_secs(1)) {
            set
        } else {
            let mut args = prefix.clone();
            args.push("--list-cmds=main,others,nohelpers,alias".into());
            let data = output(
                context,
                snapshot,
                &args,
                &snapshot.context.cwd,
                MAX_SET_BYTES,
            )?;
            let mut entries: Vec<_> = data
                .lines()
                .map(|value| Entry {
                    value: value.into(),
                    kind: Kind::Subcommand,
                    description: None,
                })
                .collect();
            entries.sort_by(|left, right| left.value.cmp(&right.value));
            entries.dedup_by(|left, right| left.value == right.value);
            let reason = (entries.len() > MAX_SET).then(|| "Git command limit reached".into());
            entries.truncate(MAX_SET);
            cache.insert(key, Set { entries, reason })
        };
        return Ok(native::select(
            query,
            context,
            &set,
            Source::Git,
            false,
            false,
        ));
    }
    if context.words.get(command_index).map(String::as_str) != Some("switch") {
        return Ok(Answer { query, candidates: Vec::new(), state: State::Unavailable(
            "built-in Git parameter completion currently covers switch; load a definition for this subcommand".into()) });
    }
    if previous == Some("--conflict") || context.word.starts_with("--conflict=") {
        let mut styles = vec![
            ("merge", "Show two-way conflict markers"),
            ("diff3", "Also show the original contents"),
        ];
        if (numbers[0], numbers[1]) >= (2, 35) {
            styles.push(("zdiff3", "Show compact three-way conflict markers"));
        }
        return Ok(native::select(
            query.clone(),
            &value_context(context, &query),
            &entries(&styles, Kind::Value),
            Source::Git,
            false,
            false,
        ));
    }
    if matches!(
        previous,
        Some("-c" | "-C" | "--create" | "--force-create" | "--orphan")
    ) {
        return Ok(Answer {
            query,
            candidates: Vec::new(),
            state: State::Unavailable(
                "this option takes a new branch name, not an existing path or branch".into(),
            ),
        });
    }
    let after_double_dash = context.words[command_index + 1..context.index]
        .iter()
        .any(|word| word == "--");
    if context.word.starts_with('-') && !after_double_dash {
        let mut options = entries(SWITCH_OPTIONS, Kind::Option);
        let prior = &context.words[command_index + 1..context.index];
        let branch_mode = prior.iter().any(|word| {
            matches!(
                word.as_str(),
                "-c" | "-C" | "-d" | "--create" | "--force-create" | "--detach" | "--orphan"
            ) || ["--create=", "--force-create=", "--orphan="]
                .iter()
                .any(|prefix| word.starts_with(prefix))
        });
        options.entries.retain(|entry| {
            let name = entry.value.trim_end_matches('=');
            !(branch_mode
                && matches!(
                    name,
                    "--create" | "--force-create" | "--detach" | "--orphan"
                ))
                && !prior.iter().any(|word| {
                    word == name
                        || word
                            .strip_prefix(name)
                            .is_some_and(|rest| rest.starts_with('='))
                })
        });
        return Ok(native::select(
            query,
            context,
            &options,
            Source::Git,
            false,
            false,
        ));
    }
    let remote = context.words[command_index + 1..context.index]
        .iter()
        .any(|word| matches!(word.as_str(), "--track" | "-t"));
    let start_point = remote
        || context.words[command_index + 1..context.index]
            .iter()
            .any(|word| {
                matches!(
                    word.as_str(),
                    "-c" | "-C" | "--create" | "--force-create" | "--detach" | "-d"
                )
            });
    let key = format!(
        "git-refs\0{}\0{:?}\0{:?}\0{remote}\0{start_point}",
        executable.display(),
        snapshot.context.cwd,
        prefix
    );
    let set = if let Some(set) = cache.get(&key, Duration::from_secs(1)) {
        set
    } else {
        let mut args = prefix;
        args.extend(["for-each-ref".into(), "--format=%(refname:strip=2)".into()]);
        if remote {
            args.push("refs/remotes".into());
        } else {
            args.push("refs/heads".into());
            if start_point {
                args.extend(["refs/tags".into(), "refs/remotes".into()]);
            }
        }
        let data = output(
            context,
            snapshot,
            &args,
            &snapshot.context.cwd,
            MAX_SET_BYTES,
        )?;
        let mut values: Vec<_> = data
            .lines()
            .filter(|name| !name.ends_with("/HEAD"))
            .map(|name| Entry {
                value: name.into(),
                kind: Kind::Branch,
                description: Some("Git ref".into()),
            })
            .collect();
        values.sort_by(|left, right| left.value.cmp(&right.value));
        values.dedup_by(|left, right| left.value == right.value);
        let reason = (values.len() > MAX_SET).then(|| "Git ref limit reached".into());
        values.truncate(MAX_SET);
        cache.insert(
            key,
            Set {
                entries: values,
                reason,
            },
        )
    };
    Ok(native::select(
        query,
        context,
        &set,
        Source::Git,
        false,
        false,
    ))
}
