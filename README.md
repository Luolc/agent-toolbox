# agent-toolbox

Command-line tools for running and operating AI coding agents (Claude Code, Codex, Grok Build), built as one Rust binary so a machine gets everything by copying a single file.

## Install

Prebuilt binaries for Linux (static musl, x86_64 and aarch64) and macOS (x86_64 and aarch64) are attached to each [GitHub release](https://github.com/Luolc/agent-toolbox/releases) as `atb-<target>.tar.gz`, with a `.sha256` next to each. Or build from crates.io:

```sh
cargo install agent-toolbox
```

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

### Proxy

Every request honours the standard proxy variables. The first valid one of `ALL_PROXY`, `HTTPS_PROXY` and `HTTP_PROXY` wins, in that order, and the lowercase spellings are accepted too. `NO_PROXY` lists the hosts that bypass the proxy. An HTTP `CONNECT` proxy is what has been tested.

## `atb linear`

```sh
atb linear claim <ISSUE> --agent <name> --source <source> --scope '<repo>: <paths>'
atb linear release <ISSUE> --agent <name> (--reason <reason> | --force <why>) [--done | --todo]
atb linear create --team <KEY> [--project <name>] --title <title> --description-file <file> [--label <name>]... [--json]
atb linear project create --team <KEY> --name <name> [--description-file <file>] [--json]
atb linear query <GRAPHQL | FILE>
```

Claims and releases Linear issues by comment, so that agents sharing one Linear account can see who is working on what; creates issues; creates a project unless one with the name exists (a same-named project that is archived, on another team, or one of several is an error and nothing is written; one in the trash does not count); runs read-only GraphQL queries (a document with a mutation or subscription is refused before it is sent). The protocol, the exit statuses (3: claim lost, 4: release refused) and example queries are in [`.agents/skills/linear/SKILL.md`](.agents/skills/linear/SKILL.md).

The API key comes from the environment, never from a flag:

1. `LINEAR_API_KEY`: the key itself.
2. `LINEAR_API_KEY_CMD`: a command that prints the key, such as a secret manager's read command. It runs through `sh -c` and its stderr is discarded. The output is cached for 24 hours in `$XDG_CONFIG_HOME/linear/api-key` (default `~/.config/linear/api-key`, under `$ATB_HOME` if set), mode 0600 in a 0700 directory; an existing directory is tightened to 0700. On a 401 the command runs once more and the request is retried once.

States are chosen by type, not by name: `claim` sets the first `started` state and records the prior one in its comment, `release --done` sets the first `completed` state, and `release` without it restores the recorded state (falling back to the first `unstarted`, then the first `backlog` state). An optional config file beside the key cache, `$XDG_CONFIG_HOME/linear/config.json` (default `~/.config/linear/config.json`, under `$ATB_HOME` if set), names the state to use for a type and labels that `create` always adds:

```json
{"states": {"started": "Active"}, "default_labels": ["bug"]}
```

Both keys are optional and the state names are examples; the skill has the details.

`LINEAR_API_URL` overrides the endpoint (`https://api.linear.app/graphql`). Each request has a 30 s timeout and follows no redirects.

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
