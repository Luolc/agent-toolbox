# Agent rules (this repository)

Language: case B, everything in this repository is English. Global rules live in `~/.agents/AGENTS.md` (source: the `machine-setup` repository, `dotfiles/dot_agents/readonly_AGENTS.md`); this file only adds what is specific to this repository. `CLAUDE.md` is a symlink to this file; always edit this file.

This is a public repository: no personal information, no hostnames of private machines, no credentials or references to private vaults. Machine-specific integration belongs to the private repositories that consume this tool.

Short name `atb`; the resident orchestra is `atb-orchestra`.

## Scope

One Rust binary with subcommands, each a self-contained tool. A tool reads local files or makes single HTTP calls; no daemons, no retry loops, no polling inside this binary. Anything that must run continuously lives in the consuming repository as a scheduled job that calls this binary.

## Command tree

Tool first, target harness second: `atb <tool> <harness>`, with the harness (`claude`, `codex` or `grok`) as a positional argument, as in `atb quota codex`. The binary is `atb`; `agent-toolbox` stays the repository and crate name.

## Credentials

Tools read vendor CLI credentials (`~/.claude/.credentials.json`, `~/.codex/auth.json`, `~/.grok/auth.json`) only to build request headers. A token value never appears in output, logs, error messages or test fixtures; tests use synthetic files under a temporary home. Every credential location must be overridable (`--config-dir`, the harness's own variable, `ATB_HOME`; the README lists the precedence) so tests never touch the real home. A token may also be passed directly, only through an environment variable, never a flag.

## Quality checks and merging

- Local: `pre-commit run --all-files` (gitleaks on the staged diff, `cargo fmt --check`); run `pre-commit install` once after cloning. Before pushing also run `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked`.
- CI: `.github/workflows/ci.yml`, job `check`: full-history gitleaks scan, `cargo fmt --check`, clippy with `-D warnings`, `cargo test`, `cargo doc` with warnings denied. `check` is the required status check on `main`.
- Reviewers also load `.agents/skills/atb-pr-review`.
