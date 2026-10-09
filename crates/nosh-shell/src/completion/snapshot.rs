use std::collections::{BTreeMap, BTreeSet};
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
    let mut environment = BTreeMap::new();
    let mut environment_bytes = 0;
    for (name, variable) in shell.env().iter_exported() {
        if !variable.value().is_set()
            || !(matches!(
                name.as_str(),
                "HOME"
                    | "GIT_DIR"
                    | "GIT_WORK_TREE"
                    | "GIT_COMMON_DIR"
                    | "GIT_NAMESPACE"
                    | "GIT_CEILING_DIRECTORIES"
                    | "GIT_DISCOVERY_ACROSS_FILESYSTEM"
                    | "XDG_CONFIG_HOME"
                    | "GNUMAKEFLAGS"
                    | "MAKEFLAGS"
                    | "MAKEFILES"
                    | "LANG"
            ) || name.starts_with("GIT_CONFIG_"))
        {
            continue;
        }
        let value = variable.value().to_cow_str(&shell);
        environment_bytes += name.len() + value.len() + 64;
        if environment_bytes > crate::input_assist::MAX_CONTEXT {
            return Err("completion environment limit".into());
        }
        environment.insert(name.clone(), value.into_owned());
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
        environment,
    };
    crate::input_assist::write_json(
        &native,
        &mut std::io::sink(),
        crate::input_assist::MAX_CONTEXT,
    )
    .map_err(|error| format!("completion snapshot: {error}"))?;
    Ok(Snapshot {
        native: Arc::new(native),
        script: Some(execution_state(&shell).map_err(|error| format!("script snapshot: {error}"))),
    })
}

pub(super) fn execution_state(
    shell: &crate::backend::BrushShell,
) -> std::io::Result<Arc<serde_json::value::RawValue>> {
    let bytes = crate::input_assist::bounded_json(&shell.completion_state(), MAX_SNAPSHOT)?;
    let json = String::from_utf8(bytes).map_err(std::io::Error::other)?;
    serde_json::value::RawValue::from_string(json)
        .map(Arc::from)
        .map_err(std::io::Error::other)
}
