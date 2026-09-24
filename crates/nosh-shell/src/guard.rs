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

/// `true` when `argv` starts with a destructive command, has no option, and
/// has at least three plain words that are not all existing paths: running
/// `rm README all temp files` would delete `README`, while plain words that
/// all name existing files are a deliberate list. The mode, owner or group of
/// `chmod`/`chown`/`chgrp` and the revision of `git reset`/`git checkout` come
/// first and need not exist.
pub fn looks_like_prose(argv: &[String], cwd: &Path) -> bool {
    let Some(name) = argv.first() else {
        return false;
    };
    let base = name.rsplit('/').next().unwrap_or(name);
    let (args, first_is_file): (&[String], bool) = if base == "git" {
        match argv.get(1).map(String::as_str) {
            Some("reset" | "checkout") => (&argv[2..], false),
            Some("clean" | "rm") => (&argv[2..], true),
            _ => return false,
        }
    } else if matches!(base, "chmod" | "chown" | "chgrp") {
        (&argv[1..], false)
    } else if DESTRUCTIVE.contains(&base) || base.starts_with("mkfs.") {
        (&argv[1..], true)
    } else {
        return false;
    };
    if args.iter().any(|a| a.starts_with('-')) {
        return false;
    }
    let words: Vec<&String> = args.iter().filter(|a| is_word(a)).collect();
    let skip = usize::from(!first_is_file && args.first().is_some_and(|a| is_word(a)));
    words.len() >= 3 && words[skip..].iter().any(|w| !cwd.join(w).exists())
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
        assert!(looks_like_prose(&v("rm /missing/one all temp files"), &dir));
        assert!(!looks_like_prose(&v("rm -rf build"), &dir));
        assert!(!looks_like_prose(&v("rm -rf all temp files"), &dir));
        assert!(!looks_like_prose(&v("rm a.txt"), &dir));
        assert!(!looks_like_prose(&v("rm /a /b /c"), &dir));
        assert!(!looks_like_prose(&v("ls all my files"), &dir));
        assert!(!looks_like_prose(&v("mv one two"), &dir));
    }

    #[test]
    fn only_existing_paths_make_a_file_list() {
        let dir = std::env::temp_dir().join(format!("nosh-guard-{}", std::process::id()));
        for name in ["alpha", "beta", "gamma"] {
            std::fs::create_dir_all(dir.join(name)).unwrap();
        }
        assert!(!looks_like_prose(&v("rm alpha beta gamma"), &dir));
        // Running these would delete `alpha`.
        assert!(looks_like_prose(&v("rm alpha all temp files"), &dir));
        assert!(looks_like_prose(&v("rm ./alpha and the rest"), &dir));
        assert!(looks_like_prose(&v("rm root alpha beta"), &dir));
        for line in [
            "chmod go-w alpha beta",
            "chown root alpha beta",
            "chgrp staff alpha beta",
            "git reset HEAD alpha beta",
            "git checkout main alpha beta",
        ] {
            assert!(!looks_like_prose(&v(line), &dir), "{line}");
        }
        assert!(looks_like_prose(&v("chown root alpha and the rest"), &dir));
        assert!(looks_like_prose(
            &v("git checkout main alpha and the docs"),
            &dir
        ));
        let _ = std::fs::remove_dir_all(dir);
    }
}
