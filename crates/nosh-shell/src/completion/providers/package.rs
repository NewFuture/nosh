use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use super::*;

const MAX_MANIFEST: usize = 2 * 1024 * 1024;
const YARN_COMMANDS: &[&str] = &[
    "add",
    "audit",
    "bin",
    "cache",
    "config",
    "constraints",
    "create",
    "dedupe",
    "dlx",
    "exec",
    "explain",
    "global",
    "help",
    "import",
    "info",
    "init",
    "install",
    "link",
    "list",
    "node",
    "npm",
    "pack",
    "patch",
    "patch-commit",
    "plugin",
    "policies",
    "publish",
    "rebuild",
    "remove",
    "run",
    "search",
    "set",
    "stage",
    "unlink",
    "unplug",
    "up",
    "upgrade",
    "upgrade-interactive",
    "version",
    "versions",
    "why",
    "workspace",
    "workspaces",
];

#[derive(serde::Deserialize)]
struct Manifest {
    #[serde(default)]
    scripts: BTreeMap<String, String>,
}

pub(super) fn generate(
    query: Query,
    context: &Context,
    snapshot: &NativeSnapshot,
    cache: &mut Cache,
    source: Source,
) -> Result<Answer, String> {
    program(context, snapshot)?;
    let npm = source == Source::Npm;
    let prefix = if npm { "--prefix=" } else { "--cwd=" };
    let option = prefix.trim_end_matches('=');
    let mut cwd = snapshot.context.cwd.clone();
    let mut index = 1;
    while index < context.index {
        let word = &context.words[index];
        let value = if word == option {
            index += 1;
            if index == context.index {
                return Ok(paths(query, context, snapshot, cache, true));
            }
            Some(
                context
                    .words
                    .get(index)
                    .ok_or("missing package directory")?
                    .as_str(),
            )
        } else {
            word.strip_prefix(prefix)
        };
        let Some(value) = value else { break };
        if value.contains(['$', '`']) {
            return Err("dynamic package directory needs a loaded completion definition".into());
        }
        cwd = snapshot.context.cwd.join(value);
        index += 1;
    }
    if index == context.index && context.word.starts_with(prefix) {
        return Ok(paths(query, context, snapshot, cache, true));
    }
    if index == context.index && context.word.starts_with('-') {
        return Ok(select(
            query,
            context,
            &entries(&[(prefix, "Project directory")], Kind::Option),
            source,
        ));
    }
    if npm && index == context.index {
        return Ok(select(
            query,
            context,
            &entries(
                &[
                    ("run", "Run a project script"),
                    ("run-script", "Run a project script"),
                ],
                Kind::Subcommand,
            ),
            source,
        ));
    }
    let explicit_run = context
        .words
        .get(index)
        .is_some_and(|word| word == "run" || (npm && word == "run-script"));
    if explicit_run {
        index += 1;
    }
    if index != context.index || (npm && !explicit_run) {
        return Ok(Answer::unavailable(
            query,
            "built-in package completion supports project script names only",
        ));
    }
    let manifest = cwd
        .ancestors()
        .take(32)
        .find_map(|directory| {
            let path = directory.join("package.json");
            match fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => Some(Ok(path)),
                Ok(_) => Some(Err("package.json is not a file".to_string())),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => Some(Err(format!("{}: {error}", path.display()))),
            }
        })
        .ok_or("no package.json found within 32 ancestor directories")??;
    let shortcut = !npm && !explicit_run;
    let key = format!("package-scripts\0{}\0{shortcut}", manifest.display());
    let set = cache.load(key, Duration::from_secs(1), || read(&manifest, shortcut))?;
    Ok(select(query, context, &set, source))
}

fn read(path: &Path, shortcut: bool) -> Result<Set, String> {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .and_then(|file| file.take((MAX_MANIFEST + 1) as u64).read_to_end(&mut bytes))
        .map_err(|error| format!("{}: {error}", path.display()))?;
    if bytes.len() > MAX_MANIFEST {
        return Err("package.json read limit reached".into());
    }
    let manifest: Manifest =
        serde_json::from_slice(&bytes).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Set::collect(
        manifest
            .scripts
            .into_iter()
            .filter(|(name, _)| !shortcut || !YARN_COMMANDS.contains(&name.as_str()))
            .map(|(value, description)| {
                Ok(Entry {
                    value,
                    kind: Kind::Target,
                    description: Some(description),
                })
            }),
    ))
}
