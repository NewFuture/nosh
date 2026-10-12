//! Explicit rules for interactive draft expansion, never execution aliases.

mod context;
mod probe;
pub(crate) mod service;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

pub(crate) use context::command_ranges;
#[cfg(test)]
pub(crate) use probe::check;
pub(crate) use probe::{Checker, Probe};
pub use service::Session;

pub const MAX_RULES: usize = 256;
pub const MAX_NAME: usize = 128;
pub const MAX_EXPANSION: usize = 8 * 1024;
pub const MAX_BYTES: usize = 256 * 1024;
pub const MAX_REPLACEMENTS: usize = 256;

pub const BUILTINS: &[(&str, &str)] = &[
    ("ll", "ls -lh"),
    ("la", "ls -lAh"),
    ("..", "cd .."),
    ("...", "cd ../.."),
    ("gst", "git status"),
    ("gd", "git diff"),
    ("gds", "git diff --staged"),
    ("gb", "git branch"),
    ("glo", "git log --oneline --decorate"),
    ("glog", "git log --oneline --decorate --graph"),
];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rule {
    pub expansion: String,
    #[serde(default, rename = "override")]
    pub override_existing: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub enabled: bool,
    pub builtins: bool,
    pub disabled: BTreeSet<String>,
    pub rules: BTreeMap<String, Rule>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enabled: true,
            builtins: true,
            disabled: BTreeSet::new(),
            rules: BTreeMap::new(),
        }
    }
}

impl Config {
    pub fn disabled() -> Self {
        Self {
            enabled: false,
            ..Self::default()
        }
    }

    pub fn validate(&self, prefix: &str) -> Result<(), String> {
        if self.rules.len() + self.disabled.len() > MAX_RULES {
            return Err(format!(
                "abbreviations: at most {MAX_RULES} rules and disabled names"
            ));
        }
        let mut size = 0usize;
        for name in self.rules.keys().chain(self.disabled.iter()) {
            validate_name(name, prefix)?;
            size += name.len() + 128;
        }
        for (name, rule) in &self.rules {
            let text = &rule.expansion;
            if text.trim().is_empty()
                || text.len() > MAX_EXPANSION
                || text.contains(['\0', '\r', '\n'])
            {
                return Err(format!(
                    "{name}: expansion must be nonempty, single-line and at most {MAX_EXPANSION} bytes"
                ));
            }
            size += text.len();
            if builtin(name).is_some() && !rule.override_existing {
                return Err(format!(
                    "{name}: overriding a built-in abbreviation requires --override"
                ));
            }
        }
        if size > MAX_BYTES {
            return Err("abbreviation configuration exceeds 256 KiB".into());
        }
        Ok(())
    }

    pub fn contains(&self, name: &str) -> bool {
        builtin(name).is_some() || self.rules.contains_key(name)
    }
}

pub fn builtin(name: &str) -> Option<&'static str> {
    BUILTINS
        .iter()
        .find(|(key, _)| *key == name)
        .map(|(_, text)| *text)
}

pub fn validate_name(name: &str, prefix: &str) -> Result<(), String> {
    const RESERVED: &[&str] = &[
        "if", "then", "else", "elif", "fi", "for", "select", "in", "do", "done", "while", "until",
        "case", "esac", "function", "time", "coproc", "!",
    ];
    if name.is_empty()
        || name.len() > MAX_NAME
        || RESERVED.contains(&name)
        || (!prefix.is_empty() && name.starts_with(prefix))
        || name
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || "'\"\\$`=|&;<>(){}[]*?!/#".contains(c))
    {
        return Err(format!(
            "invalid abbreviation name {:?}",
            crate::style::visible_text(name)
        ));
    }
    Ok(())
}

pub fn validate_expansion(text: &str) -> Result<(), String> {
    if text.trim().is_empty() || text.len() > MAX_EXPANSION || text.contains(['\0', '\r', '\n']) {
        return Err("expansion must be nonempty single-line shell text (at most 8 KiB)".into());
    }
    let depth = text.chars().filter(|c| "({".contains(*c)).count();
    if depth > 64 {
        return Err("expansion nesting limit exceeded".into());
    }
    let options = brush_parser::ParserOptions::default();
    let tokens = brush_parser::uncached_tokenize_str(text, &options.tokenizer_options())
        .map_err(|e| format!("invalid expansion: {e}"))?;
    if tokens.len() > 1024 {
        return Err("expansion token limit exceeded".into());
    }
    let program = brush_parser::parse_tokens(&tokens, &options)
        .map_err(|e| format!("invalid expansion: {e}"))?;
    if program.complete_commands.is_empty() {
        return Err("expansion must contain a shell command, not only a comment".into());
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub name: String,
    pub expansion: String,
    pub source: String,
    pub available: bool,
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    pub entries: Vec<Entry>,
    #[serde(default)]
    pub(crate) programs: BTreeMap<String, std::path::PathBuf>,
}

impl Registry {
    pub fn pending(config: &Config) -> Self {
        let mut entries = Vec::new();
        for (name, expansion) in BUILTINS {
            entries.push(Entry {
                name: (*name).into(),
                expansion: (*expansion).into(),
                source: "builtin".into(),
                available: false,
                reason: if !config.enabled || !config.builtins || config.disabled.contains(*name) {
                    "disabled"
                } else if config.rules.contains_key(*name) {
                    "overridden by user rule"
                } else {
                    "checking command and capability"
                }
                .into(),
            });
        }
        for (name, rule) in &config.rules {
            entries.push(Entry {
                name: name.clone(),
                expansion: rule.expansion.clone(),
                source: if rule.override_existing {
                    "user (override)"
                } else {
                    "user"
                }
                .into(),
                available: false,
                reason: if !config.enabled || config.disabled.contains(name) {
                    "disabled"
                } else {
                    "checking rule and name"
                }
                .into(),
            });
        }
        Self {
            entries,
            programs: BTreeMap::new(),
        }
    }

    pub fn unavailable(config: &Config, error: &str) -> Self {
        let mut registry = Self::pending(config);
        for entry in &mut registry.entries {
            if entry.reason.starts_with("checking") {
                entry.reason = format!("unavailable: {error}");
            }
        }
        registry
    }

    pub fn approved(&self, revision: u64) -> crate::input_assist::Abbreviations {
        let definitions: BTreeMap<_, _> = self
            .entries
            .iter()
            .filter(|entry| entry.available)
            .map(|entry| {
                (
                    entry.name.clone(),
                    crate::input_assist::Abbreviation {
                        expansion: entry.expansion.clone(),
                        source: entry.source.clone(),
                    },
                )
            })
            .collect();
        crate::input_assist::Abbreviations {
            revision,
            applicable: definitions.keys().cloned().collect(),
            definitions,
        }
    }

    pub(crate) fn compatible_context(
        &self,
        previous: &crate::input_assist::Context,
        current: &crate::input_assist::Context,
        config: &Config,
    ) -> bool {
        let mut environment = current.clone();
        environment.hashed_commands = previous.hashed_commands.clone();
        if &environment != previous {
            return false;
        }
        let protected: BTreeSet<_> = self
            .entries
            .iter()
            .filter(|entry| {
                entry.available
                    && config
                        .rules
                        .get(&entry.name)
                        .is_none_or(|rule| !rule.override_existing)
            })
            .map(|entry| entry.name.as_str())
            .collect();
        for name in previous
            .hashed_commands
            .keys()
            .chain(current.hashed_commands.keys())
        {
            if previous.hashed_commands.get(name) == current.hashed_commands.get(name) {
                continue;
            }
            if protected.contains(name.as_str()) {
                return false;
            }
            if let Some(confirmed) = self.programs.get(name)
                && current
                    .hashed_commands
                    .get(name)
                    .is_none_or(|path| current.cwd.join(path) != *confirmed)
            {
                return false;
            }
        }
        true
    }

    pub fn display(&self) -> String {
        self.entries
            .iter()
            .map(|entry| {
                format!(
                    "{} -> {} [{}] {}\n",
                    crate::style::visible_text(&entry.name),
                    crate::style::visible_text(&entry.expansion),
                    entry.source,
                    crate::style::visible_text(&entry.reason)
                )
            })
            .collect()
    }
}

#[derive(Clone)]
pub struct Loader(pub Arc<dyn Fn() -> Result<Config, String> + Send + Sync>);

impl std::fmt::Debug for Loader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbbreviationLoader")
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Operation {
    List,
    Set {
        name: String,
        expansion: String,
        override_existing: bool,
    },
    Remove(String),
    Toggle {
        target: Target,
        enabled: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Target {
    Name(String),
    Builtins,
    All,
}

impl Operation {
    pub fn parse(words: &[String]) -> Result<Self, String> {
        match words {
            [] => Ok(Self::List),
            [command] if command == "list" => Ok(Self::List),
            [command, name] if command == "remove" => Ok(Self::Remove(name.clone())),
            [command, name, expansion] if command == "set" => Ok(Self::Set {
                name: name.clone(), expansion: expansion.clone(), override_existing: false,
            }),
            [command, name, expansion, flag] if command == "set" && flag == "--override" => Ok(Self::Set {
                name: name.clone(), expansion: expansion.clone(), override_existing: true,
            }),
            [command, name] if matches!(command.as_str(), "enable" | "disable") => Ok(Self::Toggle {
                enabled: command == "enable",
                target: match name.as_str() {
                    "--builtins" => Target::Builtins,
                    "--all" => Target::All,
                    _ => Target::Name(name.clone()),
                },
            }),
            _ => Err("abbr: list | set NAME 'EXPANSION' [--override] | remove NAME | enable/disable NAME|--builtins|--all".into()),
        }
    }

    pub fn apply(&self, config: &mut Config, prefix: &str) -> Result<String, String> {
        let mut next = config.clone();
        let message = self.apply_to(&mut next, prefix)?;
        *config = next;
        Ok(message)
    }

    fn apply_to(&self, config: &mut Config, prefix: &str) -> Result<String, String> {
        match self {
            Self::List => return Err("list does not change configuration".into()),
            Self::Set {
                name,
                expansion,
                override_existing,
            } => {
                validate_name(name, prefix)?;
                validate_expansion(expansion)?;
                if builtin(name).is_some() && !override_existing {
                    return Err(format!(
                        "{name}: use --override to replace the built-in rule"
                    ));
                }
                config.rules.insert(
                    name.clone(),
                    Rule {
                        expansion: expansion.clone(),
                        override_existing: *override_existing,
                    },
                );
            }
            Self::Remove(name) => {
                if config.rules.remove(name).is_none() {
                    return Err(format!(
                        "{name}: no user rule; use disable to turn off a built-in"
                    ));
                }
                config.validate(prefix)?;
                return Ok(if builtin(name).is_some() {
                    format!(
                        "{name}: user rule removed; the built-in remains subject to its switches and capability checks"
                    )
                } else {
                    format!("{name}: user rule removed")
                });
            }
            Self::Toggle {
                target: Target::All,
                enabled,
            } => config.enabled = *enabled,
            Self::Toggle {
                target: Target::Builtins,
                enabled,
            } => config.builtins = *enabled,
            Self::Toggle {
                target: Target::Name(name),
                enabled,
            } => {
                if !config.contains(name) {
                    return Err(format!("{name}: unknown abbreviation"));
                }
                if *enabled {
                    config.disabled.remove(name);
                } else {
                    config.disabled.insert(name.clone());
                }
            }
        }
        config.validate(prefix)?;
        Ok("abbreviations saved; applies to the next new input".into())
    }
}

pub type CommandCallback =
    dyn Fn(&crate::EmbeddedShell, &Operation) -> Result<String, String> + Send + Sync;

#[derive(Clone)]
pub struct Command(pub Arc<CommandCallback>);

impl std::fmt::Debug for Command {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AbbreviationCommand")
    }
}
