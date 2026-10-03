use std::collections::BTreeSet;
use std::sync::Arc;

use super::types::*;
use crate::EmbeddedShell;

pub(crate) fn capture(
    shell: &EmbeddedShell,
    scripts: bool,
    abbreviations: &crate::input_assist::Abbreviations,
) -> Result<Snapshot, String> {
    let context = shell.input_context(&crate::TriggerConfig::default(), &Default::default())?;
    let (_, shared) = shell.shared();
    let shell = shared.try_lock().map_err(|_| "completion snapshot busy")?;
    let mut variables = BTreeSet::new();
    let mut variable_bytes = 0;
    let mut variables_complete = true;
    for (name, _) in shell.env().iter() {
        if variables.len() >= MAX_SET || variable_bytes + name.len() + 32 > 64 * 1024 {
            variables_complete = false;
            break;
        }
        variable_bytes += name.len() + 32;
        variables.insert(name.clone());
    }
    let native = NativeSnapshot {
        context,
        registry: Registry::capture(&shell),
        variables,
        variables_complete,
        word_breaks: shell
            .env_str("COMP_WORDBREAKS")
            .map_or_else(|| " \t\n\"'@><=;|&(:".into(), |value| value.into_owned()),
        scripts,
        abbreviations: abbreviations.clone(),
        nocase_paths: shell.options().case_insensitive_pathname_expansion,
        environment: [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_COMMON_DIR",
            "GIT_NAMESPACE",
            "GIT_CONFIG_PARAMETERS",
            "GIT_CONFIG_COUNT",
            "XDG_CONFIG_HOME",
            "MAKEFLAGS",
            "MAKEFILES",
            "LANG",
        ]
        .into_iter()
        .filter_map(|name| {
            shell
                .env_str(name)
                .map(|value| (name.into(), value.into_owned()))
        })
        .collect(),
    };
    crate::input_assist::write_json(
        &native,
        &mut std::io::sink(),
        crate::input_assist::MAX_CONTEXT,
    )
    .map_err(|error| format!("completion snapshot: {error}"))?;
    let script = crate::input_assist::bounded_json(&shell.completion_state(), MAX_SNAPSHOT)
        .map_err(|error| format!("script snapshot: {error}"))
        .and_then(|bytes| String::from_utf8(bytes).map_err(|error| error.to_string()))
        .and_then(|json| {
            serde_json::value::RawValue::from_string(json)
                .map(Arc::from)
                .map_err(|error| error.to_string())
        });
    Ok(Snapshot {
        native: Arc::new(native),
        script,
    })
}
