# Development guide

nosh is a Rust, Bash-compatible shell with local AI inference.
Use [architecture](docs/ARCHITECTURE.md) for development and code boundaries.
For behavior, read [system design](docs/DESIGN.md) and the [owning feature document](docs/README.md).

## Build and test

Run affected crate/test targets first; interactive changes need CLI/PTY coverage.
`cargo test -p nosh-tests --locked` covers cross-crate flows and architecture,
not the full suite. Check formatting with `cargo fmt --all -- --check`.
Use [architecture](docs/ARCHITECTURE.md) for platform build, lint and regression commands.
Clippy targets are alternatives: `--all-targets` on Unix, `--lib --bins` on native Windows.

Use the pinned `rust-toolchain.toml`. Run `cargo source prepare` on first checkout
and after switching branches or changing dependency pins/patches, before other
workspace Cargo commands.

Build on Linux/macOS/WSL or native Windows x64 with MSVC and C++ build tools.
In Windows worktrees, prepare sources with Windows Git/Rust. Use a separate
`CARGO_TARGET_DIR` for WSL checks; do not run Linux Git against the Windows `.git` file.

For evaluation changes, run relevant Python unittest modules from [eval](eval/README.md).
Real-model tests and downloads are opt-in.

## Conventions

- Follow the documented crate boundaries, enforced by `tests/architecture.rs`.
  Keep core independent of inference and download implementations.
- Report errors explicitly; preserve
  approval, cancellation and shared-shell state semantics.
- Add regression coverage for behavior changes and update the owning feature
  document rather than duplicating its rules.
- Edit Reedline/brush in prepared `.nosh/` copies, then run `cargo source export`
  and `cargo source check`. Do not edit clean upstream submodules or Cargo's
  registry; see [source maintenance](docs/REEDLINE-MAINTENANCE.md).
