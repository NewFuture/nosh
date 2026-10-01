use std::{fs, path::Path};

pub fn check(root: &Path) -> Result<(), String> {
    for name in ["source.toml", "nosh.patch"] {
        let requested = root.join("patches/reedline").join(name);
        let prepared = root.join(".nosh/reedline/.git/nosh").join(name);
        println!("cargo:rerun-if-changed={}", requested.display());
        println!("cargo:rerun-if-changed={}", prepared.display());
        let expected =
            fs::read_to_string(&requested).map_err(|e| format!("{}: {e}", requested.display()))?;
        let actual = fs::read_to_string(&prepared).map_err(|e| {
            format!(
                "Reedline is not prepared ({}: {e}). Run cargo source prepare from the repository root",
                prepared.display()
            )
        })?;
        if actual.replace("\r\n", "\n") != expected.replace("\r\n", "\n") {
            return Err(
                "Reedline preparation is stale. Run cargo source prepare from the repository root; unexported edits will be preserved."
                    .into(),
            );
        }
    }
    Ok(())
}
