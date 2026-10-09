use crate::Result;
use std::{
    env, fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const MAX_ATTEMPTS: u32 = 4;

pub(crate) fn text(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| "path is not UTF-8".into())
}

fn command(dir: &Path) -> Command {
    let mut command = Command::new(env::var_os("NOSH_GIT").unwrap_or_else(|| "git".into()));
    if !dir.join(".git").exists() && dir.join("HEAD").is_file() && dir.join("objects").is_dir() {
        command.arg("--git-dir").arg(dir);
    }
    command
        .arg("-C")
        .arg(dir)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env("LC_ALL", "C");
    command
}

fn checked(dir: &Path, args: &[&str], output: Output) -> Result<Vec<u8>> {
    if !output.status.success() {
        return Err(format!(
            "git {} in {} failed: {}{}",
            args.join(" "),
            dir.display(),
            String::from_utf8_lossy(&output.stderr),
            String::from_utf8_lossy(&output.stdout)
        )
        .into());
    }
    Ok(output.stdout)
}

pub(crate) fn git_output(dir: &Path, args: &[&str]) -> Result<Output> {
    Ok(command(dir).args(args).output()?)
}

pub(crate) fn git(dir: &Path, args: &[&str]) -> Result<Vec<u8>> {
    checked(dir, args, git_output(dir, args)?)
}

pub(crate) fn git_text(dir: &Path, args: &[&str]) -> Result<String> {
    Ok(String::from_utf8(git(dir, args)?)?.trim().to_owned())
}

pub(crate) fn has_commit(dir: &Path, revision: &str) -> Result<bool> {
    Ok(
        git_output(dir, &["cat-file", "-e", &format!("{revision}^{{commit}}")])?
            .status
            .success(),
    )
}

pub(crate) fn unique(parent: &Path, name: &str) -> PathBuf {
    parent.join(format!(
        "{name}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

pub(crate) fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let temporary = unique(path.parent().ok_or("file has no parent")?, "write");
    fs::write(&temporary, bytes)?;
    fs::rename(&temporary, path)?;
    Ok(())
}

fn retry_delay(attempt: u32) {
    std::thread::sleep(Duration::from_millis(25 << attempt));
}

pub(crate) fn rename_directory(from: &Path, to: &Path) -> Result<()> {
    for attempt in 1..=MAX_ATTEMPTS {
        if to.try_exists()? {
            return Err(format!("refusing to replace existing directory {}", to.display()).into());
        }
        match fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(error) if attempt < MAX_ATTEMPTS && matches!(error.kind(),
                std::io::ErrorKind::PermissionDenied | std::io::ErrorKind::WouldBlock) => {
                retry_delay(attempt);
            }
            Err(error) => return Err(format!(
                "cannot rename {} -> {} after {attempt} attempt(s): {error}; inspect the retained source before retrying",
                from.display(), to.display()
            ).into()),
        }
    }
    unreachable!()
}

#[derive(Clone, Copy)]
enum Snapshot {
    Editable,
    Exact,
}

pub(crate) fn snapshot(dir: &Path) -> Result<String> {
    snapshot_worktree(dir, Snapshot::Editable)
}

fn snapshot_worktree(dir: &Path, mode: Snapshot) -> Result<String> {
    let index = unique(&dir.join(".git"), "nosh-index");
    let run = |args: &[&str]| -> Result<Vec<u8>> {
        let output = command(dir)
            .args([
                "-c",
                match mode {
                    Snapshot::Editable => "core.autocrlf=input",
                    Snapshot::Exact => "core.autocrlf=false",
                },
                "-c",
                "core.safecrlf=false",
                "-c",
                "core.fsmonitor=false",
            ])
            .env("GIT_INDEX_FILE", &index)
            .args(args)
            .output()?;
        checked(dir, args, output)
    };
    let result = (|| {
        // A fresh index forces hashing even when file size and timestamps are unchanged.
        run(&["read-tree", "HEAD"])?;
        let mut args = vec!["add", "--force", "--all", "--", "."];
        if matches!(mode, Snapshot::Editable) {
            args.push(":(exclude)target");
        }
        run(&args)?;
        Ok(String::from_utf8(run(&["write-tree"])?)?.trim().to_owned())
    })();
    if index.exists() {
        fs::remove_file(index)?;
    }
    result
}

fn verify_staging(dir: &Path, expected: &str) -> Result<()> {
    if git_text(dir, &["write-tree"])? != expected {
        return Err("staging index changed; refusing to refresh or retry the patch".into());
    }
    if snapshot_worktree(dir, Snapshot::Exact)? != expected {
        return Err("staging source changed; refusing to refresh or retry the patch".into());
    }
    Ok(())
}

pub(crate) fn apply_indexed_patch(
    dir: &Path,
    patch: &Path,
    mut apply: impl FnMut(&[&str]) -> Result<Output>,
) -> Result<()> {
    let expected = git_text(dir, &["rev-parse", "HEAD^{tree}"])?;
    for attempt in 1..=MAX_ATTEMPTS {
        verify_staging(dir, &expected)?;
        git(dir, &["update-index", "--really-refresh"])?;
        let mut output = apply(&["apply", "--check", "--index", text(patch)?])?;
        if output.status.success() {
            git(dir, &["update-index", "--really-refresh"])?;
            output = apply(&["apply", "--index", text(patch)?])?;
        }
        if output.status.success() {
            return Ok(());
        }
        let errors = String::from_utf8_lossy(&output.stderr);
        let metadata_only = !errors.trim().is_empty()
            && errors.lines().all(|line| {
                line.starts_with("error: ") && line.ends_with(": does not match index")
            });
        if !metadata_only || attempt == MAX_ATTEMPTS {
            return Err(format!(
                "indexed patch application in {} failed after {attempt} attempt(s): {errors}{}",
                dir.display(),
                String::from_utf8_lossy(&output.stdout)
            )
            .into());
        }
        verify_staging(dir, &expected)?;
        eprintln!(
            "nosh-source: verified unchanged staging content/index at {}; retrying stale Git stat metadata ({attempt}/{})",
            dir.display(),
            MAX_ATTEMPTS - 1
        );
        retry_delay(attempt);
    }
    unreachable!()
}

#[cfg(test)]
mod tests;
