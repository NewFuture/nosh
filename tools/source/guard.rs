use std::{fs, path::Path};

pub fn check(root: &Path) -> Result<(), String> {
    for (dependency, inputs, generated) in [
        ("Reedline", "patches/reedline", ".nosh/reedline"),
        ("brush-core", "patches/brush-core", ".nosh/brush"),
    ] {
        for name in ["source.toml", "nosh.patch"] {
            let requested = root.join(inputs).join(name);
            let prepared = root.join(generated).join(".git/nosh").join(name);
            println!("cargo:rerun-if-changed={}", requested.display());
            println!("cargo:rerun-if-changed={}", prepared.display());
            let expected = fs::read_to_string(&requested)
                .map_err(|e| format!("{}: {e}", requested.display()))?;
            let actual = fs::read_to_string(&prepared).map_err(|e| {
            format!(
                "{dependency} is not prepared ({}: {e}). Run cargo source prepare from the repository root",
                prepared.display(),
            )
        })?;
            if actual.replace("\r\n", "\n") != expected.replace("\r\n", "\n") {
                return Err(format!(
                    "{dependency} preparation is stale. Run cargo source prepare from the repository root; unexported edits will be preserved."
                ));
            }
        }
    }
    Ok(())
}
