mod git;
pub(super) mod make;
mod package;

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use super::cache::{Cache, Entry, Set};
use super::context::Context;
use super::types::*;

pub(crate) fn generate(
    query: Query,
    context: &Context,
    snapshot: &NativeSnapshot,
    cache: &mut Cache,
) -> Option<Answer> {
    let name = context.command.as_deref()?;
    if context.index == 0
        || snapshot.context.aliases.contains(name)
        || snapshot.context.functions.contains(name)
        || snapshot.context.builtins.contains(name)
    {
        return None;
    }
    let result = match std::path::Path::new(name).file_name()?.to_str()? {
        "git" => git::generate(query.clone(), context, snapshot, cache),
        "make" | "gmake" => make::generate(query.clone(), context, snapshot, cache),
        "npm" => package::generate(query.clone(), context, snapshot, cache, Source::Npm),
        "yarn" => package::generate(query.clone(), context, snapshot, cache, Source::Yarn),
        _ => return None,
    };
    Some(result.unwrap_or_else(|error| Answer::failed(query, error)))
}

fn version(
    context: &Context,
    snapshot: &NativeSnapshot,
    cache: &mut Cache,
) -> Result<(PathBuf, String), String> {
    let executable = program(context, snapshot)?;
    let key = format!("version\0{}", executable.display());
    if let Some(set) = cache.get(&key, Duration::from_secs(5)) {
        return Ok((
            executable,
            set.entries
                .first()
                .ok_or("empty provider version cache")?
                .value
                .clone(),
        ));
    }
    let value = output(
        context,
        snapshot,
        &["--version".into()],
        &snapshot.context.cwd,
        4096,
    )?;
    cache.insert(
        key,
        Set {
            entries: vec![Entry {
                value: value.clone(),
                kind: Kind::Value,
                description: None,
            }],
            reason: None,
        },
    );
    Ok((executable, value))
}

fn program(context: &Context, snapshot: &NativeSnapshot) -> Result<PathBuf, String> {
    let name = context.command.as_deref().ok_or("missing command")?;
    if name.contains('/') {
        let path = snapshot.context.cwd.join(name);
        return crate::backend::is_executable(&path)
            .then_some(path)
            .ok_or_else(|| "provider command is not executable".into());
    }
    if context.path == snapshot.context.path
        && let Some(path) = snapshot.context.hashed_commands.get(name)
    {
        let path = snapshot.context.cwd.join(path);
        return crate::backend::is_executable(&path)
            .then_some(path)
            .ok_or_else(|| "hashed provider command is not executable".into());
    }
    context
        .path
        .as_deref()
        .into_iter()
        .flat_map(|path| path.split(':'))
        .take(128)
        .map(|directory| snapshot.context.cwd.join(directory).join(name))
        .find(|path| crate::backend::is_executable(path))
        .ok_or_else(|| "provider command was not found in the current PATH".into())
}

fn output(
    context: &Context,
    snapshot: &NativeSnapshot,
    args: &[String],
    cwd: &std::path::Path,
    limit: usize,
) -> Result<String, String> {
    let mut command = Command::new(program(context, snapshot)?);
    command
        .args(args)
        .current_dir(cwd)
        .envs(&snapshot.environment)
        .env("PATH", context.path.as_deref().unwrap_or(""))
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(home) = &snapshot.context.home {
        command.env("HOME", home);
    }
    let mut child = command.spawn().map_err(|error| error.to_string())?;
    let result = (|| {
        let stdout = child.stdout.take().ok_or("missing provider output pipe")?;
        let mut bytes = Vec::new();
        stdout
            .take((limit + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        if bytes.len() > limit {
            return Err("provider output limit reached".into());
        }
        let status = child.wait().map_err(|error| error.to_string())?;
        if !status.success() {
            return Err(format!(
                "{} query exited with {status}",
                context.command.as_deref().unwrap_or("provider")
            ));
        }
        String::from_utf8(bytes).map_err(|_| "provider returned non-UTF-8 names".into())
    })();
    if result.is_err()
        && child
            .try_wait()
            .map_err(|error| error.to_string())?
            .is_none()
    {
        child
            .kill()
            .map_err(|error| format!("provider failure; cannot stop child: {error}"))?;
        child
            .wait()
            .map_err(|error| format!("provider failure; cannot reap child: {error}"))?;
    }
    result
}

fn entries(values: &[(&str, &str)], kind: Kind) -> Set {
    Set {
        entries: values
            .iter()
            .map(|(value, description)| Entry {
                value: (*value).into(),
                kind,
                description: Some((*description).into()),
            })
            .collect(),
        reason: None,
    }
}

fn value_context(context: &Context, query: &Query) -> Context {
    let mut span = context.span.clone();
    let mut word = context.word.clone();
    if let Some((_, value)) = word.split_once('=') {
        if let Some(offset) = query.text[span.clone()].find('=') {
            span.start += offset + 1;
        }
        word = value.into();
    }
    if context.quote.is_some() && query.text[span.clone()].ends_with(context.quote.unwrap_or('"')) {
        span.end = span.end.saturating_sub(1);
    }
    Context {
        word,
        span,
        redirect: false,
        ..context.clone()
    }
}
