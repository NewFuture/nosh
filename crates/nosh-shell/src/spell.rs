//! Local spelling correction for command names (edit distance ≤ 2).

/// Optimal string alignment distance (Levenshtein + adjacent transpositions).
pub fn osa_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let (n, m) = (a.len(), b.len());
    let mut d = vec![vec![0usize; m + 1]; n + 1];
    for (i, row) in d.iter_mut().enumerate() {
        row[0] = i;
    }
    for (j, cell) in d[0].iter_mut().enumerate() {
        *cell = j;
    }
    for i in 1..=n {
        for j in 1..=m {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            let mut v = (d[i - 1][j] + 1)
                .min(d[i][j - 1] + 1)
                .min(d[i - 1][j - 1] + cost);
            if i > 1 && j > 1 && a[i - 1] == b[j - 2] && a[i - 2] == b[j - 1] {
                v = v.min(d[i - 2][j - 2] + 1);
            }
            d[i][j] = v;
        }
    }
    d[n][m]
}

/// Commonly used commands win ties.
const COMMON: &[&str] = &[
    "git",
    "ls",
    "cd",
    "cat",
    "grep",
    "find",
    "docker",
    "python3",
    "python",
    "make",
    "cargo",
    "npm",
    "node",
    "vim",
    "ssh",
    "cp",
    "mv",
    "rm",
    "mkdir",
    "echo",
    "sudo",
    "kubectl",
    "pip",
    "curl",
    "wget",
    "tar",
    "less",
    "head",
    "tail",
    "top",
    "ps",
    "kill",
    "man",
    "go",
    "rustc",
    "clear",
    "exit",
    "history",
    "which",
    "touch",
    "chmod",
    "sed",
    "awk",
    "sort",
    "uniq",
    "wc",
    "du",
    "df",
    "free",
    "systemctl",
    "journalctl",
    "apt",
    "brew",
    "yarn",
    "pnpm",
    "code",
    "nano",
];

fn max_distance(word: &str) -> usize {
    match word.chars().count() {
        0..=1 => 0,
        2..=4 => 1,
        _ => 2,
    }
}

/// Candidates within the allowed distance, best first, if the word is plausibly a typo.
pub fn ranked_matches<'a>(word: &str, candidates: &'a [String]) -> Vec<&'a str> {
    if !word.is_ascii() || word.len() < 2 {
        return Vec::new();
    }
    let max = max_distance(word);
    let mut found: Vec<(usize, usize, usize, &str)> = Vec::new();
    for c in candidates {
        if c == word || c.len().abs_diff(word.len()) > max {
            continue;
        }
        let d = osa_distance(word, c);
        if d == 0 || d > max {
            continue;
        }
        let rank = COMMON.iter().position(|x| x == c).unwrap_or(COMMON.len());
        found.push((d, rank, c.len(), c.as_str()));
    }
    found.sort();
    found.into_iter().map(|f| f.3).collect()
}

/// Best candidate within the allowed distance.
pub fn best_match<'a>(word: &str, candidates: &'a [String]) -> Option<&'a str> {
    ranked_matches(word, candidates).into_iter().next()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn distances() {
        assert_eq!(osa_distance("gti", "git"), 1);
        assert_eq!(osa_distance("sl", "ls"), 1);
        assert_eq!(osa_distance("pytohn3", "python3"), 1);
        assert_eq!(osa_distance("dcoker", "docker"), 1);
        assert_eq!(osa_distance("kitten", "sitting"), 3);
    }

    #[test]
    fn picks_common_commands() {
        let c: Vec<String> = [
            "git", "gtk", "gio", "gdb", "ls", "lsb", "docker", "python3", "cat",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(best_match("gti", &c), Some("git"));
        assert_eq!(best_match("sl", &c), Some("ls"));
        assert_eq!(best_match("dcoker", &c), Some("docker"));
        assert_eq!(best_match("pyhton3", &c), Some("python3"));
        assert_eq!(best_match("帮我看看", &c), None);
        assert_eq!(best_match("zzzzzz", &c), None);
    }
}
