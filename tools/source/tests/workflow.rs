use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
};
use tempfile::TempDir;

#[path = "../guard.rs"]
mod guard;

fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().into()
}

fn write(root: &Path, name: &str, contents: &str) {
    let file = root.join(name);
    fs::create_dir_all(file.parent().unwrap()).unwrap();
    fs::write(file, contents).unwrap();
}

struct Fixture {
    _temporary: TempDir,
    root: PathBuf,
    upstream: PathBuf,
    revision: String,
}

impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("project with spaces");
        let upstream = temporary.path().join("upstream");
        fs::create_dir_all(&root).unwrap();
        fs::create_dir(&upstream).unwrap();
        git(&upstream, &["init", "--quiet"]);
        git(&upstream, &["config", "user.name", "source test"]);
        git(
            &upstream,
            &["config", "user.email", "source-test@example.invalid"],
        );
        write(
            &upstream,
            "src/lib.rs",
            "pub fn upstream() -> bool { false }\n",
        );
        write(&upstream, "delete-me", "original\n");
        write(&upstream, ".gitignore", "target/\nignored.rs\n");
        write(
            &upstream,
            "Cargo.toml",
            "[package]\nname = \"reedline\"\nversion = \"0.52.0\"\nedition = \"2021\"\n",
        );
        git(&upstream, &["add", "."]);
        git(&upstream, &["commit", "--quiet", "-m", "fixture base"]);
        let revision = git(&upstream, &["rev-parse", "HEAD"]);
        write(
            &upstream,
            "src/lib.rs",
            "pub fn upstream() -> bool { true }\n",
        );
        fs::create_dir_all(root.join("patches/reedline")).unwrap();
        git(
            &upstream,
            &[
                "diff",
                "--binary",
                "--full-index",
                &format!(
                    "--output={}",
                    root.join("patches/reedline/nosh.patch").display()
                ),
                "HEAD",
            ],
        );
        git(&upstream, &["restore", "src/lib.rs"]);
        write(
            &root,
            "patches/reedline/source.toml",
            &format!(
                "repository = \"https://github.com/nushell/reedline.git\"\nrevision = \"{revision}\"\n"
            ),
        );
        write(
            &root,
            "Cargo.toml",
            "[package]\nname=\"source-consumer\"\nversion=\"0.0.0\"\nedition=\"2021\"\n[workspace]\nexclude=[\".nosh\"]\n[dependencies]\nreedline={path=\".nosh/reedline\",version=\"=0.52.0\"}\n",
        );
        write(
            &root,
            "src/main.rs",
            "fn main() { assert!(reedline::upstream()); }\n",
        );
        git(&root, &["init", "--quiet"]);
        git(
            &root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{revision},third_party/reedline-upstream"),
            ],
        );
        Self {
            _temporary: temporary,
            root,
            upstream,
            revision,
        }
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_nosh-source"))
            .args(args)
            .arg("--root")
            .arg(&self.root)
            .arg("--cache")
            .arg(&self.upstream)
            .arg("--offline")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn fails(&self, args: &[&str], expected: &str) {
        let output = self.run(args);
        assert!(!output.status.success());
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn source(&self) -> PathBuf {
        self.root.join(".nosh/reedline")
    }
}

#[test]
fn prepare_export_new_deleted_binary_ignored_files_and_replay() {
    let fixture = Fixture::new();
    fixture.ok(&["prepare"]);
    assert!(guard::check(&fixture.root).is_ok());
    let first = fixture.ok(&["provenance"]);
    assert!(fixture.ok(&["prepare"]).contains("already prepared"));
    assert_eq!(first, fixture.ok(&["provenance"]));
    let old_patch = fs::read(fixture.root.join("patches/reedline/nosh.patch")).unwrap();
    write(&fixture.source(), "new.rs", "pub fn local() {}\n");
    write(
        &fixture.source(),
        "ignored.rs",
        "untracked but required\r\n",
    );
    fs::write(fixture.source().join("binary.bin"), [0, 1, 255]).unwrap();
    fs::remove_file(fixture.source().join("delete-me")).unwrap();
    fixture.fails(&["prepare"], "unexported");
    fixture.fails(&["check"], "unexported");
    assert_eq!(
        old_patch,
        fs::read(fixture.root.join("patches/reedline/nosh.patch")).unwrap()
    );
    fixture.ok(&["export"]);
    fixture.ok(&["check"]);
    let exported = fixture.ok(&["provenance"]);
    let saved = fixture.root.join(".nosh/saved-edits");
    fs::rename(fixture.source(), &saved).unwrap();
    fixture.ok(&["prepare"]);
    assert_eq!(exported, fixture.ok(&["provenance"]));
    assert_eq!(
        fs::read(fixture.source().join("binary.bin")).unwrap(),
        [0, 1, 255]
    );
    assert!(fixture.source().join("ignored.rs").exists());
    assert!(!fixture.source().join("delete-me").exists());
    assert_eq!(git(&fixture.upstream, &["status", "--porcelain"]), "");
}

#[test]
fn absent_source_and_stale_preparation_fail_without_fallback_or_overwrite() {
    let fixture = Fixture::new();
    let output = Command::new("cargo")
        .args(["metadata", "--offline", "--format-version", "1"])
        .current_dir(&fixture.root)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let output = Command::new(env!("CARGO_BIN_EXE_nosh-source"))
        .args(["prepare", "--offline", "--root"])
        .arg(&fixture.root)
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stderr).contains("offline:"));
    fixture.ok(&["prepare"]);
    let output = Command::new("cargo")
        .args(["metadata", "--offline", "--format-version", "1"])
        .current_dir(&fixture.root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let patch = fixture.root.join("patches/reedline/nosh.patch");
    fs::write(&patch, b"invalid patch\n").unwrap();
    fixture.fails(&["check"], "stale");
    assert!(guard::check(&fixture.root).unwrap_err().contains("stale"));
    fixture.fails(&["prepare"], "Live source and patch were not replaced");
    assert!(
        fs::read_to_string(fixture.source().join("src/lib.rs"))
            .unwrap()
            .contains("true")
    );
    write(&fixture.source(), "unsaved.rs", "developer work\n");
    fixture.fails(&["prepare"], "unexported");
    assert_eq!(
        fs::read_to_string(fixture.source().join("unsaved.rs")).unwrap(),
        "developer work\n"
    );
}

#[test]
fn archive_materialization_has_identical_provenance_without_gitlink_contents() {
    let fixture = Fixture::new();
    fixture.ok(&["prepare"]);
    let expected = fixture.ok(&["provenance"]);
    let archive_root = fixture._temporary.path().join("source archive");
    write(
        &archive_root,
        "patches/reedline/source.toml",
        &fs::read_to_string(fixture.root.join("patches/reedline/source.toml")).unwrap(),
    );
    fs::copy(
        fixture.root.join("patches/reedline/nosh.patch"),
        archive_root.join("patches/reedline/nosh.patch"),
    )
    .unwrap();
    let run = |verb| {
        Command::new(env!("CARGO_BIN_EXE_nosh-source"))
            .args([verb, "--offline", "--root"])
            .arg(&archive_root)
            .arg("--cache")
            .arg(&fixture.upstream)
            .output()
            .unwrap()
    };
    let output = run("prepare");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        String::from_utf8(run("provenance").stdout).unwrap(),
        expected
    );
}

#[test]
fn absorbed_upstream_change_can_be_resolved_and_exported_as_empty_patch() {
    let fixture = Fixture::new();
    fixture.ok(&["prepare"]);
    write(
        &fixture.upstream,
        "src/lib.rs",
        "pub fn upstream() -> bool { true }\n",
    );
    git(&fixture.upstream, &["add", "."]);
    git(
        &fixture.upstream,
        &["commit", "--quiet", "-m", "upstream absorbs fix"],
    );
    let next = git(&fixture.upstream, &["rev-parse", "HEAD"]);
    fixture.fails(&["upgrade", "--rev", &next], "Candidate retained");
    assert!(fixture.ok(&["provenance"]).contains(&fixture.revision));
    let candidate = fs::read_dir(fixture.root.join(".nosh"))
        .unwrap()
        .map(|e| e.unwrap().path())
        .find(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("staging-")
        })
        .unwrap();
    fixture.ok(&[
        "upgrade",
        "--rev",
        &next,
        "--resolved",
        candidate.to_str().unwrap(),
    ]);
    fixture.ok(&["check"]);
    assert!(
        fs::read(fixture.root.join("patches/reedline/nosh.patch"))
            .unwrap()
            .is_empty()
    );
    assert!(fixture.ok(&["provenance"]).contains(&next));
    assert!(guard::check(&fixture.root).is_ok());
}

#[test]
fn mismatched_gitlink_dirty_upstream_and_unknown_arguments_are_errors() {
    let fixture = Fixture::new();
    fixture.fails(&["prepare", "--typo"], "unknown");
    git(
        &fixture.root,
        &[
            "update-index",
            "--force-remove",
            "third_party/reedline-upstream",
        ],
    );
    fixture.fails(&["prepare"], "gitlink");
    git(
        &fixture.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{},third_party/reedline-upstream", fixture.revision),
        ],
    );
    let upstream = fixture.root.join("third_party/reedline-upstream");
    fs::create_dir_all(upstream.parent().unwrap()).unwrap();
    git(
        &fixture.root,
        &[
            "clone",
            "--quiet",
            fixture.upstream.to_str().unwrap(),
            upstream.to_str().unwrap(),
        ],
    );
    write(&upstream, "unexported", "local edit\n");
    fixture.fails(&["prepare"], "submodule is dirty");
    assert_eq!(
        fs::read_to_string(upstream.join("unexported")).unwrap(),
        "local edit\n"
    );
}

#[test]
fn consumer_build_script_compiles_and_rejects_stale_inputs() {
    let fixture = Fixture::new();
    fixture.ok(&["prepare"]);
    let manifest = fs::read_to_string(fixture.root.join("Cargo.toml")).unwrap();
    write(
        &fixture.root,
        "Cargo.toml",
        &manifest.replace(
            "[workspace]",
            "[workspace]\nmembers=[\"crates/nosh-shell\"]",
        ),
    );
    write(
        &fixture.root,
        "crates/nosh-shell/Cargo.toml",
        "[package]\nname=\"nosh-shell\"\nversion=\"0.0.0\"\nedition=\"2024\"\n",
    );
    write(&fixture.root, "crates/nosh-shell/src/lib.rs", "");
    write(
        &fixture.root,
        "crates/nosh-shell/build.rs",
        include_str!("../../../crates/nosh-shell/build.rs"),
    );
    write(
        &fixture.root,
        "tools/source/guard.rs",
        include_str!("../guard.rs"),
    );
    let cargo = |args: &[&str]| {
        Command::new("cargo")
            .args(args)
            .current_dir(&fixture.root)
            .output()
            .unwrap()
    };
    assert!(cargo(&["generate-lockfile", "--offline"]).status.success());
    let output = cargo(&["check", "--offline", "--locked", "-p", "nosh-shell"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let patch = fixture.root.join("patches/reedline/nosh.patch");
    let changed = fs::read_to_string(&patch).unwrap() + "\n";
    fs::write(patch, changed).unwrap();
    let output = cargo(&["check", "--offline", "--locked", "-p", "nosh-shell"]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("preparation is stale"));
}

#[test]
fn clean_upgrade_keeps_initialized_submodule_and_gitlink_in_sync() {
    let fixture = Fixture::new();
    write(&fixture.upstream, "upstream-news", "new upstream file\n");
    git(&fixture.upstream, &["add", "."]);
    git(
        &fixture.upstream,
        &["commit", "--quiet", "-m", "next upstream"],
    );
    let next = git(&fixture.upstream, &["rev-parse", "HEAD"]);
    let upstream = fixture.root.join("third_party/reedline-upstream");
    fs::create_dir_all(upstream.parent().unwrap()).unwrap();
    git(
        &fixture.root,
        &[
            "clone",
            "--quiet",
            fixture.upstream.to_str().unwrap(),
            upstream.to_str().unwrap(),
        ],
    );
    git(
        &upstream,
        &["checkout", "--quiet", "--detach", &fixture.revision],
    );
    fixture.ok(&["prepare"]);
    fixture.ok(&["upgrade", "--rev", &next]);
    fixture.ok(&["check"]);
    assert_eq!(git(&upstream, &["rev-parse", "HEAD"]), next);
    assert_eq!(git(&upstream, &["status", "--porcelain"]), "");
    assert!(fixture.ok(&["prepare"]).contains("already prepared"));
    assert!(fixture.source().join("upstream-news").is_file());
    assert!(
        fs::read_to_string(fixture.source().join("src/lib.rs"))
            .unwrap()
            .contains("true")
    );
}

#[test]
fn explicit_worktree_cache_is_read_only_when_revision_is_missing_online() {
    let fixture = Fixture::new();
    let missing = "1111111111111111111111111111111111111111";
    write(
        &fixture.root,
        "patches/reedline/source.toml",
        &format!(
            "repository = \"https://github.com/nushell/reedline.git\"\nrevision = \"{missing}\"\n"
        ),
    );
    git(
        &fixture.root,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("160000,{missing},third_party/reedline-upstream"),
        ],
    );
    let original_config = fs::read(fixture.upstream.join(".git/config")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_nosh-source"))
        .args(["prepare", "--root"])
        .arg(&fixture.root)
        .arg("--cache")
        .arg(&fixture.upstream)
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("read-only"));
    assert_eq!(git(&fixture.upstream, &["status", "--porcelain"]), "");
    assert_eq!(
        original_config,
        fs::read(fixture.upstream.join(".git/config")).unwrap()
    );
    assert!(!fixture.upstream.join("HEAD").exists());
    assert!(!fixture.upstream.join("config").exists());
    assert!(!fixture.source().exists());
}

#[test]
fn clean_crlf_upstream_checkout_is_not_mistaken_for_local_edits() {
    let fixture = Fixture::new();
    let upstream = fixture.root.join("third_party/reedline-upstream");
    fs::create_dir_all(upstream.parent().unwrap()).unwrap();
    git(
        &fixture.root,
        &[
            "clone",
            "--quiet",
            "--config",
            "core.autocrlf=true",
            fixture.upstream.to_str().unwrap(),
            upstream.to_str().unwrap(),
        ],
    );
    assert!(
        fs::read_to_string(upstream.join("src/lib.rs"))
            .unwrap()
            .contains("\r\n")
    );
    fixture.ok(&["prepare"]);
    fixture.ok(&["check"]);
    fixture.ok(&["export"]);
    assert!(fixture.ok(&["prepare"]).contains("already prepared"));
    assert!(
        !fs::read_to_string(fixture.source().join("src/lib.rs"))
            .unwrap()
            .contains("\r\n")
    );
}

#[test]
fn cargo_alias_bootstraps_before_workspace_dependency_exists() {
    let fixture = Fixture::new();
    write(
        &fixture.root,
        ".cargo/config.toml",
        include_str!("../../../.cargo/config.toml"),
    );
    write(
        &fixture.root,
        "tools/source/Cargo.toml",
        "[package]\nname=\"source-alias-probe\"\nversion=\"0.0.0\"\nedition=\"2024\"\n[workspace]\n",
    );
    write(
        &fixture.root,
        "tools/source/src/main.rs",
        r#"fn main() {
    let status = std::process::Command::new(std::env::var_os("NOSH_SOURCE_TEST_BIN").unwrap())
        .args(std::env::args_os().skip(1)).status().unwrap();
    std::process::exit(status.code().unwrap_or(1));
}
"#,
    );
    let command = |args: &[&str]| {
        Command::new("cargo")
            .args(args)
            .current_dir(&fixture.root)
            .env("NOSH_SOURCE_TEST_BIN", env!("CARGO_BIN_EXE_nosh-source"))
            .env(
                "CARGO_TARGET_DIR",
                fixture._temporary.path().join("alias-target"),
            )
            .output()
            .unwrap()
    };
    let lock = command(&[
        "generate-lockfile",
        "--offline",
        "--manifest-path",
        "tools/source/Cargo.toml",
    ]);
    assert!(
        lock.status.success(),
        "{}",
        String::from_utf8_lossy(&lock.stderr)
    );
    let help = command(&["--offline", "source", "--help"]);
    assert!(
        help.status.success(),
        "{}",
        String::from_utf8_lossy(&help.stderr)
    );
    assert!(String::from_utf8_lossy(&help.stdout).contains("cargo source"));
    assert!(!fixture.root.join(".nosh").exists());
    let args = [
        "--offline",
        "source",
        "prepare",
        "--offline",
        "--root",
        fixture.root.to_str().unwrap(),
        "--cache",
        fixture.upstream.to_str().unwrap(),
    ];
    let first = command(&args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let repeated = command(&args);
    assert!(
        repeated.status.success(),
        "{}",
        String::from_utf8_lossy(&repeated.stderr)
    );
    assert!(String::from_utf8_lossy(&repeated.stdout).contains("already prepared"));
    fixture.ok(&["check"]);
}
