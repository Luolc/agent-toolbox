# agent-toolbox

Command-line tools for running and operating AI coding agents (Claude Code, Codex, Grok Build), built as one Rust binary so a machine gets everything by copying a single file.

## Status

Skeleton only. The first tool to land is `usage`: subscription quota (the vendor's 5-hour / 7-day windows) and local token ledgers, ported from the Python `lo-coding-agent-usage` in the author's private tooling. Planned next: an account service for machines that rotate between several subscription accounts.

## Build

```sh
cargo build --release
./target/release/agent-toolbox
```

The toolchain is pinned in `rust-toolchain.toml`; `rustup` picks it up automatically.

## Checks

`pre-commit run --all-files` (gitleaks, `cargo fmt --check`) before every push; CI's `check` job adds `cargo clippy -D warnings`, `cargo test` and `cargo doc`.

## License

Apache-2.0, see `LICENSE`.
