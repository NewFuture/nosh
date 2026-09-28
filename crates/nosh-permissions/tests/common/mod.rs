use nosh_permissions::Context;

pub fn workspace_context() -> (tempfile::TempDir, Context) {
    let dir = tempfile::tempdir_in(std::env::current_dir().unwrap()).unwrap();
    let root = std::fs::canonicalize(dir.path()).unwrap();
    let home = root.join("home");
    let workspace = home.join("proj");
    std::fs::create_dir_all(&workspace).unwrap();
    let context = Context::new(&workspace, &workspace).with_home(&home);
    (dir, context)
}
