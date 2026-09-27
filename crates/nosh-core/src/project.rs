//! Bounded, read-only project facts for the current directory, not task instructions.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Read;
use std::path::Path;

use nosh_permissions::{Context, PathClass, classify_path, classify_path_real, real_path};
use serde_json::{Value, json};

const MAX_ANCESTORS: usize = 32;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;
const MARKERS: &[(&str, &str)] = &[
    ("Cargo.toml", "rust"),
    ("package.json", "node"),
    ("pyproject.toml", "python"),
    ("setup.cfg", "python"),
    ("setup.py", "python"),
    ("requirements.txt", "python"),
    ("go.mod", "go"),
    ("pom.xml", "maven"),
    ("build.gradle", "gradle"),
    ("build.gradle.kts", "gradle"),
];
const BUILD_MARKERS: &[(&str, &str)] = &[
    ("CMakeLists.txt", "cmake"),
    ("GNUmakefile", "make"),
    ("Makefile", "make"),
    ("makefile", "make"),
];

fn warn(warnings: &mut Vec<String>, message: String) {
    if warnings.len() < 8 {
        warnings.push(message);
    } else if warnings.len() == 8 {
        warnings.push("Additional metadata warnings omitted.".into());
    }
}

fn file_marker(path: &Path, warnings: &mut Vec<String>) -> bool {
    match fs::symlink_metadata(path) {
        Ok(metadata) => metadata.is_file() || metadata.file_type().is_symlink(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            warn(warnings, format!("Cannot inspect {}: {e}", path.display()));
            false
        }
    }
}

fn protected(path: &Path, ctx: &Context) -> bool {
    matches!(
        classify_path_real(path, ctx, true).0,
        PathClass::Protected(_)
    )
}

fn read_metadata(path: &Path, ctx: &Context, warnings: &mut Vec<String>) -> Option<String> {
    let lexical = classify_path(path, ctx);
    let resolved = real_path(path, true).unwrap_or_else(|| path.to_path_buf());
    let class = classify_path_real(&resolved, ctx, true).0;
    if matches!(lexical, PathClass::Protected(_) | PathClass::Null)
        || matches!(class, PathClass::Protected(_) | PathClass::Null)
    {
        warn(
            warnings,
            format!("Metadata not read: protected path {}", path.display()),
        );
        return None;
    }
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let read = || -> std::io::Result<String> {
        let file = options.open(resolved)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() > MAX_MANIFEST_BYTES {
            return Err(std::io::Error::other(
                "not a regular metadata file within 64 KiB",
            ));
        }
        let mut bytes = Vec::new();
        file.take(MAX_MANIFEST_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(std::io::Error::other("metadata exceeds 64 KiB"));
        }
        String::from_utf8(bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    };
    match read() {
        Ok(text) => Some(text),
        Err(e) => {
            warn(warnings, format!("Cannot read {}: {e}", path.display()));
            None
        }
    }
}

fn text_field(info: &mut Value, key: &str, value: &str, warnings: &mut Vec<String>) {
    if value.chars().count() <= 256 {
        info[key] = json!(value);
    } else {
        warn(warnings, format!("Oversized project field omitted: {key}"));
    }
}

fn manifest(
    root: &Path,
    file: &str,
    kind: &str,
    ctx: &Context,
    warnings: &mut Vec<String>,
) -> Value {
    let mut info = json!({"file": file, "type": kind});
    if !matches!(file, "Cargo.toml" | "package.json" | "pyproject.toml") {
        return info;
    }
    let Some(text) = read_metadata(&root.join(file), ctx, warnings) else {
        info["metadata"] = json!("unavailable");
        return info;
    };
    if file == "package.json" {
        let value = match serde_json::from_str::<Value>(&text) {
            Ok(v) if v.is_object() => v,
            Ok(_) => {
                warn(warnings, "package.json must contain an object.".into());
                info["metadata"] = json!("invalid");
                return info;
            }
            Err(e) => {
                warn(warnings, format!("Cannot parse package.json: {e}"));
                info["metadata"] = json!("invalid");
                return info;
            }
        };
        for key in ["name", "version", "type", "packageManager"] {
            match value.get(key) {
                Some(Value::String(s)) => {
                    let output = if key == "type" { "module_type" } else { key };
                    text_field(&mut info, output, s, warnings);
                }
                Some(_) => warn(warnings, format!("Invalid package.json field: {key}")),
                None => {}
            }
        }
        if let Some(scripts) = value.get("scripts") {
            if let Some(scripts) = scripts.as_object() {
                let mut names: Vec<_> = scripts
                    .iter()
                    .filter(|(name, command)| name.chars().count() <= 64 && command.is_string())
                    .map(|(name, _)| name.as_str())
                    .collect();
                names.sort_unstable();
                if names.len() != scripts.len() || names.len() > 16 {
                    warn(
                        warnings,
                        "Some script names omitted: invalid or oversized metadata.".into(),
                    );
                }
                names.truncate(16);
                info["scripts"] = json!(names);
            } else {
                warn(warnings, "package.json scripts must be an object.".into());
            }
        } else {
            info["scripts"] = json!([]);
        }
        let mut dependencies = serde_json::Map::new();
        for key in [
            "dependencies",
            "devDependencies",
            "optionalDependencies",
            "peerDependencies",
        ] {
            match value.get(key) {
                None => {
                    dependencies.insert(key.into(), json!(0));
                }
                Some(Value::Object(packages)) => {
                    dependencies.insert(key.into(), json!(packages.len()));
                }
                Some(_) => warn(
                    warnings,
                    format!("Invalid package.json dependency table: {key}"),
                ),
            }
        }
        info["declared_dependency_counts"] = Value::Object(dependencies);
        if value.get("packageManager").is_none() {
            let managers: BTreeSet<_> = [
                ("package-lock.json", "npm"),
                ("npm-shrinkwrap.json", "npm"),
                ("pnpm-lock.yaml", "pnpm"),
                ("yarn.lock", "yarn"),
                ("bun.lock", "bun"),
                ("bun.lockb", "bun"),
            ]
            .into_iter()
            .filter(|(lock, _)| file_marker(&root.join(lock), warnings))
            .map(|(_, manager)| manager)
            .collect();
            info["package_managers"] = if managers.is_empty() {
                json!(["npm"])
            } else {
                json!(managers)
            };
        }
        return info;
    }
    let table = match text.parse::<toml::Table>() {
        Ok(table) => table,
        Err(e) => {
            warn(warnings, format!("Cannot parse {file}: {}", e.message()));
            info["metadata"] = json!("invalid");
            return info;
        }
    };
    let package = if file == "Cargo.toml" {
        info["workspace"] = json!(table.contains_key("workspace"));
        table.get("package")
    } else {
        table
            .get("project")
            .or_else(|| table.get("tool")?.get("poetry"))
    };
    if let Some(package) = package {
        if let Some(package) = package.as_table() {
            for key in ["name", "version", "edition", "requires-python"] {
                match package.get(key) {
                    Some(toml::Value::String(s)) => text_field(&mut info, key, s, warnings),
                    Some(toml::Value::Table(t))
                        if t.get("workspace").and_then(toml::Value::as_bool) == Some(true) =>
                    {
                        info[format!("{key}_inherited")] = json!(true);
                    }
                    Some(_) => warn(warnings, format!("Unsupported {file} field: {key}")),
                    None => {}
                }
            }
        } else {
            warn(warnings, format!("Invalid project table in {file}."));
        }
    }
    info
}

fn git_info(root: Option<&Path>, ctx: &Context, warnings: &mut Vec<String>) -> Value {
    if [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
    ]
    .iter()
    .any(|name| {
        ctx.exported.contains(*name) && ctx.variables.get(*name).is_some_and(|v| !v.is_empty())
    }) {
        warn(
            warnings,
            "Git environment overrides are active; repository metadata was not inferred.".into(),
        );
        return json!({"status": "unavailable"});
    }
    let Some(root) = root else {
        return json!({"status": "none_detected"});
    };
    let root_display = root.display().to_string();
    let unavailable = json!({"status": "unavailable", "root": root_display});
    let marker = root.join(".git");
    if protected(&marker, ctx) {
        warn(
            warnings,
            "Git metadata is protected; it was not read.".into(),
        );
        return unavailable;
    }
    let gitdir = if marker.is_dir() {
        marker
    } else {
        let Some(text) = read_metadata(&marker, ctx, warnings) else {
            return unavailable;
        };
        let Some(path) = text
            .trim()
            .strip_prefix("gitdir:")
            .map(str::trim)
            .filter(|p| !p.is_empty() && p.len() <= 4096 && !p.contains(['\n', '\r']))
        else {
            warn(warnings, "Invalid .git metadata file.".into());
            return unavailable;
        };
        root.join(path)
    };
    let Some(head) = read_metadata(&gitdir.join("HEAD"), ctx, warnings) else {
        return unavailable;
    };
    let head = head.trim();
    let label = if let Some(branch) = head.strip_prefix("ref: refs/heads/") {
        if branch.is_empty() || branch.chars().count() > 256 || branch.chars().any(char::is_control)
        {
            warn(warnings, "Invalid Git HEAD reference.".into());
            return unavailable;
        }
        branch.to_string()
    } else if matches!(head.len(), 40 | 64) && head.bytes().all(|b| b.is_ascii_hexdigit()) {
        head[..7].to_string()
    } else {
        warn(warnings, "Unrecognized Git HEAD metadata.".into());
        return unavailable;
    };
    let common_marker = gitdir.join("commondir");
    let common = if file_marker(&common_marker, warnings) {
        match read_metadata(&common_marker, ctx, warnings) {
            Some(value)
                if !value.trim().is_empty()
                    && value.trim().len() <= 4096
                    && !value.trim().contains(['\n', '\r']) =>
            {
                Some(gitdir.join(value.trim()))
            }
            _ => {
                warn(
                    warnings,
                    "Git common directory is unavailable; status not queried.".into(),
                );
                return json!({"status": "present", "root": root_display, "head": label, "dirty": null});
            }
        }
    } else {
        None
    };
    let git_paths = [Some(gitdir.clone()), common]
        .into_iter()
        .flatten()
        .map(|path| real_path(&path, true).unwrap_or(path))
        .collect::<Vec<_>>();
    let dirty = if git_paths.iter().any(|dir| {
        protected(dir, ctx)
            || ctx
                .protected
                .iter()
                .any(|p| p.starts_with(dir) || dir.starts_with(p))
    }) || ctx
        .home_dir()
        .is_some_and(|home| protected(&home.join(".gitconfig"), ctx))
    {
        warn(
            warnings,
            "Git status not queried because Git configuration is protected.".into(),
        );
        None
    } else {
        let dirty = crate::prompt::git_dirty(root);
        if dirty.is_none() {
            warn(warnings, "Git working-tree status is unavailable.".into());
        }
        dirty
    };
    json!({"status": "present", "root": root_display, "head": label, "dirty": dirty})
}

fn discover(ctx: &Context) -> Value {
    let mut warnings = Vec::new();
    let mut root = None;
    let mut entries = Vec::new();
    let mut git_root = None;
    let mut git_unknown = false;
    for (depth, dir) in ctx.cwd.ancestors().enumerate() {
        if depth == MAX_ANCESTORS {
            warn(
                &mut warnings,
                "Project ancestor search limit reached.".into(),
            );
            git_unknown = true;
            break;
        }
        if protected(dir, ctx) {
            warn(
                &mut warnings,
                format!("Project directory is protected: {}", dir.display()),
            );
            git_unknown = true;
            break;
        }
        match fs::symlink_metadata(dir.join(".git")) {
            Ok(_) => git_root = Some(dir.to_path_buf()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                git_unknown = true;
                warn(
                    &mut warnings,
                    format!("Cannot inspect Git metadata in {}: {e}", dir.display()),
                );
            }
        }
        if root.is_none() {
            let mut found: Vec<_> = MARKERS
                .iter()
                .filter(|(file, _)| file_marker(&dir.join(file), &mut warnings))
                .copied()
                .collect();
            if found.is_empty() {
                found = BUILD_MARKERS
                    .iter()
                    .filter(|(file, _)| file_marker(&dir.join(file), &mut warnings))
                    .copied()
                    .collect();
            }
            if !found.is_empty() {
                root = Some(dir.to_path_buf());
                for (file, kind) in found {
                    entries.push(manifest(dir, file, kind, ctx, &mut warnings));
                }
            }
        }
        if git_root.is_some() || ctx.home_dir() == Some(dir) {
            break;
        }
    }
    let status = if root.is_some() {
        "detected"
    } else if warnings.is_empty() {
        "none_detected"
    } else {
        "unavailable"
    };
    let git = if git_root.is_none() && git_unknown {
        json!({"status": "unavailable"})
    } else {
        git_info(git_root.as_deref(), ctx, &mut warnings)
    };
    let types: BTreeSet<_> = entries.iter().filter_map(|v| v["type"].as_str()).collect();
    json!({"status": status, "root": root.map(|p| p.display().to_string()), "types": types, "manifests": entries, "git": git, "warnings": warnings})
}

pub(crate) fn describe(ctx: &Context) -> String {
    format!("[project] {}", discover(ctx))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(home: &Path, cwd: &Path) -> Context {
        Context::new(cwd, home).with_home(home)
    }

    fn git_marker(root: &Path) {
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::write(root.join(".git/HEAD"), "ref: refs/heads/feature/demo\n").unwrap();
    }

    #[test]
    fn ordinary_directories_and_git_only_repositories_are_explicit() {
        let home = tempfile::tempdir().unwrap();
        let plain = home.path().join("plain");
        let repo = home.path().join("repo");
        fs::create_dir_all(&plain).unwrap();
        git_marker(&repo);
        let plain = discover(&context(home.path(), &plain));
        assert_eq!(plain["status"], "none_detected");
        assert_eq!(plain["git"]["status"], "none_detected");
        assert!(plain["root"].is_null());
        let repo = discover(&context(home.path(), &repo));
        assert_eq!(repo["status"], "none_detected");
        assert_eq!(repo["git"]["status"], "present");
        assert_eq!(repo["git"]["head"], "feature/demo");
        assert!(repo["git"]["dirty"].is_null());
    }

    #[test]
    fn nearest_project_wins_without_crossing_a_git_boundary() {
        let home = tempfile::tempdir().unwrap();
        let outer = home.path().join("outer");
        let node = outer.join("frontend");
        let nested = node.join("src");
        fs::create_dir_all(&nested).unwrap();
        git_marker(&outer);
        fs::write(outer.join("Cargo.toml"), "[package]\nname='outer'\n").unwrap();
        fs::write(
            node.join("package.json"),
            r#"{"name":"frontend","scripts":{"build":"touch must-not-run","test":"node --test"}}"#,
        )
        .unwrap();
        let value = discover(&context(home.path(), &nested));
        assert_eq!(value["types"], json!(["node"]));
        assert_eq!(value["root"], json!(node));
        assert_eq!(value["git"]["root"], json!(outer));
        assert_eq!(value["manifests"][0]["scripts"], json!(["build", "test"]));
        assert_eq!(
            value["manifests"][0]["declared_dependency_counts"]["dependencies"],
            0
        );
        assert!(!value.to_string().contains("must-not-run"));
        assert!(!node.join("must-not-run").exists());

        let separate = outer.join("separate");
        git_marker(&separate);
        let value = discover(&context(home.path(), &separate));
        assert_eq!(value["status"], "none_detected");
        assert_eq!(value["git"]["root"], json!(separate));
    }

    #[test]
    fn rust_python_and_manifest_changes_are_detected_without_a_cache() {
        let home = tempfile::tempdir().unwrap();
        let rust = home.path().join("rust");
        let python = home.path().join("python");
        fs::create_dir_all(&rust).unwrap();
        fs::create_dir_all(&python).unwrap();
        fs::write(
            rust.join("Cargo.toml"),
            "[package]\nname='demo'\nversion.workspace=true\nedition='2024'\n[workspace]\n",
        )
        .unwrap();
        fs::write(
            python.join("pyproject.toml"),
            "[project]\nname='calculator'\nrequires-python='>=3.11'\n",
        )
        .unwrap();
        let value = discover(&context(home.path(), &rust));
        assert_eq!(value["types"], json!(["rust"]));
        assert_eq!(value["manifests"][0]["name"], "demo");
        assert_eq!(value["manifests"][0]["version_inherited"], true);
        assert_eq!(value["manifests"][0]["workspace"], true);
        let value = discover(&context(home.path(), &python));
        assert_eq!(value["types"], json!(["python"]));
        assert_eq!(value["manifests"][0]["requires-python"], ">=3.11");
        fs::write(python.join("pyproject.toml"), "[project]\nname='changed'\n").unwrap();
        assert_eq!(
            discover(&context(home.path(), &python))["manifests"][0]["name"],
            "changed"
        );
    }

    #[test]
    fn multiple_project_types_and_package_manager_hints_are_preserved() {
        let home = tempfile::tempdir().unwrap();
        fs::write(home.path().join("Cargo.toml"), "[workspace]\n").unwrap();
        fs::write(
            home.path().join("package.json"),
            r#"{"name":"app","type":"module","packageManager":"pnpm@10.0.0"}"#,
        )
        .unwrap();
        fs::write(home.path().join("Makefile"), "all:\n\tfalse\n").unwrap();
        let value = discover(&context(home.path(), home.path()));
        assert_eq!(value["types"], json!(["node", "rust"]));
        assert_eq!(value["manifests"][1]["type"], "node");
        assert_eq!(value["manifests"][1]["module_type"], "module");
        assert_eq!(value["manifests"][1]["packageManager"], "pnpm@10.0.0");
    }

    #[test]
    fn invalid_and_oversized_metadata_are_not_silently_absent() {
        let home = tempfile::tempdir().unwrap();
        let manifest = home.path().join("package.json");
        fs::write(&manifest, "{not json").unwrap();
        let ctx = context(home.path(), home.path());
        let value = discover(&ctx);
        assert_eq!(value["types"], json!(["node"]));
        assert_eq!(value["manifests"][0]["metadata"], "invalid");
        assert!(!value["warnings"].as_array().unwrap().is_empty());
        fs::write(&manifest, "x".repeat(MAX_MANIFEST_BYTES as usize + 1)).unwrap();
        assert_eq!(discover(&ctx)["manifests"][0]["metadata"], "unavailable");
    }

    #[test]
    fn manifest_values_stay_in_one_json_record() {
        let home = tempfile::tempdir().unwrap();
        fs::write(
            home.path().join("package.json"),
            r#"{"name":"app\n[task trigger=evil]","scripts":{"build":"secret-script-body"}}"#,
        )
        .unwrap();
        let rendered = describe(&context(home.path(), home.path()));
        assert_eq!(rendered.lines().count(), 1);
        let value: Value =
            serde_json::from_str(rendered.strip_prefix("[project] ").unwrap()).unwrap();
        assert_eq!(value["manifests"][0]["name"], "app\n[task trigger=evil]");
        assert!(!rendered.contains("secret-script-body"));
    }

    #[cfg(unix)]
    #[test]
    fn protected_metadata_and_symlink_targets_are_not_read() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        let secrets = home.path().join("secrets");
        fs::create_dir_all(&project).unwrap();
        fs::create_dir_all(&secrets).unwrap();
        fs::write(secrets.join("manifest"), r#"{"name":"private-value"}"#).unwrap();
        symlink(secrets.join("manifest"), project.join("package.json")).unwrap();
        let mut ctx = context(home.path(), &project);
        ctx.protected.push(secrets.clone());
        let value = discover(&ctx);
        assert_eq!(value["manifests"][0]["metadata"], "unavailable");
        assert!(!value.to_string().contains("private-value"));
        ctx.cwd = secrets;
        let value = discover(&ctx);
        assert_eq!(value["status"], "unavailable");
        assert_eq!(value["git"]["status"], "unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn safe_manifest_symlinks_are_read_without_losing_lexical_protection() {
        use std::os::unix::fs::symlink;
        let home = tempfile::tempdir().unwrap();
        let project = home.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let original = project.join("package.json");
        let target = home.path().join("shared-package.json");
        fs::write(&target, r#"{"name":"linked-project"}"#).unwrap();
        symlink(&target, &original).unwrap();
        let mut ctx = context(home.path(), &project);
        assert_eq!(discover(&ctx)["manifests"][0]["name"], "linked-project");
        ctx.protected.push(original);
        let value = discover(&ctx);
        assert_eq!(value["manifests"][0]["metadata"], "unavailable");
        assert!(!value.to_string().contains("linked-project"));
    }

    #[test]
    fn relative_git_metadata_files_are_supported() {
        let home = tempfile::tempdir().unwrap();
        let worktree = home.path().join("worktree");
        let metadata = home.path().join("metadata");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&metadata).unwrap();
        fs::write(worktree.join(".git"), "gitdir: ../metadata\n").unwrap();
        fs::write(metadata.join("HEAD"), "ref: refs/heads/worktree-branch\n").unwrap();
        let value = discover(&context(home.path(), &worktree));
        assert_eq!(value["git"]["root"], json!(worktree));
        assert_eq!(value["git"]["head"], "worktree-branch");
    }

    #[test]
    fn git_tracking_status_refreshes_without_counting_untracked_files() {
        use std::process::Command;
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet", "--initial-branch=main", "--template="])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        fs::write(root.join("file.txt"), "content").unwrap();
        let ctx = context(home.path(), &root);
        let value = discover(&ctx);
        assert_eq!(value["status"], "none_detected");
        assert_eq!(value["git"]["head"], "main");
        assert_eq!(value["git"]["dirty"], false);
        assert!(
            Command::new("git")
                .args(["add", "file.txt"])
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        assert_eq!(discover(&ctx)["git"]["dirty"], true);
    }

    #[cfg(unix)]
    #[test]
    fn automatic_git_status_does_not_run_fsmonitor_hooks() {
        use std::os::unix::fs::PermissionsExt;
        use std::process::Command;
        let home = tempfile::tempdir().unwrap();
        let root = home.path().join("repo");
        fs::create_dir_all(&root).unwrap();
        assert!(
            Command::new("git")
                .args(["init", "--quiet", "--initial-branch=main", "--template="])
                .arg(&root)
                .status()
                .unwrap()
                .success()
        );
        let hook = root.join("monitor");
        fs::write(
            &hook,
            "#!/bin/sh\nprintf ran > \"$(dirname \"$0\")/fsmonitor-ran\"\n",
        )
        .unwrap();
        fs::set_permissions(&hook, fs::Permissions::from_mode(0o700)).unwrap();
        assert!(
            Command::new("git")
                .args(["config", "core.fsmonitor"])
                .arg(&hook)
                .current_dir(&root)
                .status()
                .unwrap()
                .success()
        );
        let value = discover(&context(home.path(), &root));
        assert_eq!(value["git"]["dirty"], false);
        assert!(!root.join("fsmonitor-ran").exists());
    }

    #[test]
    fn git_overrides_and_protected_worktree_configuration_remain_unknown() {
        let home = tempfile::tempdir().unwrap();
        let worktree = home.path().join("worktree");
        let common = home.path().join("common");
        let metadata = common.join("worktrees/example");
        fs::create_dir_all(&worktree).unwrap();
        fs::create_dir_all(&metadata).unwrap();
        fs::write(
            worktree.join(".git"),
            "gitdir: ../common/worktrees/example\n",
        )
        .unwrap();
        fs::write(metadata.join("HEAD"), "ref: refs/heads/example\n").unwrap();
        fs::write(metadata.join("commondir"), "../..\n").unwrap();
        let mut ctx = context(home.path(), &worktree);
        ctx.protected.push(common.join("config"));
        let value = discover(&ctx);
        assert_eq!(value["git"]["head"], "example");
        assert!(value["git"]["dirty"].is_null());
        assert!(
            value["warnings"]
                .to_string()
                .contains("configuration is protected")
        );
        ctx.exported.insert("GIT_DIR".into());
        ctx.variables.insert("GIT_DIR".into(), "/somewhere".into());
        assert_eq!(discover(&ctx)["git"]["status"], "unavailable");
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_directory_names_do_not_panic_during_serialization() {
        use std::ffi::OsString;
        use std::os::unix::ffi::OsStringExt;
        let home = tempfile::tempdir().unwrap();
        let dir = home
            .path()
            .join(OsString::from_vec(b"project-\xff".to_vec()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Cargo.toml"), "[workspace]\n").unwrap();
        let value = discover(&context(home.path(), &dir));
        assert_eq!(value["types"], json!(["rust"]));
        assert!(value["root"].as_str().is_some());
    }
}
