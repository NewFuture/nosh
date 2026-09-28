use std::path::{Path, PathBuf};

use nosh_permissions::{Context, PathClass, classify_path};

fn non_temp_dir(path: &Path) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).ok()?;
    (!matches!(classify_path(&path, &Context::default()), PathClass::Temp)).then_some(path)
}

pub fn workspace_context() -> (tempfile::TempDir, Context) {
    let bases = std::iter::once(std::env::current_dir().unwrap()).chain(
        ["HOME", "USERPROFILE"]
            .into_iter()
            .filter_map(std::env::var_os)
            .map(PathBuf::from),
    );
    let dir = bases
        .filter_map(|base| non_temp_dir(&base))
        .find_map(|base| tempfile::tempdir_in(base).ok())
        .expect("tests need a writable non-temporary directory");
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let home = root.join("home");
    let workspace = home.join("proj");
    std::fs::create_dir_all(&workspace).unwrap();
    let context = Context::new(&workspace, &workspace).with_home(&home);
    (dir, context)
}
