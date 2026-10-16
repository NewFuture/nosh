//! Read-only command lookup for assistance running independently of the live shell.

use std::path::{Path, PathBuf};

use crate::{EmbeddedShell, Resolution, backend, host, input_assist, suggestion};

pub(crate) fn resolve_file(
    context: &input_assist::Context,
    path: Option<&str>,
    name: &str,
) -> Result<Option<PathBuf>, String> {
    let fixed = if host::contains_separator(name) {
        Some(Path::new(name))
    } else if path == context.path.as_deref() {
        context.hashed_commands.get(name).map(PathBuf::as_path)
    } else {
        None
    };
    let search =
        path.filter(|_| fixed.is_none() || (context.check_hash && !host::contains_separator(name)));
    let paths = fixed.into_iter().map(|p| context.cwd.join(p)).chain(
        search
            .into_iter()
            .flat_map(std::env::split_paths)
            .take(129)
            .map(|directory| context.cwd.join(directory).join(name)),
    );
    let mut failure = None;
    for (index, path) in paths.enumerate() {
        if index >= 128 + usize::from(fixed.is_some()) {
            return Err("command lookup directory limit".into());
        }
        let path = host::resolve_executable(&path).unwrap_or(path);
        match std::fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => {
                match host::check_access(&path, host::Access::Execute) {
                    Ok(()) => return Ok(Some(path)),
                    Err(error)
                        if matches!(
                            error.kind(),
                            std::io::ErrorKind::PermissionDenied
                                | std::io::ErrorKind::NotFound
                                | std::io::ErrorKind::NotADirectory
                        ) => {}
                    Err(error) => failure = Some(format!("{}: {error}", path.display())),
                }
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => failure = Some(format!("{}: {error}", path.display())),
        }
    }
    failure.map_or(Ok(None), Err)
}

#[derive(Debug, Clone)]
pub struct CommandSnapshot(input_assist::Context);

impl CommandSnapshot {
    pub fn capture(shell: &EmbeddedShell) -> Result<Self, String> {
        shell
            .input_context(&crate::TriggerConfig::default(), &Default::default())
            .map(Self)
    }

    pub fn cwd(&self) -> &Path {
        &self.0.cwd
    }

    pub fn resolve(&self, name: &str) -> Resolution {
        if self.0.aliases.contains(name) {
            return Resolution::Alias(String::new());
        }
        if self.0.functions.contains(name) {
            return Resolution::Function;
        }
        if self.0.builtins.contains(name) {
            return Resolution::Builtin;
        }
        if host::contains_separator(name) {
            let path = self.0.cwd.join(name);
            return host::resolve_executable(&path).map_or(Resolution::NotFound, Resolution::File);
        }
        if let Some(path) = self.0.hashed_commands.get(name) {
            let path = self.0.cwd.join(path);
            if !self.0.check_hash || backend::is_executable(&path) {
                return Resolution::File(path);
            }
        }
        self.0
            .path
            .as_deref()
            .into_iter()
            .flat_map(std::env::split_paths)
            .map(|dir| self.0.cwd.join(dir).join(name))
            .find_map(|path| host::resolve_executable(&path))
            .map_or(Resolution::NotFound, Resolution::File)
    }

    /// Candidate names, not a guarantee that every entry is executable.
    pub fn candidates(&self, prefix: &str) -> (Vec<String>, bool, Option<String>) {
        let index = input_assist::scan_index(&self.0.cwd, self.0.path.as_deref());
        let mut names: std::collections::BTreeSet<_> = self
            .0
            .builtins
            .iter()
            .chain(&self.0.functions)
            .chain(&self.0.aliases)
            .chain(index.names.iter())
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect();
        let complete = index.complete && names.len() <= 100;
        while names.len() > 100 {
            names.pop_last();
        }
        let reason = index
            .reason
            .or_else(|| (!complete).then(|| "candidate limit reached".into()));
        (names.into_iter().collect(), complete, reason)
    }

    pub fn validate(&self, program: &str) -> bool {
        suggestion::validate_view(program, self)
    }
}

impl suggestion::CommandView for CommandSnapshot {
    fn parse(&self, text: &str) -> Option<brush_parser::ast::Program> {
        let options = self.0.options();
        let tokens =
            brush_parser::uncached_tokenize_str(text, &options.tokenizer_options()).ok()?;
        brush_parser::parse_tokens(&tokens, &options).ok()
    }

    fn resolve(&self, name: &str) -> Resolution {
        self.resolve(name)
    }

    fn var(&self, name: &str) -> Option<String> {
        match name {
            "HOME" => self.0.home.clone(),
            "OLDPWD" => self.0.oldpwd.clone(),
            _ => None,
        }
    }

    fn cwd(&self) -> PathBuf {
        self.0.cwd.clone()
    }
}
