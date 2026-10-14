use std::path::Path;

use nosh_shell::abbreviations::{Config, Operation, Target};

pub fn parse(table: &toml::Table) -> Result<Config, String> {
    let shell = table
        .get("shell")
        .map(|item| item.as_table().ok_or("shell must be a table"))
        .transpose()?;
    let prefix = shell
        .and_then(|shell| shell.get("ai_prefix"))
        .and_then(toml::Value::as_str)
        .unwrap_or("#");
    let config = shell
        .and_then(|shell| shell.get("abbreviations"))
        .map(|value| {
            value
                .clone()
                .try_into::<Config>()
                .map_err(|e| format!("shell.abbreviations: {e}"))
        })
        .transpose()?
        .unwrap_or_default();
    config.validate(prefix)?;
    Ok(config)
}

pub fn load(path: &Path) -> Result<Config, String> {
    let text = super::read_text(path)?;
    let table = text
        .parse()
        .map_err(|e| format!("invalid configuration: {e}"))?;
    parse(&table)
}

fn child<'a>(
    table: &'a mut dyn toml_edit::TableLike,
    name: &str,
) -> Result<&'a mut dyn toml_edit::TableLike, String> {
    if !table.contains_key(name) {
        table.insert(name, toml_edit::Item::Table(toml_edit::Table::new()));
    }
    table
        .get_mut(name)
        .and_then(toml_edit::Item::as_table_like_mut)
        .ok_or_else(|| format!("{name} must be a table"))
}

pub fn save(path: &Path, operation: &Operation) -> Result<String, String> {
    super::document::update(path, |document| {
        let table: toml::Table = document
            .to_string()
            .parse()
            .map_err(|e| format!("invalid configuration: {e}"))?;
        let prefix = table
            .get("shell")
            .and_then(|s| s.get("ai_prefix"))
            .and_then(toml::Value::as_str)
            .unwrap_or("#");
        let mut config = parse(&table)?;
        let result = operation.apply(&mut config, prefix)?;
        let shell = child(document.as_table_mut(), "shell")?;
        let section = child(shell, "abbreviations")?;
        match operation {
            Operation::Set {
                name,
                expansion,
                override_existing,
            } => {
                let rules = child(section, "rules")?;
                let mut value = toml_edit::InlineTable::new();
                value.insert("expansion", expansion.as_str().into());
                value.insert("override", (*override_existing).into());
                let mut value = toml_edit::Value::InlineTable(value);
                if let Some(old) = rules.get(name).and_then(toml_edit::Item::as_value) {
                    *value.decor_mut() = old.decor().clone();
                }
                rules.insert(name, toml_edit::Item::Value(value));
            }
            Operation::Remove(name) => {
                child(section, "rules")?.remove(name);
            }
            Operation::Toggle {
                target: Target::All,
                enabled,
            } => {
                section.insert("enabled", toml_edit::value(*enabled));
            }
            Operation::Toggle {
                target: Target::Builtins,
                enabled,
            } => {
                section.insert("builtins", toml_edit::value(*enabled));
            }
            Operation::Toggle {
                target: Target::Name(_),
                ..
            } => {
                let values = config
                    .disabled
                    .iter()
                    .map(String::as_str)
                    .collect::<toml_edit::Array>();
                section.insert("disabled", toml_edit::value(values));
            }
            Operation::List => return Err("list is not a configuration update".into()),
        }
        Ok(result)
    })
}
