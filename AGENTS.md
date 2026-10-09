# Agent rules (this repository)

Everything in this repository is English. `CLAUDE.md` is a symlink to this file; always edit this file.

This is a public repository. Machine-specific integration belongs to the private repositories that consume this tool.

Short name `atb`.

## Scope

One Rust binary with subcommands, each a self-contained tool. A command reads local files or makes a fixed, bounded set of HTTP calls (comment pagination counts as bounded, by the server's `hasNextPage`); no daemons, no retry loops, no polling inside this binary. The one exception is `atb linear`: on a 401 with a key from `LINEAR_API_KEY_CMD` it fetches the key once more and retries that request once. Anything that must run continuously lives in the consuming repository as a scheduled job that calls this binary.

## Command tree

Tool first, target harness second: `atb <tool> <harness>`, with the harness (`claude`, `codex` or `grok`) as a positional argument, as in `atb quota codex`. A tool that targets no harness takes its own subcommands, as in `atb linear claim`. The binary is `atb`; `agent-toolbox` stays the repository and crate name.

## Credentials

Tools read vendor CLI credentials (`~/.claude/.credentials.json`, `~/.codex/auth.json`, `~/.grok/auth.json`) only to build request headers. A token value never appears in output, logs, error messages or test fixtures; tests use synthetic files under a temporary home. Every credential location must be overridable (`--config-dir`, the harness's own variable, `ATB_HOME`; the README lists the precedence) so tests never touch the real home. A token may also be passed directly, only through an environment variable, never a flag. The Linear key comes from `LINEAR_API_KEY`, or from the output of the command in `LINEAR_API_KEY_CMD`, cached for 24 hours in `$XDG_CONFIG_HOME/linear/api-key` (mode 0600, directory 0700); the command's stderr is never shown.

## Layout and docs

- `docs/design.md` is the one design document: current state only, edited in place, five sections, at most 200 lines (checked in CI). It is edited only in a design pass the orchestra triggers; every other PR fills in "Design impact" in the template.
- Every PR follows `.github/pull_request_template.md`.
- Manuals live in `docs/knowledge/` (the release manual). The CLI reference is `atb --help`.
- There are no ADRs, research notes or to-do list in the repository. Open work is tracked outside the repository.

## Quality checks

- Local: `pre-commit run --all-files` (gitleaks on the staged diff, `cargo fmt --check`); run `pre-commit install` once after cloning. Before pushing also run `cargo clippy --all-targets --locked -- -D warnings` and `cargo test --locked`.
- After `cargo publish --dry-run`, `cargo test` can run a stale binary and miss source edits. Run `cargo clean -p agent-toolbox` before mutation runs and before any test run that follows a publish dry run.
- CI: `.github/workflows/ci.yml`, job `check`: full-history gitleaks scan, `cargo fmt --check`, clippy with `-D warnings`, `cargo test`, `cargo doc` with warnings denied, the design document length check. `check` is the required status check on `main`.
