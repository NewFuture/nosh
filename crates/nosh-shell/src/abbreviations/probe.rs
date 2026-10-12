use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::{Config, Registry, validate_expansion};
use crate::command_snapshot::resolve_file;
use crate::input_assist::Context;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Probe {
    pub serial: u64,
    pub config: Config,
    pub context: Context,
}

fn local(context: &Context, name: &str) -> Option<&'static str> {
    if context.aliases.contains(name) {
        Some("alias")
    } else if context.functions.contains(name) {
        Some("function")
    } else if context.builtins.contains(name) {
        Some("builtin")
    } else {
        None
    }
}

fn program(context: &Context, name: &str) -> Result<PathBuf, String> {
    if let Some(kind) = local(context, name) {
        return Err(format!(
            "{name} resolves to a {kind}; external usage is not confirmed"
        ));
    }
    resolve_file(context, context.path.as_deref(), name)?
        .ok_or_else(|| format!("target command {name} is unavailable"))
}

fn output(context: &Context, program: &std::path::Path, args: &[&str]) -> Result<String, String> {
    let mut command = Command::new(program);
    command
        .args(args)
        .current_dir(&context.cwd)
        .env_clear()
        .env("PATH", context.path.as_deref().unwrap_or(""))
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if let Some(marker) = std::env::var_os(crate::procs::RUN_VAR) {
        command.env(crate::procs::RUN_VAR, marker);
    }
    let mut child = command.spawn().map_err(|e| e.to_string())?;
    let result = (|| {
        let mut bytes = Vec::new();
        child
            .stdout
            .take()
            .ok_or("probe has no output pipe")?
            .take(4097)
            .read_to_end(&mut bytes)
            .map_err(|e| e.to_string())?;
        if bytes.len() > 4096 {
            return Err("capability probe output exceeds 4 KiB".into());
        }
        let status = child.wait().map_err(|e| e.to_string())?;
        if !status.success() {
            return Err(format!("capability probe exited with {status}"));
        }
        String::from_utf8(bytes).map_err(|_| "capability probe returned non-UTF-8 output".into())
    })();
    if result.is_err() && child.try_wait().map_err(|e| e.to_string())?.is_none() {
        child.kill().map_err(|e| e.to_string())?;
        child.wait().map_err(|e| e.to_string())?;
    }
    result
}

#[derive(PartialEq, Eq)]
struct Identity {
    path: PathBuf,
    cwd: PathBuf,
    search: Option<String>,
    file: (u64, u64, u64, i64, i64, u32),
}

struct Cached {
    identity: Identity,
    checked: Instant,
    result: Result<String, String>,
}

#[derive(Default)]
pub(crate) struct Checker {
    capabilities: BTreeMap<String, Cached>,
}

fn capability(context: &Context, target: &str, cache: &mut Checker) -> Result<String, String> {
    if target == "cd" {
        return if local(context, "cd") == Some("builtin") {
            Ok("available: shell cd builtin".into())
        } else {
            Err("cd is disabled or shadowed by an alias/function".into())
        };
    }
    let resolved = program(context, target)?;
    let metadata = std::fs::metadata(&resolved).map_err(|e| e.to_string())?;
    let identity = Identity {
        path: resolved.clone(),
        cwd: context.cwd.clone(),
        search: context.path.clone(),
        file: (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.mode(),
        ),
    };
    if let Some(cached) = cache.capabilities.get(target)
        && cached.identity == identity
        && cached.checked.elapsed() < Duration::from_secs(5)
    {
        return cached.result.clone();
    }
    let result = check_capability(context, target, &resolved);
    cache.capabilities.insert(
        target.into(),
        Cached {
            identity,
            checked: Instant::now(),
            result: result.clone(),
        },
    );
    result
}

fn check_capability(
    context: &Context,
    target: &str,
    resolved: &std::path::Path,
) -> Result<String, String> {
    if target == "git" {
        let text = output(context, resolved, &["--version"])?;
        let major = text
            .trim()
            .strip_prefix("git version ")
            .and_then(|version| version.split('.').next())
            .and_then(|major| major.parse::<u32>().ok());
        if major.is_some_and(|major| major >= 2) {
            return Ok(format!("available: {}", text.trim()));
        }
        return Err("Git 2+ usage could not be confirmed".into());
    }
    if cfg!(target_os = "macos")
        && std::fs::canonicalize(resolved).map_err(|e| e.to_string())?
            == std::path::Path::new("/bin/ls")
    {
        output(context, resolved, &["-ldAh", "."])?;
        return Ok("available: macOS BSD ls (-lh / -lAh)".into());
    }
    let text = output(context, resolved, &["--version"])?;
    if text.starts_with("ls (GNU coreutils)") {
        Ok("available: GNU ls (-lh / -lAh)".into())
    } else if text.starts_with("ls (uutils coreutils)") {
        output(context, resolved, &["-ldAh", "."])?;
        Ok("available: uutils ls (-lh / -lAh)".into())
    } else {
        Err(
            "ls implementation/flags are not confirmed; supported: GNU, uutils and macOS BSD"
                .into(),
        )
    }
}

#[cfg(test)]
pub(crate) fn check(probe: &Probe) -> Registry {
    Checker::default().check(probe)
}

impl Checker {
    pub(crate) fn check(&mut self, probe: &Probe) -> Registry {
        if let Err(error) = probe.config.validate(&probe.context.ai_prefix) {
            return Registry::unavailable(&probe.config, &error);
        }
        let mut registry = Registry::pending(&probe.config);
        let mut capabilities = BTreeMap::new();
        for entry in &mut registry.entries {
            if !entry.reason.starts_with("checking") {
                continue;
            }
            let user = probe
                .config
                .rules
                .get(&entry.name)
                .filter(|_| entry.source != "builtin");
            let result = (|| {
                if user.is_none_or(|rule| !rule.override_existing) {
                    if let Some(kind) = local(&probe.context, &entry.name) {
                        return Err(format!(
                            "name conflicts with an existing {kind}; use an explicit user override"
                        ));
                    }
                    if let Some(path) =
                        resolve_file(&probe.context, probe.context.path.as_deref(), &entry.name)?
                    {
                        return Err(format!("name conflicts with executable {}", path.display()));
                    }
                }
                if user.is_some() {
                    validate_expansion(&entry.expansion)?;
                    return Ok("available: explicit user text; execution is not validated".into());
                }
                let target = entry.expansion.split_whitespace().next().unwrap_or("");
                capabilities
                    .entry(target.to_owned())
                    .or_insert_with(|| capability(&probe.context, target, self))
                    .clone()
            })();
            match result {
                Ok(reason) => {
                    entry.available = true;
                    entry.reason = reason;
                    if entry.source == "builtin"
                        && let Some(target) = entry.expansion.split_whitespace().next()
                        && let Some(confirmed) = self.capabilities.get(target)
                    {
                        registry
                            .programs
                            .insert(target.into(), confirmed.identity.path.clone());
                    }
                }
                Err(error) => entry.reason = error,
            }
        }
        registry
    }
}
