# Reedline source maintenance

Reedline and brush each use an **official upstream submodule, a maintained patch,
and an ignored editable build copy**. Source preparation requires Rust, Git
(with `git archive --mtime` support), and the platform linker.

## Layout and source of truth

| Path | Purpose |
|---|---|
| `third_party/reedline-upstream` | Clean official `nushell/reedline` gitlink |
| `patches/reedline/source.toml` | Official URL and full revision; checked against the gitlink; also works in Git archives |
| `patches/reedline/nosh.patch` | Only maintained delta, including library tests, explanation and standalone `Cargo.lock` changes |
| `tools/source` | Independent Rust workspace and lockfile; starts before root Cargo metadata can resolve dependencies |
| `.cargo/config.toml` | Portable `cargo source` alias; no wrapper script or additional tool |
| `.nosh/reedline` | Ignored, patched source used directly by Cargo and editable in the same IDE |
| `.nosh/reedline/.git/nosh` | Generated input snapshots and provenance, not another maintained source |
| `.nosh/cache/reedline.git` | Optional official Git object cache when the submodule is absent |
| `third_party/brush-upstream` | Clean official `reubeno/brush` gitlink at the brush-core 0.5.0 release |
| `patches/brush-core/source.toml` / `nosh.patch` | Pinned brush monorepo and the completion-only maintained delta |
| `.nosh/brush` | Ignored editable monorepo; Cargo overrides core and parser to its matching crate subdirectories |
| `.nosh/cache/brush-core.git` | Per-checkout brush object cache |

The exact path-and-version dependency cannot silently fall back to registry
Reedline. Never modify Cargo's shared registry, edit the clean submodule, or
expect a gitlink to contain uncommitted submodule edits.

The source tool checks each **index** gitlink, so an upgrade can be reviewed before
committing. Source archives lack an index; their tracked `source.toml` pins the
same commit. Git's object identity, patch SHA256, upstream/patched Git trees and
fixed-mtime patched archive SHA256 are recorded by `provenance`. Text input
hashes use LF, matching the repository's `.gitattributes`.

## First checkout and daily commands

From the repository root, use `cargo source prepare`, `check`, `export`,
`provenance` or `upgrade`; `cargo source --help` lists the options. The Cargo
alias expands to `cargo run --manifest-path tools/source/Cargo.toml --locked --`
and works before the application's path dependency exists. The explicit form
remains supported for CI and callers outside the repository root, with
`--root <repository-or-archive>` when their current directory is elsewhere.
The default root is the **runtime working directory**, never a path baked into
a cached tool binary; sharing `CARGO_TARGET_DIR` between checkouts/archives does
not redirect preparation into the checkout where the tool was first compiled.
An invalid root is rejected before creating `.nosh`.

Use native Git and Rust on Linux/macOS, or in a Linux-native WSL checkout:

```bash
git clone --recurse-submodules https://github.com/NewFuture/nosh.git
cd nosh
cargo source prepare --offline
```

For a Windows-hosted worktree, prepare sources with Windows Git/Rust in
PowerShell. The source tool and standalone Reedline tests support Windows;
the nosh application has **not** been ported to native Windows:

```powershell
git clone --recurse-submodules https://github.com/NewFuture/nosh.git
Set-Location nosh
cargo source prepare --offline
```

Run application builds in a **Linux/macOS terminal**. For the Windows checkout
above, switch to WSL and enter its mounted directory first (for example,
`cd /mnt/c/github/nosh` for `C:\github\nosh`):

```bash
cargo metadata --locked --format-version 1
cargo build --locked
cargo test --workspace --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Remaining maintenance examples use PowerShell paths; use `/` on Linux/macOS.
Do not run Linux Git against a Windows worktree's `.git` file.

That is **one additional preparation command** after clone. `--offline` after
`--` forbids fetching upstream Git objects; Cargo itself can still obtain the
bootstrap's locked Rust dependencies. To forbid both networks, use
`cargo --offline source prepare --offline` after warming the crate cache. With an ordinary
non-recursive clone or a source archive, omit the tool's `--offline`: it fetches
the exact official commit into the per-checkout cache, not a moving branch.

Rerun `prepare` after switching branches/pins/patches, before opening or reloading
the IDE. Missing sources make full Cargo metadata fail; `cargo fmt` or metadata
with `--no-deps` is not a preparation check. The shell's `build.rs` only rejects
stale prepared input snapshots; it never fetches or applies patches. IDE metadata
alone cannot detect a stale path dependency; use `check` before trusting it.

```powershell
cargo source check
cargo source provenance
```

`prepare`, `check`, `export` and default `provenance` cover both fixed dependencies.
Default provenance is an object keyed by `reedline` and `brush-core`, recording
the build inputs used by source verification and evaluation.

Use `--dependency reedline` or `--dependency brush-core` for a focused operation,
or for separate read-only offline caches. Focused provenance returns that
dependency's state object; the default command returns the two-dependency map.
`upgrade` without a selector targets Reedline; use
`--dependency brush-core --rev <SHA>` for brush.

Repeated clean preparation is a no-op. Missing offline objects, invalid patches,
dirty upstream checkouts, mismatched gitlinks and unexported edits are explicit
errors. There is no unpatched fallback. Build output under the generated library's
`target` directory is excluded; all other files, including ignored/new/deleted
source and binary files, participate in export/check.

## Editing nosh and Reedline together

Edit nosh normally and the library under `.nosh/reedline`; root Cargo already
uses this copy. Unexported library edits are allowed for local compilation, but
`check` and subsequent preparation refuse them until exported.

```powershell
cargo fmt --manifest-path .nosh\reedline\Cargo.toml
cargo test --manifest-path .nosh\reedline\Cargo.toml --lib --features external_printer --locked --target-dir target\reedline
cargo source export
cargo source check
git diff -- patches\reedline\nosh.patch
```

`export` snapshots the editable copy using a temporary Git index, regenerates the
delta against the pinned commit, and replays it in another clean copy before
replacing the patch. It does not edit the upstream checkout or the main index.
Review and commit nosh edits and the exported patch in **one PR**. Do not edit the
patch and the generated source independently; an input change blocks export
rather than guessing which copy wins. Export before changing branches.

For brush, edit `.nosh/brush/brush-core` and run the completion regressions through
the root workspace in Linux/macOS/WSL. Export with the checkout's native Git:

```powershell
cargo source export --dependency brush-core
cargo source check
```

## Upgrading upstream

Start with `check` passing and a clean upstream checkout. The tool accepts a full
lowercase commit SHA, never an upstream branch name:

```powershell
cargo source upgrade --rev <40-character-commit>
```

It prepares and replays a candidate before updating the pin, patch and staged
gitlink. An initialized upstream checkout is moved to the new clean commit; an
absent submodule remains absent. Review the staged gitlink along with the other
changes. Align the root's exact Reedline version if the upstream package version
changed; then update the root/standalone lockfiles deliberately and rerun export
and regressions. A version mismatch is a Cargo error, not a registry fallback.

If the old patch conflicts, the command fails and prints the retained candidate
path. The live source, pin and patch are preserved. Import old base
objects into that candidate and apply/resolve there, never in the submodule:

```powershell
git -C <candidate> fetch --no-tags <absolute-path-to-.nosh\reedline> <old-commit>
git -C <candidate> apply --3way --index <absolute-path-to-patches\reedline\nosh.patch>
# Resolve conflicts in the candidate, retaining all still-needed fixes.
git -C <candidate> add --all
cargo source upgrade --rev <new-commit> --resolved <candidate>
```

When upstream has absorbed a fix, remove its redundant hunk by resolving to the
new upstream behavior. Export is recomputed from the new base, so absorbed
changes disappear naturally, including an entirely empty patch. No heuristic
silently skips a failed hunk. Candidates remain available for inspection.

Source commands hold one per-checkout exclusive lock across all selected
dependencies. Do not edit the source while preparing/upgrading it.
Interrupted installs retain `.nosh/previous-reedline` and
refuse another replacement until it has been inspected; never delete it without
preserving any wanted work. Invalid/incomplete state is an error, not permission
to overwrite an existing source directory.
Directory installation retries only transient permission/sharing failures a
bounded number of times; a persistent failure names both paths and preserves
the candidate (and attempts to restore the previous live copy).
In private staging, the tool refreshes Git's index before both indexed patch
checks and application. A `does not match index` failure is retried only when
the index tree and a freshly hashed worktree both still match the pinned base,
at most four attempts. Actual edits, changed index entries and patch conflicts
fail without retry; `--index` is never removed.

## Changing the maintenance tool

The tool explicitly manages these two known official dependencies, not a generic
dependency manager or plugin framework. Modify the
smallest owning module rather than adding another wrapper or preparation route:

| Change | Location |
|---|---|
| Commands and options | `tools/source/src/main.rs` |
| Prepare/export/upgrade, input snapshots and provenance | `tools/source/src/source.rs` |
| Git commands, fresh tree hashing and platform retries | `tools/source/src/worktree.rs` |
| Consumer's stale-input guard, without extra dependencies | `tools/source/guard.rs` |
| Low-level regressions / complete lifecycle and alias bootstrap | `src/worktree/tests.rs` / `tests/workflow.rs` under `tools/source` |

Export and upgrade share one delta-and-replay function. Each operation keeps one
LF-normalized pin/patch snapshot for verification and provenance; replay patches
stay inside the private candidate's `.git`, not in another maintained file.

```powershell
cargo fmt --manifest-path tools\source\Cargo.toml -- --check
cargo test --manifest-path tools\source\Cargo.toml --locked
cargo clippy --manifest-path tools\source\Cargo.toml --all-targets --locked -- -D warnings
```

Run these on Windows and Linux/WSL for changes to Git or filesystem handling.
The alias-bootstrap regression starts with a missing application dependency;
the lifecycle suite also compiles the actual consumer build script.

## Archives, offline builds and CI

`git archive` includes the gitlink directory but **not its contents**. The archive
does include the pin, patch, tool, Cargo alias, both application/bootstrap
lockfiles and toolchain file; these are sufficient to reconstruct the library:

```powershell
Set-Location <archive>
cargo source prepare
cargo source provenance
```

For disconnected use, provide `--offline --cache <existing-Git-repository>`
containing the exact commit and a warmed Cargo crate cache. An explicitly
supplied cache is always read-only, including when it lacks the commit; omit
`--cache` to permit downloading into the private cache. It is not a
copy of the edited library. An archive is not advertised as a completely offline
source bundle: the repository already also depends on registry crates and Candle
Git objects.

Every CI job prepares **before `Swatinem/rust-cache`**, which invokes full Cargo
metadata. Regular CI initializes submodules; the evaluation job instead archives
its selected commit, then executes the **selected archive's** source tool.
It records `source-dependencies.json` and includes it in `build-info.json`, beside
the root archive hash and exact binary hash. A second strict provenance read
after compilation must match the pre-build one. Selected sources must use the
current complete managed layout; missing preparation tools or pins fail instead
of falling back. Every tracked archive file is checked again after
the build, including lockfiles that cache-action metadata could otherwise rewrite.

Windows source-tool/editor tests complement Linux/macOS CI. For this app's
Windows worktrees, run preparation/export with Windows Cargo and Windows Git.
WSL can then build/test the files; do not use Linux Git on a `.git` file containing
a Windows worktree path, repair shared `.git` configuration, or alter another
worktree. Native Linux checkouts/source archives use native Git normally.

## Why not trimmed vendor, subtree or a release-package downloader?

The controlled comparison used upstream `61d43080` and release archive SHA256
`3798f88894590a7acdc9a25936605efbde199641f31e8d99248fce72e535638a`.
All 71 compared source/README files were identical after CRLF-to-LF normalization;
the original release manifest matched upstream Git. The initial Git patch replay
matched the verified implementation's library source, with the standalone
lockfile included. Cargo metadata compared equal for dependency declarations and
target definitions; the trimmed-vendor control also resolved directly with its
locked manifest.

Retaining `src`, examples required by Cargo targets, both Cargo files, README,
license and patch explanation still required **100 files / 48,368 lines** in the
experiment; source alone was 44,682 lines. Subtree improves upstream import/merge
history, not tracked source size or the initial PR diff. It would preserve
zero-preparation native Cargo, but does not meet the small-main-repository-diff
goal. Removing tests/source or hiding diff statistics would not fix that tradeoff.

A fixed release package plus SHA256 is viable for consumers, but adds a separate
download/extraction/checksum pipeline and normalization against Git for upstream
maintenance. Here Git already supplies immutable objects, checkout, binary
patches and the upgrade/export machinery. Only the Git-based route is shipped.

This maintenance change preserves the four-zone prompt integration, read-only
context and opt-in menu submission protection. It does **not** resolve the
recorded tmux resize/history failures or replace the required host acceptance
checks; see [STATUS-BAR.md](STATUS-BAR.md).

The input-editing integration also uses the same maintained delta: opt-in
authoritative input-context dispatch, distinct history acceptance/cancellation,
one-step host draft replacement, buffered-submit protection and bounded Vi
sequences/repeats. It does not introduce another maintained source or modify
the clean upstream. Contracts and terminal capability limits are documented in
[INPUT-EDITING.md](INPUT-EDITING.md); exported library regressions accompany
the host's configuration and real-PTY tests.
