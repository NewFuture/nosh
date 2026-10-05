use std::time::Duration;

use super::*;

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
        return Ok(paths(query, context, snapshot, cache, true));
    }
    if context.index == command_index {
        let key = format!(
            "git-commands\0{}\0{:?}\0{:?}",
            executable.display(),
            snapshot.context.cwd,
            prefix
        );
        let set = cache.load(key, Duration::from_secs(1), || {
            let mut args = prefix.clone();
            args.push("--list-cmds=main,others,nohelpers,alias".into());
            let data = output(
                context,
                snapshot,
                &args,
                &snapshot.context.cwd,
                MAX_SET_BYTES,
            )?;
            Ok(Set::lines(data.lines(), Kind::Subcommand, None))
        })?;
        return Ok(select(query, context, &set, Source::Git));
    }
    if context.words.get(command_index).map(String::as_str) != Some("switch") {
        return Ok(Answer::unavailable(
            query,
            "built-in Git parameter completion currently covers switch; load a definition for this subcommand",
        ));
    }
    if previous == Some("--conflict") || context.word.starts_with("--conflict=") {
        let mut styles = vec![
            ("merge", "Show two-way conflict markers"),
            ("diff3", "Also show the original contents"),
        ];
        if (numbers[0], numbers[1]) >= (2, 35) {
            styles.push(("zdiff3", "Show compact three-way conflict markers"));
        }
        return Ok(select(
            query.clone(),
            &value_context(context, &query),
            &entries(&styles, Kind::Value),
            Source::Git,
        ));
    }
    if matches!(
        previous,
        Some("-c" | "-C" | "--create" | "--force-create" | "--orphan")
    ) {
        return Ok(Answer::unavailable(
            query,
            "this option takes a new branch name, not an existing path or branch",
        ));
    }
    let prior = &context.words[command_index + 1..context.index];
    let after_double_dash = prior.iter().any(|word| word == "--");
    if context.word.starts_with('-') && !after_double_dash {
        let mut options = entries(SWITCH_OPTIONS, Kind::Option);
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
        return Ok(select(query, context, &options, Source::Git));
    }
    let tracking = prior.iter().map(String::as_str).rev().find(|word| {
        matches!(
            *word,
            "--track" | "-t" | "--track=direct" | "--track=inherit" | "--no-track"
        )
    });
    let creating = prior.iter().any(|word| {
        matches!(word.as_str(), "-c" | "-C" | "--create" | "--force-create")
            || word.starts_with("--create=")
            || word.starts_with("--force-create=")
    });
    let refs: &[&str] = match tracking {
        Some("--track=inherit") => &["refs/heads"],
        Some("--track" | "-t" | "--track=direct") if creating => &["refs/heads", "refs/remotes"],
        Some("--track" | "-t" | "--track=direct") => &["refs/remotes"],
        _ if creating
            || prior
                .iter()
                .any(|word| matches!(word.as_str(), "--detach" | "-d")) =>
        {
            &["refs/heads", "refs/tags", "refs/remotes"]
        }
        _ => &["refs/heads"],
    };
    let key = format!(
        "git-refs\0{}\0{:?}\0{:?}\0{refs:?}",
        executable.display(),
        snapshot.context.cwd,
        prefix
    );
    let set = cache.load(key, Duration::from_secs(1), || {
        let mut args = prefix;
        args.extend(["for-each-ref".into(), "--format=%(refname:strip=2)".into()]);
        args.extend(refs.iter().map(|reference| (*reference).into()));
        let data = output(
            context,
            snapshot,
            &args,
            &snapshot.context.cwd,
            MAX_SET_BYTES,
        )?;
        Ok(Set::lines(
            data.lines().filter(|name| !name.ends_with("/HEAD")),
            Kind::Branch,
            Some("Git ref"),
        ))
    })?;
    Ok(select(query, context, &set, Source::Git))
}
