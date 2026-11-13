# Breeze

Breeze keeps local-only file overlays for Git worktrees: machine-specific
files and small patches that stay out of commits, applied consistently across
every worktree of a repository. It also deep-merges overlay configuration into
`fog` configuration without touching tracked files.

## Build

Requires a Rust toolchain with Cargo (edition 2021; Rust 1.70 or newer).

```sh
cargo build
cargo test
```

Run the CLI with `cargo run -- <command>` or directly via
`target/debug/breeze`.
