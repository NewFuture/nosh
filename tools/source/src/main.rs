mod source;
mod worktree;

use source::Manager;
use std::{env, error::Error, path::PathBuf};

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const USAGE: &str = "usage: cargo source prepare|check|export|provenance|upgrade [--offline] [--root PATH] [--cache REPOSITORY] [--rev SHA] [--resolved CANDIDATE]";

enum Action {
    Prepare,
    Check,
    Export,
    Provenance,
    Upgrade,
}

impl Action {
    fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "prepare" => Self::Prepare,
            "check" => Self::Check,
            "export" => Self::Export,
            "provenance" => Self::Provenance,
            "upgrade" => Self::Upgrade,
            _ => return None,
        })
    }
}

fn run() -> Result<()> {
    let mut args = env::args().skip(1);
    let mut action = None;
    let mut root = None;
    let mut cache = None;
    let mut offline = false;
    let mut revision = None;
    let mut resolved = None;
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--help" | "-h" => {
                println!("{USAGE}");
                return Ok(());
            }
            "--offline" => offline = true,
            "--root" | "--cache" | "--resolved" | "--rev" => {
                let value = args
                    .next()
                    .ok_or_else(|| format!("{argument} needs a value"))?;
                match argument.as_str() {
                    "--root" => root = Some(PathBuf::from(value)),
                    "--cache" => cache = Some(dunce::canonicalize(value)?),
                    "--resolved" => resolved = Some(PathBuf::from(value)),
                    "--rev" => revision = Some(value),
                    _ => unreachable!(),
                }
            }
            value if action.is_none() => {
                action =
                    Some(Action::parse(value).ok_or_else(|| format!("unknown argument: {value}"))?);
            }
            _ => return Err(format!("unknown/repeated argument: {argument}").into()),
        }
    }
    let action = action.ok_or(USAGE)?;
    if !matches!(action, Action::Upgrade) && (revision.is_some() || resolved.is_some()) {
        return Err("--rev and --resolved are only valid with upgrade".into());
    }
    if matches!(action, Action::Upgrade) && revision.is_none() {
        return Err("upgrade requires --rev".into());
    }
    let root = match root {
        Some(root) => root,
        None => env::current_dir()?,
    };
    let manager = Manager::new(root, cache, offline)?;
    match action {
        Action::Prepare => manager.prepare(),
        Action::Export => manager.export(),
        Action::Upgrade => manager.upgrade(
            revision.as_deref().expect("upgrade revision was checked"),
            resolved.as_deref(),
        ),
        Action::Check => {
            manager.check()?;
            println!(
                "Prepared Reedline matches the committed-source inputs; no unexported changes."
            );
            Ok(())
        }
        Action::Provenance => {
            println!("{}", serde_json::to_string_pretty(&manager.check()?)?);
            Ok(())
        }
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("nosh-source: {error}");
        std::process::exit(1);
    }
}
