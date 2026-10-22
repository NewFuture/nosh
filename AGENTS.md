# Development guide

nosh is a Rust, Bash-compatible shell with local AI inference.
Use [architecture](docs/ARCHITECTURE.md) for development and code boundaries.
For behavior, read [system design](docs/DESIGN.md) and the owning document in `docs/`.

## Build and test

Use the pinned `rust-toolchain.toml`. Build on Linux/macOS/WSL or native Windows
x64 with the MSVC toolchain and C++ build tools. In Windows worktrees, prepare
sources with Windows Git/Rust. Use a separate target directory for WSL checks;
do not run Linux Git against the Windows `.git` file.

```text
cargo source prepare
cargo test -p nosh-tests --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Run affected crate/test targets first. Interactive changes need CLI/PTY coverage.
Native Windows regressions: `cargo test -p nosh-cli -p nosh-permissions -p nosh-shell --test windows --locked`.
Windows production lint: `cargo clippy --workspace --lib --bins --locked -- -D warnings`.
For evaluation changes, run relevant Python unittest modules from [eval](eval/README.md).
Real-model tests and downloads are opt-in.

## Conventions

- Follow the documented crate boundaries, enforced by `tests/architecture.rs`.
  Keep core independent of inference and download implementations.
- Reuse existing helpers and patterns. Report errors explicitly; preserve
  approval, cancellation and shared-shell state semantics.
- Add regression coverage for behavior changes and update the owning feature
  document rather than duplicating its rules.
- Edit Reedline/brush in prepared `.nosh/` copies, then run `cargo source export`
  and `cargo source check`. Do not edit clean upstream submodules or Cargo's
  registry; see [source maintenance](docs/REEDLINE-MAINTENANCE.md).
