//! Daily development commands run in the workspace (design §6, §16 #14:
//! convenience first). Queries never ask, not even in confirm mode, and auto
//! mode asks for none of these commands. The share that confirm mode asks
//! for is printed as the reference for later rule changes:
//! `cargo test -p nosh-permissions --test daily -- --nocapture`.

use nosh_permissions::{
    ApprovalMode, Context, Decision, Risk, SessionAllowList, UserRules, assess_command, decide,
};

use Kind::*;
use Risk::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Reads or prints something.
    Query,
    /// Builds, tests or runs the project.
    Build,
    /// Routine changes to the workspace and its repository.
    Write,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Query => "queries",
            Build => "builds and tests",
            Write => "writes",
        }
    }
}

const DAILY: &[(&str, Kind, Risk)] = &[
    // ---------------- Queries ----------------
    // Programs outside the rule table asked only for their version or usage.
    ("rustc --version", Query, Safe),
    ("rustc --help", Query, Safe),
    ("rustup --version", Query, Safe),
    ("gcc --version", Query, Safe),
    ("gcc --help", Query, Safe),
    ("g++ --version", Query, Safe),
    ("clang --version", Query, Safe),
    ("tsc --version", Query, Safe),
    ("pytest --version", Query, Safe),
    ("gh --version", Query, Safe),
    // Listed read-only forms.
    ("node --version", Query, Safe),
    ("npm --version", Query, Safe),
    ("python3 --version", Query, Safe),
    ("go version", Query, Safe),
    ("cargo --version", Query, Safe),
    ("cargo --help", Query, Safe),
    ("cargo tree", Query, Safe),
    ("make --version", Query, Safe),
    ("git status", Query, Safe),
    ("git status -s", Query, Safe),
    ("git log --oneline -5", Query, Safe),
    ("git log --oneline | head -20", Query, Safe),
    ("git diff", Query, Safe),
    ("git diff --staged", Query, Safe),
    ("git diff --stat", Query, Safe),
    ("git show --stat HEAD", Query, Safe),
    ("git branch --show-current", Query, Safe),
    ("git rev-parse --abbrev-ref HEAD", Query, Safe),
    ("git stash list", Query, Safe),
    ("git blame src/main.rs", Query, Safe),
    ("git grep -n TODO", Query, Safe),
    ("ls -la", Query, Safe),
    ("tree -L 2", Query, Safe),
    ("du -sh .", Query, Safe),
    ("cat Cargo.toml", Query, Safe),
    ("head -50 src/main.rs", Query, Safe),
    ("tail -n 100 build.log", Query, Safe),
    ("wc -l src/*.rs", Query, Safe),
    ("grep -rn \"fn main\" src/", Query, Safe),
    ("rg TODO", Query, Safe),
    ("find . -name \"*.rs\"", Query, Safe),
    ("which cargo", Query, Safe),
    ("jq '.scripts' package.json", Query, Safe),
    ("npm ls --depth=0", Query, Safe),
    ("pip list", Query, Safe),
    // ---------------- Builds and tests ----------------
    ("cargo build", Build, Mutating),
    ("cargo build --release", Build, Mutating),
    ("cargo test", Build, Mutating),
    ("cargo test --workspace", Build, Mutating),
    ("cargo check", Build, Mutating),
    ("cargo clippy --all-targets -- -D warnings", Build, Mutating),
    ("cargo fmt", Build, Mutating),
    ("cargo run", Build, Mutating),
    ("cargo run -- --help", Build, Mutating),
    ("RUST_LOG=debug cargo run", Build, Mutating),
    ("cargo build 2>&1 | tail -20", Build, Mutating),
    ("npm test", Build, Mutating),
    ("npm run build", Build, Mutating),
    ("npm run lint", Build, Mutating),
    ("yarn test", Build, Mutating),
    ("pnpm build", Build, Mutating),
    ("make", Build, Mutating),
    ("make test", Build, Mutating),
    ("make -j8", Build, Mutating),
    ("cmake --build build", Build, Mutating),
    ("pytest", Build, Mutating),
    ("pytest -q tests/test_api.py", Build, Mutating),
    ("python3 -m pytest", Build, Mutating),
    ("go build ./...", Build, Mutating),
    ("go test ./...", Build, Mutating),
    ("go vet ./...", Build, Safe),
    ("mvn test", Build, Mutating),
    ("tsc --noEmit", Build, Mutating),
    ("eslint .", Build, Mutating),
    // A local program stays unknown, `--help` or not.
    ("./target/debug/app --help", Build, Mutating),
    // ---------------- Routine writes ----------------
    ("git add -A", Write, Mutating),
    ("git add src/main.rs", Write, Mutating),
    ("git commit -m \"fix: handle empty input\"", Write, Mutating),
    ("git commit --amend --no-edit", Write, Mutating),
    ("git switch -c feature/x", Write, Mutating),
    ("git checkout main", Write, Mutating),
    ("git stash", Write, Mutating),
    ("git stash pop", Write, Mutating),
    ("git restore --staged src/main.rs", Write, Mutating),
    ("git reset --soft HEAD~1", Write, Mutating),
    ("git merge --no-ff feature", Write, Mutating),
    ("git rebase main", Write, Mutating),
    ("git cherry-pick abc123", Write, Mutating),
    ("git tag v0.1.0", Write, Mutating),
    ("git mv old.rs new.rs", Write, Mutating),
    ("mkdir -p build", Write, Mutating),
    ("touch src/lib.rs", Write, Mutating),
    ("cp a b", Write, Mutating),
    ("cp -r templates/ out/", Write, Mutating),
    ("mv src/a.rs src/b.rs", Write, Mutating),
    ("echo \"target/\" >> .gitignore", Write, Mutating),
    ("sed -i 's/foo/bar/g' src/config.rs", Write, Mutating),
    ("chmod +x scripts/run.sh", Write, Mutating),
    ("rm build.log", Write, Mutating),
    ("cargo build 2>&1 | tee build.log", Write, Mutating),
];

/// The workspace is the working directory.
fn ctx() -> Context {
    Context::new("/home/u/proj", "/home/u/proj").with_home("/home/u")
}

#[test]
fn daily_commands_have_the_expected_levels() {
    assert!(DAILY.len() >= 100, "{} cases", DAILY.len());
    let c = ctx();
    let failures: Vec<String> = DAILY
        .iter()
        .filter_map(|(cmd, _, want)| {
            let r = assess_command(cmd, &c);
            (r.risk() != *want)
                .then(|| format!("{cmd:?}: want {want}, got {} ({:?})", r.risk(), r.findings))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} failures:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

#[test]
fn convenience_first_on_daily_commands() {
    let c = ctx();
    let (rules, grants) = (UserRules::default(), SessionAllowList::default());
    let mut failures = Vec::new();
    let mut asked = Vec::new();
    for (cmd, kind, _) in DAILY {
        let r = assess_command(cmd, &c);
        let confirm = decide(&r, cmd, ApprovalMode::Confirm, &rules, &grants);
        let auto = decide(&r, cmd, ApprovalMode::Auto, &rules, &grants);
        if *kind == Query && confirm != Decision::Allow {
            failures.push(format!(
                "{cmd:?}: a query asks in confirm mode ({confirm:?})"
            ));
        }
        if auto != Decision::Allow {
            failures.push(format!("{cmd:?}: asks in auto mode ({auto:?})"));
        }
        if confirm != Decision::Allow {
            asked.push(*kind);
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    let groups: Vec<String> = [Query, Build, Write]
        .iter()
        .map(|k| {
            let total = DAILY.iter().filter(|(_, kind, _)| kind == k).count();
            let n = asked.iter().filter(|a| *a == k).count();
            format!("{} {n}/{total}", k.label())
        })
        .collect();
    println!(
        "confirm mode asks for {} of {} daily commands ({:.0}%): {}",
        asked.len(),
        DAILY.len(),
        100.0 * asked.len() as f64 / DAILY.len() as f64,
        groups.join(", ")
    );
}
