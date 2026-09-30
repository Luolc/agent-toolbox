# agent-toolbox

Command-line tools for running and operating AI coding agents (Claude Code, Codex, Grok Build), built as one Rust binary so a machine gets everything by copying a single file.

## `atb quota`

```sh
atb quota claude|codex|grok [--json] [--config-dir <DIR>]
```

Reads the subscription quota the vendor enforces: window percentages and reset times. The default output is a table; `--json` prints the raw record. Every record carries `ts` (UTC), `host`, `agent` and `provenance`.

**Quota percentages are not ledgers and not bills.** They say how much of the current window is used and when it resets. They do not say how many tokens were spent or what anything cost, and the two kinds of number cannot stand in for each other.

| Harness | Source | `provenance` | Requests |
|---|---|---|---|
| `claude` | `GET https://api.anthropic.com/api/oauth/usage`, the request `/usage` makes | `oauth_usage` | 1 |
| `codex` | `GET https://chatgpt.com/backend-api/wham/usage`, the request `/status` makes | `usage_api` | 1 |
| `codex` (fallback) | newest rate-limit snapshot in `sessions/*/*/*/rollout-*.jsonl` | `local_rollout` | 0 |
| `grok` | `GET https://cli-chat-proxy.grok.com/v1/billing?format=credits` | `billing_api` | 1 |

Each run makes at most one request, with a timeout (Claude 5 s, Codex 10 s, Grok 15 s), and never retries. The official Claude and Codex clients do not poll these endpoints, so do not schedule this command at a high frequency either. A `429` prints the `retry-after` value and exits with status 2; other errors exit with status 1.

Headers match what each vendor's CLI sends; the version in the `User-Agent` comes from running `claude --version`, `codex --version` or `grok --version`, so that CLI must be on `PATH`. `atb` never refreshes a token: if the stored one has expired, start the vendor CLI once.

Codex falls back to the local snapshot when the usage API gives no answer: the credential file is missing or unusable, `codex --version` cannot be run, the request fails or times out, the status is not 2xx, or the body is not the expected JSON. A note on stderr says why, and `provenance` says which source answered. In a fallback record `ts` is when Codex wrote the snapshot, not when the command ran. A `429` never falls back. Window names follow the window length (300 minutes is `five_hour`, 10080 minutes is `seven_day`, anything else is `window_<n>m`).

### Credential sources

Highest first:

1. A token passed in the environment: `ATB_CLAUDE_TOKEN`; `ATB_CODEX_TOKEN` with `ATB_CODEX_ACCOUNT_ID`; `ATB_GROK_TOKEN` with `ATB_GROK_USER_ID`. When the token variable is set, no credential file is read, and for Codex and Grok the id variable is required (its absence is an error). There is no flag for a token, because a flag would show up in the process list and the shell history.
2. `--config-dir <DIR>`: the harness config directory itself, the one holding `.credentials.json` (Claude) or `auth.json` (Codex, Grok).
3. The harness's own variable: `CLAUDE_CONFIG_DIR`, `CODEX_HOME` or `GROK_HOME`.
4. `ATB_HOME` as a whole-home override: `<ATB_HOME>/.claude`, `<ATB_HOME>/.codex`, `<ATB_HOME>/.grok`.
5. The default home: `~/.claude`, `~/.codex`, `~/.grok`.

An empty variable counts as unset. The Codex fallback reads `sessions/` under the directory that 2 to 5 resolve to. Records that used a credential say where it came from in `credential_source`: `env`, or the path of the file. A token value is only ever used to build the `Authorization` header; it never appears in output or in an error message. The `--version` child process is started without any `ATB_*` variable, so a token passed in the environment does not reach it.

## Build

```sh
cargo build --release
./target/release/atb --help
```

The toolchain is pinned in `rust-toolchain.toml`; `rustup` picks it up automatically.

## Checks

`pre-commit run --all-files` (gitleaks, `cargo fmt --check`) before every push; CI's `check` job adds `cargo clippy -D warnings`, `cargo test` and `cargo doc`.

## License

Apache-2.0, see `LICENSE`.
