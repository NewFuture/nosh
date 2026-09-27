//! Scoped AGENTS.md guidance, separate from project facts and reference documents.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use nosh_hub::store::FileStamp;
use nosh_permissions::{Context, PathClass, classify_path_real};
use serde_json::json;

const GUIDANCE_CHARS: usize = 4000;
const MAX_ANCESTORS: usize = 32;
const README_CHARS: usize = 1000;
const READMES: &[&str] = &[
    "README.md",
    "Readme.md",
    "readme.md",
    "README.rst",
    "README.txt",
    "README",
];

#[derive(Default)]
pub(crate) struct GuidanceCache {
    files: HashMap<PathBuf, (FileStamp, String)>,
}

pub(crate) struct Guidance {
    pub key: String,
    pub text: String,
    pub complete: bool,
}

impl GuidanceCache {
    fn read_cached(
        &mut self,
        path: &Path,
        ctx: &Context,
        warnings: &mut Vec<String>,
    ) -> (Option<FileStamp>, Option<String>) {
        let stamp = FileStamp::of(path);
        if matches!(
            classify_path_real(path, ctx, true).0,
            PathClass::Protected(_)
        ) {
            self.files.remove(path);
            warnings.push(format!(
                "Project document not read: protected path {}",
                path.display()
            ));
            return (stamp, None);
        }
        if let Some((_, text)) = self
            .files
            .get(path)
            .filter(|(before, _)| stamp.as_ref() == Some(before))
        {
            return (stamp, Some(text.clone()));
        }
        let contents = crate::project::read_metadata(path, ctx, warnings);
        if let Some(ref contents) = contents {
            let after = FileStamp::of(path);
            if after != stamp {
                warnings.push(format!(
                    "Project document changed while reading: {}",
                    path.display()
                ));
                self.files.remove(path);
                return (after, None);
            }
            if let Some(ref stamp) = after {
                self.files
                    .insert(path.to_path_buf(), (stamp.clone(), contents.clone()));
            }
        }
        (stamp, contents)
    }

    fn readme(&mut self, path: &Path, ctx: &Context) -> Guidance {
        self.files.retain(|cached, _| cached == path);
        let mut warnings = Vec::new();
        let (stamp, contents) = self.read_cached(path, ctx, &mut warnings);
        let mut key = format!("README:{path:?}:{stamp:?}");
        let mut text = format!("[README reference {}]\n", json!(path.display().to_string()));
        if let Some(contents) = contents {
            if stamp.is_none() {
                key.push_str(&contents);
            }
            text.push_str(&readme_excerpt(&contents));
        }
        if !warnings.is_empty() {
            for warning in &warnings {
                text.push_str(&format!("reference unavailable: {}\n", json!(warning)));
            }
            key.push_str(&format!("{warnings:?}"));
        }
        Guidance {
            key,
            text,
            complete: true,
        }
    }

    pub fn load(&mut self, ctx: &Context) -> Guidance {
        let mut paths = Vec::new();
        let mut directories = Vec::new();
        let mut warnings = Vec::new();
        for (depth, dir) in ctx.cwd.ancestors().enumerate() {
            if depth == MAX_ANCESTORS {
                warnings.push("AGENTS.md ancestor search limit reached.".to_string());
                break;
            }
            if matches!(
                classify_path_real(dir, ctx, true).0,
                PathClass::Protected(_)
            ) {
                warnings.push(format!("AGENTS.md scope is protected: {}", dir.display()));
                break;
            }
            directories.push(dir.to_path_buf());
            let path = dir.join("AGENTS.md");
            match std::fs::symlink_metadata(&path) {
                Ok(_) => paths.push(path),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => warnings.push(format!("Cannot inspect {}: {e}", path.display())),
            }
            let git_boundary = match std::fs::symlink_metadata(dir.join(".git")) {
                Ok(_) => true,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
                Err(e) => {
                    warnings.push(format!(
                        "Cannot inspect AGENTS.md scope boundary in {}: {e}",
                        dir.display()
                    ));
                    true
                }
            };
            if git_boundary || ctx.home_dir() == Some(dir) {
                break;
            }
        }
        if paths.is_empty() && warnings.is_empty() {
            for directory in directories {
                for name in READMES {
                    let candidate = directory.join(name);
                    match std::fs::symlink_metadata(&candidate) {
                        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
                            return self.readme(&candidate, ctx);
                        }
                        Ok(_) => {}
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                        Err(_) => return self.readme(&candidate, ctx),
                    }
                }
            }
        }
        paths.reverse();
        self.files.retain(|path, _| paths.contains(path));
        let mut key = String::new();
        let mut text = String::new();
        let mut budget = GUIDANCE_CHARS;
        let mut all_complete = true;
        for path in paths {
            let (stamp, contents) = self.read_cached(&path, ctx, &mut warnings);
            key.push_str(&format!("{path:?}:{stamp:?}\n"));
            let Some(contents) = contents else { continue };
            if stamp.is_none() {
                key.push_str(&contents);
            }
            let count = contents.chars().count();
            let complete = count <= budget;
            all_complete &= complete;
            text.push_str(&format!(
                "[AGENTS.md {}{}]\n",
                json!(path.display().to_string()),
                if complete { "" } else { " not loaded" },
            ));
            if complete {
                text.push_str(contents.trim_end());
                text.push('\n');
                budget -= count;
            } else {
                text.push_str("Size limit. Read before acting in this scope.\n");
            }
        }
        if !warnings.is_empty() {
            all_complete = false;
            for warning in &warnings {
                text.push_str(&format!("guidance unavailable: {}\n", json!(warning)));
            }
            key.push_str(&format!("{warnings:?}"));
        }
        Guidance {
            key,
            text,
            complete: all_complete,
        }
    }
}

fn readme_excerpt(text: &str) -> String {
    let mut intro = Vec::new();
    let mut headings = Vec::new();
    let mut fence: Option<(u8, usize)> = None;
    let mut intro_chars = 0;
    let mut intro_done = false;
    let mut truncated = false;
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        let marker = line.as_bytes().first().copied();
        let marker_len = line.bytes().take_while(|b| Some(*b) == marker).count();
        if let Some((opening, length)) = fence {
            if marker == Some(opening)
                && marker_len >= length
                && line[marker_len..].trim().is_empty()
            {
                fence = None;
            }
            continue;
        }
        if matches!(marker, Some(b'`' | b'~')) && marker_len >= 3 {
            intro_done |= !intro.is_empty();
            fence = marker.map(|marker| (marker, marker_len));
            continue;
        }
        if line.is_empty() {
            intro_done |= !intro.is_empty();
            continue;
        }
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        if (1..=6).contains(&hashes)
            && line
                .as_bytes()
                .get(hashes)
                .is_some_and(u8::is_ascii_whitespace)
        {
            intro_done |= !intro.is_empty();
            if headings.len() < 8 {
                headings.push(format!(
                    "L{}: {}",
                    index + 1,
                    line.trim_start_matches('#').trim()
                ));
            } else {
                truncated = true;
            }
            continue;
        }
        if !intro_done
            && !["![", "[![", "<!--", "<", "[!"]
                .iter()
                .any(|prefix| line.starts_with(*prefix))
        {
            if intro.len() < 4 && intro_chars < 600 {
                let content: String = line.chars().take(600 - intro_chars).collect();
                let count = content.chars().count();
                truncated |= count < line.chars().count();
                intro_chars += count;
                intro.push(format!("L{}: {content}", index + 1));
            } else {
                truncated = true;
                intro_done = true;
            }
        }
    }
    let mut body = intro.join("\n");
    if !headings.is_empty() {
        body.push_str("\nSections:\n");
        body.push_str(&headings.join("\n"));
    }
    truncated |= body.chars().count() > README_CHARS;
    let body: String = body.chars().take(README_CHARS).collect();
    if truncated {
        format!("{body}\n[excerpt truncated]\n")
    } else {
        format!("{body}\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agents_take_precedence_over_readme_and_legacy_names() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("NOSH.md"), "legacy instructions").unwrap();
        std::fs::write(root.path().join("README.md"), "readme instructions").unwrap();
        let ctx = Context::new(root.path(), root.path()).with_home(root.path());
        let mut cache = GuidanceCache::default();
        let fallback = cache.load(&ctx);
        assert!(fallback.text.contains("README reference"));
        assert!(fallback.text.contains("readme instructions"));
        assert!(!fallback.text.contains("legacy instructions"));
        assert!(!fallback.text.contains("No AGENTS.md"));
        assert!(!fallback.text.contains("read the relevant"));
        std::fs::write(root.path().join("AGENTS.md"), "Use project conventions.").unwrap();
        let snapshot = cache.load(&ctx);
        assert!(snapshot.complete);
        assert!(snapshot.text.contains("Use project conventions."));
        assert!(!snapshot.text.contains("legacy instructions"));
        assert!(!snapshot.text.contains("readme instructions"));
        std::fs::remove_file(root.path().join("AGENTS.md")).unwrap();
        let again = cache.load(&ctx);
        assert!(again.text.contains("README reference"));
        assert_ne!(again.key, snapshot.key);
    }

    #[test]
    fn readme_fallback_is_a_bounded_reference_and_not_a_script() {
        let root = tempfile::tempdir().unwrap();
        let content = format!(
            "# Project\n\nA small project.\n\n## Usage\n```sh\necho should-not-be-injected\n```\n\n## Development\n{}\n",
            "details ".repeat(2000)
        );
        std::fs::write(root.path().join("README.md"), content).unwrap();
        let ctx = Context::new(root.path(), root.path()).with_home(root.path());
        let mut cache = GuidanceCache::default();
        let fallback = cache.load(&ctx);
        assert!(fallback.complete);
        assert!(fallback.text.contains("A small project."));
        assert!(fallback.text.contains("Sections:"));
        assert!(!fallback.text.contains("should-not-be-injected"));
        assert!(!fallback.text.contains("details"));
        assert!(fallback.text.chars().count() < README_CHARS + 600);
        std::fs::write(
            root.path().join("AGENTS.md"),
            "x".repeat(GUIDANCE_CHARS + 1),
        )
        .unwrap();
        let blocked = cache.load(&ctx);
        assert!(!blocked.complete);
        assert!(!blocked.text.contains("README reference"));
    }

    #[test]
    fn readme_unavailability_is_not_missing_agent_guidance() {
        let root = tempfile::tempdir().unwrap();
        let readme = root.path().join("README.md");
        std::fs::write(&readme, "private reference text").unwrap();
        let mut ctx = Context::new(root.path(), root.path()).with_home(root.path());
        ctx.protected.push(readme);
        let snapshot = GuidanceCache::default().load(&ctx);
        assert!(snapshot.complete);
        assert!(snapshot.text.contains("reference unavailable"));
        assert!(!snapshot.text.contains("private reference text"));
    }

    #[test]
    fn ancestor_guidance_takes_precedence_over_nearer_readme() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("src");
        std::fs::create_dir(&child).unwrap();
        let agents = root.path().join("AGENTS.md");
        std::fs::write(&agents, "ancestor instruction").unwrap();
        std::fs::write(child.join("README.md"), "nearer reference").unwrap();
        let mut ctx = Context::new(&child, root.path()).with_home(root.path());
        let mut cache = GuidanceCache::default();
        let guidance = cache.load(&ctx);
        assert!(guidance.complete);
        assert!(guidance.text.contains("ancestor instruction"));
        assert!(!guidance.text.contains("nearer reference"));

        ctx.protected.push(agents);
        let blocked = cache.load(&ctx);
        assert!(!blocked.complete);
        assert!(blocked.text.contains("protected path"));
        assert!(!blocked.text.contains("README reference"));
    }

    #[test]
    fn nearest_readme_is_refreshed_without_crossing_git_boundary() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        let child = repo.join("src");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(repo.join(".git"), "gitdir: elsewhere").unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "outer instruction").unwrap();
        std::fs::write(repo.join("README.md"), "root reference").unwrap();
        let ctx = Context::new(&child, root.path()).with_home(root.path());
        let mut cache = GuidanceCache::default();
        let first = cache.load(&ctx);
        assert!(first.complete);
        assert!(first.text.contains("root reference"));
        assert!(!first.text.contains("outer instruction"));
        assert_eq!(first.key, cache.load(&ctx).key);

        let nearest = child.join("README.md");
        std::fs::write(&nearest, "nearer reference").unwrap();
        let second = cache.load(&ctx);
        assert_ne!(first.key, second.key);
        assert!(second.text.contains("nearer reference"));
        assert!(!second.text.contains("root reference"));

        std::fs::write(&nearest, "updated nearby reference").unwrap();
        let updated = cache.load(&ctx);
        assert_ne!(second.key, updated.key);
        assert!(updated.text.contains("updated nearby reference"));
        std::fs::remove_file(&nearest).unwrap();
        assert_eq!(first.key, cache.load(&ctx).key);
    }

    #[test]
    fn unreadable_or_non_file_agents_never_fall_back() {
        let root = tempfile::tempdir().unwrap();
        let agents = root.path().join("AGENTS.md");
        std::fs::write(root.path().join("README.md"), "reference").unwrap();
        let ctx = Context::new(root.path(), root.path()).with_home(root.path());
        let mut cache = GuidanceCache::default();
        std::fs::write(&agents, [0xff, 0xfe]).unwrap();
        let invalid = cache.load(&ctx);
        assert!(!invalid.complete);
        assert!(invalid.text.contains("Cannot read"));
        assert!(!invalid.text.contains("README reference"));

        std::fs::remove_file(&agents).unwrap();
        std::fs::create_dir(&agents).unwrap();
        let directory = cache.load(&ctx);
        assert!(!directory.complete);
        assert!(!directory.text.contains("README reference"));
    }

    #[test]
    fn readme_fences_close_only_with_matching_delimiters() {
        let excerpt = readme_excerpt(
            "# Project\nIntro.\n````markdown\n```sh\nhidden command\n```\n## hidden heading\n````\n~~~sh\n```\nhidden mixed fence\n~~~\n## Usage\nVisible reference.\n",
        );
        assert!(excerpt.contains("Intro."));
        assert!(!excerpt.contains("Visible reference."));
        assert!(excerpt.contains("Usage"));
        assert!(!excerpt.contains("hidden"));
    }

    #[test]
    fn instructions_follow_scope_and_refresh_on_file_changes() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("src");
        std::fs::create_dir_all(&child).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root instruction").unwrap();
        std::fs::write(child.join("AGENTS.md"), "child instruction").unwrap();
        let ctx = Context::new(&child, root.path()).with_home(root.path());
        let mut cache = GuidanceCache::default();
        let first = cache.load(&ctx);
        assert!(
            first.text.find("root instruction").unwrap()
                < first.text.find("child instruction").unwrap()
        );
        let again = cache.load(&ctx);
        assert_eq!(first.key, again.key);
        std::fs::write(child.join("AGENTS.md"), "updated child instruction").unwrap();
        let changed = cache.load(&ctx);
        assert_ne!(first.key, changed.key);
        assert!(changed.text.contains("updated child instruction"));
        let parent = cache.load(&Context::new(root.path(), root.path()).with_home(root.path()));
        assert!(!parent.text.contains("child instruction"));
    }

    #[test]
    fn oversized_and_protected_guidance_are_explicitly_unavailable() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("AGENTS.md");
        std::fs::write(&file, "x".repeat(GUIDANCE_CHARS + 1)).unwrap();
        let mut ctx = Context::new(root.path(), root.path()).with_home(root.path());
        let mut cache = GuidanceCache::default();
        let snapshot = cache.load(&ctx);
        assert!(!snapshot.complete);
        assert!(snapshot.text.contains("not loaded"));
        assert!(snapshot.text.contains("Read before acting"));
        assert!(!snapshot.text.contains(&"x".repeat(100)));
        ctx.protected.push(file);
        let blocked = cache.load(&ctx);
        assert!(blocked.text.contains("protected path"));
        assert!(!blocked.text.contains(&"x".repeat(100)));
    }

    #[test]
    fn guidance_budget_preserves_whole_files_root_first() {
        let root = tempfile::tempdir().unwrap();
        let child = root.path().join("src");
        std::fs::create_dir(&child).unwrap();
        let root_text = "r".repeat(GUIDANCE_CHARS - 5);
        std::fs::write(root.path().join("AGENTS.md"), &root_text).unwrap();
        std::fs::write(child.join("AGENTS.md"), "child instruction").unwrap();
        let ctx = Context::new(&child, root.path()).with_home(root.path());
        let guidance = GuidanceCache::default().load(&ctx);
        assert!(!guidance.complete);
        assert!(guidance.text.contains(&root_text));
        assert!(!guidance.text.contains("child instruction"));
        assert!(guidance.text.contains("not loaded"));
    }

    #[test]
    fn nested_repositories_do_not_inherit_outer_guidance() {
        let root = tempfile::tempdir().unwrap();
        let repo = root.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "outer instruction").unwrap();
        let ctx = Context::new(&repo, root.path()).with_home(root.path());
        assert!(GuidanceCache::default().load(&ctx).text.is_empty());
    }
}
