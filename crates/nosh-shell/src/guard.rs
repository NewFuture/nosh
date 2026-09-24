//! Safety net: a destructive command whose arguments read like prose
//! (`rm all temp files`) is held for confirmation before it runs.

use std::path::Path;

const DESTRUCTIVE: &[&str] = &[
    "rm", "mv", "dd", "chmod", "chown", "chgrp", "kill", "killall", "pkill", "truncate", "shred",
    "rmdir", "unlink", "mkfs", "wipefs",
];

fn is_word(s: &str) -> bool {
    let mut chars = s.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first.is_alphabetic())
        && s.chars()
            .all(|c| c.is_alphabetic() || c == '\'' || c == '-')
        && !s.contains("--")
}

/// `true` when `argv` starts with a destructive command followed by at least
/// three plain words, none of which is an option or an existing path.
pub fn looks_like_prose(argv: &[String], cwd: &Path) -> bool {
    let Some(name) = argv.first() else {
        return false;
    };
    let base = name.rsplit('/').next().unwrap_or(name);
    let args: &[String] = if base == "git" {
        match argv.get(1).map(String::as_str) {
            Some("reset" | "clean" | "rm" | "checkout") => &argv[2..],
            _ => return false,
        }
    } else if DESTRUCTIVE.contains(&base) || base.starts_with("mkfs.") {
        &argv[1..]
    } else {
        return false;
    };
    if args
        .iter()
        .any(|a| a.starts_with('-') || cwd.join(a).exists() || Path::new(a).is_absolute())
    {
        return false;
    }
    args.iter().filter(|a| is_word(a)).count() >= 3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Vec<String> {
        s.split_whitespace().map(str::to_string).collect()
    }

    #[test]
    fn detects_prose() {
        let dir = std::env::temp_dir();
        assert!(looks_like_prose(&v("rm all temp files please"), &dir));
        assert!(looks_like_prose(&v("kill the node server"), &dir));
        assert!(looks_like_prose(
            &v("git reset everything to yesterday"),
            &dir
        ));
        assert!(!looks_like_prose(&v("rm -rf build"), &dir));
        assert!(!looks_like_prose(&v("rm a.txt"), &dir));
        assert!(!looks_like_prose(&v("ls all my files"), &dir));
        assert!(!looks_like_prose(&v("mv one two"), &dir));
    }

    #[test]
    fn existing_path_disables_guard() {
        let dir = std::env::temp_dir().join(format!("nosh-guard-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("files")).unwrap();
        assert!(!looks_like_prose(&v("rm all temp files"), &dir));
        let _ = std::fs::remove_dir_all(dir);
    }
}
