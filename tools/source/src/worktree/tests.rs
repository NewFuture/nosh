use super::*;
use std::fs::File;

fn patch_fixture() -> (tempfile::TempDir, PathBuf) {
    let temporary = tempfile::tempdir().unwrap();
    let dir = temporary.path();
    git(dir, &["init", "--quiet"]).unwrap();
    git(dir, &["config", "core.autocrlf", "false"]).unwrap();
    git(dir, &["config", "user.name", "source test"]).unwrap();
    git(
        dir,
        &["config", "user.email", "source-test@example.invalid"],
    )
    .unwrap();
    fs::write(dir.join("README.md"), "alpha\n").unwrap();
    git(dir, &["add", "README.md"]).unwrap();
    git(dir, &["commit", "--quiet", "-m", "fixture"]).unwrap();
    fs::write(dir.join("README.md"), "beta\n").unwrap();
    let patch = dir.join(".git/fixture.patch");
    fs::write(
        &patch,
        git(dir, &["diff", "--binary", "--full-index", "HEAD"]).unwrap(),
    )
    .unwrap();
    fs::write(dir.join("README.md"), "alpha\n").unwrap();
    git(dir, &["update-index", "--really-refresh"]).unwrap();
    (temporary, patch)
}

fn stale_mtime(dir: &Path, sequence: u64) {
    File::options()
        .write(true)
        .open(dir.join("README.md"))
        .unwrap()
        .set_times(
            fs::FileTimes::new()
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_600_000_000 + sequence)),
        )
        .unwrap();
}

#[test]
fn metadata_only_change_is_refreshed_before_indexed_check() {
    let (temporary, patch) = patch_fixture();
    let dir = temporary.path();
    stale_mtime(dir, 0);
    let failure = git_output(dir, &["apply", "--check", "--index", text(&patch).unwrap()]).unwrap();
    assert!(!failure.status.success());
    assert!(String::from_utf8_lossy(&failure.stderr).contains("does not match index"));
    apply_indexed_patch(dir, &patch, |args| git_output(dir, args)).unwrap();
    assert_eq!(fs::read_to_string(dir.join("README.md")).unwrap(), "beta\n");
}

#[test]
fn metadata_drift_between_check_and_apply_is_retried_with_index() {
    let (temporary, patch) = patch_fixture();
    let dir = temporary.path();
    let mut applications = 0;
    apply_indexed_patch(dir, &patch, |args| {
        assert!(args.contains(&"--index"));
        if !args.contains(&"--check") {
            applications += 1;
            if applications == 1 {
                stale_mtime(dir, 0);
            }
        }
        git_output(dir, args)
    })
    .unwrap();
    assert_eq!(applications, 2);
    assert_eq!(fs::read_to_string(dir.join("README.md")).unwrap(), "beta\n");
}

#[test]
fn real_change_after_precheck_is_preserved_not_retried() {
    let (temporary, patch) = patch_fixture();
    let dir = temporary.path();
    let mut applications = 0;
    let error = apply_indexed_patch(dir, &patch, |args| {
        if !args.contains(&"--check") {
            applications += 1;
            fs::write(dir.join("README.md"), "real developer edit\n")?;
        }
        git_output(dir, args)
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("staging source changed"), "{error}");
    assert_eq!(applications, 1);
    assert_eq!(
        fs::read_to_string(dir.join("README.md")).unwrap(),
        "real developer edit\n"
    );
}

#[test]
fn real_index_change_is_rejected_before_patch_execution() {
    let (temporary, patch) = patch_fixture();
    let dir = temporary.path();
    fs::write(dir.join("README.md"), "real staged edit\n").unwrap();
    git(dir, &["add", "README.md"]).unwrap();
    let error = apply_indexed_patch(dir, &patch, |_| panic!("must not run apply"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("staging index changed"), "{error}");
    assert_eq!(
        fs::read_to_string(dir.join("README.md")).unwrap(),
        "real staged edit\n"
    );
}

#[test]
fn fresh_verification_hashes_edits_with_unchanged_size_and_mtime() {
    let (temporary, patch) = patch_fixture();
    let dir = temporary.path();
    let path = dir.join("README.md");
    let modified = fs::metadata(&path).unwrap().modified().unwrap();
    fs::write(&path, "omega\n").unwrap();
    File::options()
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(fs::FileTimes::new().set_modified(modified))
        .unwrap();
    let error = apply_indexed_patch(dir, &patch, |_| panic!("must not run apply"))
        .unwrap_err()
        .to_string();
    assert!(error.contains("staging source changed"), "{error}");
    assert_eq!(fs::read_to_string(path).unwrap(), "omega\n");
}

#[test]
fn patch_conflicts_fail_without_retry_or_modification() {
    let (temporary, patch) = patch_fixture();
    let dir = temporary.path();
    let conflicting = fs::read_to_string(&patch)
        .unwrap()
        .replace("-alpha", "-not-the-base");
    fs::write(&patch, conflicting).unwrap();
    let mut checks = 0;
    let error = apply_indexed_patch(dir, &patch, |args| {
        assert!(args.contains(&"--check"));
        checks += 1;
        git_output(dir, args)
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("patch does not apply"), "{error}");
    assert_eq!(checks, 1);
    assert_eq!(
        fs::read_to_string(dir.join("README.md")).unwrap(),
        "alpha\n"
    );
}

#[test]
fn repeated_metadata_drift_has_a_fixed_retry_limit() {
    let (temporary, patch) = patch_fixture();
    let dir = temporary.path();
    let mut applications = 0;
    let error = apply_indexed_patch(dir, &patch, |args| {
        assert!(args.contains(&"--index"));
        if !args.contains(&"--check") {
            applications += 1;
            stale_mtime(dir, applications);
        }
        git_output(dir, args)
    })
    .unwrap_err()
    .to_string();
    assert!(error.contains("after 4 attempt(s)"), "{error}");
    assert_eq!(applications, 4);
    assert_eq!(
        fs::read_to_string(dir.join("README.md")).unwrap(),
        "alpha\n"
    );
}

#[test]
fn failed_directory_installation_reports_paths_and_preserves_source() {
    let temporary = tempfile::tempdir().unwrap();
    let from = temporary.path().join("candidate");
    fs::create_dir(&from).unwrap();
    fs::write(from.join("local-edit"), "preserve me").unwrap();
    let occupied = temporary.path().join("occupied");
    fs::create_dir(&occupied).unwrap();
    assert!(
        rename_directory(&from, &occupied)
            .unwrap_err()
            .to_string()
            .contains("refusing")
    );
    let missing_parent = temporary.path().join("missing/live");
    let error = rename_directory(&from, &missing_parent)
        .unwrap_err()
        .to_string();
    assert!(error.contains(&from.display().to_string()));
    assert!(error.contains(&missing_parent.display().to_string()));
    assert_eq!(
        fs::read_to_string(from.join("local-edit")).unwrap(),
        "preserve me"
    );
    let live = temporary.path().join("live");
    rename_directory(&from, &live).unwrap();
    assert_eq!(
        fs::read_to_string(live.join("local-edit")).unwrap(),
        "preserve me"
    );
}
