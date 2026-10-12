use std::path::Path;

use nosh_shell::abbreviations::{Operation, Session};

pub fn execute(
    path: &Path,
    shell: &nosh_shell::EmbeddedShell,
    operation: &Operation,
) -> Result<String, String> {
    let config = crate::config::abbreviations::load(path)?;
    if matches!(operation, Operation::List) {
        return Session::inspect(
            shell,
            config,
            nosh_shell::input_assist::WorkerCommand::nosh().map_err(|e| e.to_string())?,
        )
        .map(|registry| registry.display());
    }
    if let Operation::Set {
        name,
        override_existing: false,
        ..
    } = operation
    {
        let mut proposed = config;
        operation.apply(&mut proposed, "")?;
        proposed.enabled = true;
        proposed.builtins = false;
        proposed.disabled.clear();
        proposed.rules.retain(|key, _| key == name);
        let registry = Session::inspect(
            shell,
            proposed,
            nosh_shell::input_assist::WorkerCommand::nosh().map_err(|e| e.to_string())?,
        )?;
        let entry = registry
            .entries
            .iter()
            .find(|entry| entry.name == *name && entry.source.starts_with("user"))
            .ok_or_else(|| format!("{name}: rule inspection did not return a result; not saved"))?;
        if !entry.available {
            return Err(format!("{name}: {}; not saved", entry.reason));
        }
    }
    crate::config::abbreviations::save(path, operation)
}

pub fn run(words: &[String]) -> i32 {
    let result = (|| {
        let operation = Operation::parse(words)?;
        let shell = nosh_shell::EmbeddedShell::new(nosh_shell::ShellOptions::default())
            .map_err(|e| e.to_string())?;
        let result = execute(&nosh_platform::paths::config_file(), &shell, &operation)?;
        if matches!(operation, Operation::List) {
            println!(
                "Current nosh inspection context; parent-shell aliases/functions are not visible."
            );
        }
        println!("{result}");
        Ok::<_, String>(())
    })();
    match result {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("nosh: {}", nosh_shell::style::visible_text(&error));
            2
        }
    }
}
