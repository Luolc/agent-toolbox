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
atb linear release <ISSUE> --agent <name> (--reason <reason> | --force <why>) [--done | --abandon | --todo]
atb linear comment <ISSUE> --body-file <file> [--json]
atb linear create --team <KEY> [--project <name>] [--parent <ISSUE>] --title <title> --description-file <file> [--label <name>]... [--json]
atb linear project create --team <KEY> --name <name> [--description-file <file>] [--json]
atb linear set-project <ISSUE> --project <name> [--json]
atb linear relate <ISSUE> <OTHER> [--json]
atb linear query <GRAPHQL | FILE>
```

Claims and releases Linear issues by comment, so that agents sharing one Linear account can see who is working on what; writes other comments; creates issues, optionally as sub-issues of a parent; creates a project unless one with the name exists; puts an issue into a project; relates two issues; runs read-only GraphQL queries. Examples below use `ABC-123` for an issue and `TEAM` for a team key.

```sh
atb linear claim ABC-123 --agent docs-impl --source thread-42 --scope 'my-repo: src/, docs/'
atb linear release ABC-123 --agent docs-impl --reason merged --done
atb linear release ABC-123 --agent docs-impl --reason 'blocked on review'
atb linear release ABC-123 --agent docs-impl --reason 'approach does not work' --abandon
atb linear release ABC-123 --agent my-orchestra --force 'stale for three days, holder gone'
atb linear comment ABC-123 --body-file notes.md
atb linear create --team TEAM --project 'Project name' --title 'Short title' --description-file body.md --label bug
atb linear create --team TEAM --parent ABC-123 --title 'Second attempt' --description-file body.md
atb linear project create --team TEAM --name 'Project name' --description-file overview.md
atb linear set-project ABC-123 --project 'Project name'
atb linear relate ABC-123 ABC-124
atb linear query '{ viewer { id } }'
```

### Claim, conflict and holder

`claim` sets the issue to the team's first `started` state, then writes one comment: line 1 `claim: <agent> <source>` (the source is a thread key or the name of the dispatching agent), line 2 `scope: <repo>: <paths>`, line 3 `from: <state>`, the state the issue was in before. After writing it, `claim` reads back every comment, ordered by `createdAt`, then by comment id. If an earlier `claim:` by another agent has no later `release:` by that agent, the earlier claim wins: `claim` writes `release: <agent> lost` and exits 3. An earlier claim by the same agent is not a conflict. This is not a lock: two agents can still claim within the same moment, and the work then shows as a merge conflict on a pull request.

The current holder is the agent named in the latest `claim:` comment that has no later `release:` comment by the same agent. `release: <agent> lost`, `release: <agent> abandoned: ...` and `release: <agent> forced by ...` count as releases by `<agent>`. With no such claim the issue has no holder. If a `claim` fails after its comment was written but before the conflict check finished, it says so on stderr; run it again or release it.

### Release

`release` needs exactly one of `--reason <reason>` (the holder releases its own claim) or `--force <why>` (any agent releases the holder's claim; the comment reads `release: <holder> forced by <agent>: <why>`). The comment is written first, then the state is set. When the release is refused, nothing is written. With `--done` the state is the first `completed` one. With `--abandon` the comment reads `release: <agent> abandoned: <reason>` (with `--force` the forced comment is unchanged) and the state is the one named by `states.abandoned` in the config; `--abandon` combines with `--reason` or `--force`, not with `--done`, and exits 1 before writing anything if `states.abandoned` is unset or names a state that is missing or not of type `canceled`. Without either, the state the claim recorded in `from:` is restored; if the issue was already in a `started` state when claimed (for example after a lost claim), `from:` records that state and it is the one restored. `--todo` is deprecated: it is accepted and does nothing, since restoring is the default.

### Comment, create, project, set-project, relate, query

- `comment` writes the file's content as one comment, verbatim, and prints its URL (`{"id", "url"}` with `--json`). It refuses a body that is empty or whitespace only, or whose first line, after leading whitespace, starts with `claim:` or `release:` (exit 2) before the key is read, so nothing is sent.
- `create` resolves the team by key and the project by name within that team (missing or ambiguous: error) and prints `<identifier> <url>`, or JSON with `--json`. It adds the labels given with `--label` (repeatable) and those in `default_labels` of the config file, each once; with neither, the issue is created without labels. A label that is neither a workspace label nor the team's own is created on the team. `--parent <ISSUE>` creates a sub-issue of that issue, looked up before anything is written (missing: exit 1, nothing created); the parent may be on another team, and the new issue goes on `--team` either way.
- `project create` is idempotent by name. It looks up every project named exactly `<name>` (case-sensitive), archived ones included and projects in the trash left out. None: it creates the project on the team, with the file's Markdown as the project's content (Linear's short `description` stays empty), and prints `<name> <url>`. Exactly one, on the team and not archived: it prints that project, says on stderr that it already exists and changes nothing; it never adds the team to an existing project. An archived one, one not on the team, or more than one: exit 1, nothing written. `--json` prints `{"name", "url", "id", "created"}`.
- `set-project` puts an issue that is in no project into a project of the issue's team, named exactly `<name>` (case-sensitive), and prints `<identifier> <project name> <project url>`. Archived projects, those in the trash included, do not count. The issue and the project are looked up before anything is written: a missing issue, or a team with no such project or more than one, is exit 1 and nothing is written. Already in that project: it prints the same, says so on stderr and changes nothing. In another project: exit 1, nothing written, and the message names the current project; it never moves an issue between projects. `--json` prints `{"identifier", "project", "url", "changed"}`.
- `relate` adds one `related` relation between two issues and prints both identifiers. Both are looked up first (missing: exit 1, nothing written). If a relation of any type already links them, in either direction, it prints the same, names that relation's type and direction on stderr and writes nothing; a `blocks`, `duplicate` or `similar` relation is left as it is and no `related` one is added. The same issue twice is exit 2: two identifiers equal ignoring case are refused before anything is sent, and two arguments that resolve to one issue (an id and its identifier) after the lookups, with nothing written. `--json` prints `{"issue", "other", "type", "created"}`, where `type` is the existing relation's type when nothing was created.
- `query` takes a file path if such a file exists, otherwise the query text, and prints the response's `data` as JSON. It is read-only: a document containing a `mutation` or `subscription` operation is refused before anything is sent.

Example queries (the filters follow Linear's schema and have not been run against a real workspace):

```sh
# Open issues assigned to the shared account
atb linear query '{ viewer { assignedIssues(filter: {state: {type: {nin: ["completed", "canceled"]}}}) { nodes { identifier title state { name } } } } }'
# Comments on one issue: who holds it
atb linear query '{ issue(id: "ABC-123") { comments { nodes { body createdAt } } } }'
```

### Exit statuses

| Exit status | Meaning |
|---|---|
| 0 | Success (`claim`: the issue is yours) |
| 1 | Error: no key, issue or parent not found, comment file unreadable, `release --abandon` without a configured canceled state, a project with the name is archived, off the team or exists more than once, `set-project` on an issue in another project or with no single project of that name on the team, no state of the needed type or override, unreadable config, Linear refused a change, HTTP or GraphQL error |
| 2 | Usage error (including a `comment` body that is empty or starts with `claim:` or `release:`, and `relate` given the same issue twice), or Linear answered 429 (the `retry-after` value is printed; nothing is retried) |
| 3 | `claim` lost to an earlier claim; `release: <agent> lost` is written |
| 4 | `release` refused: the issue has no holder, or the holder is another agent and `--force` was not given |

### API key

The API key comes from the environment, never from a flag:

1. `LINEAR_API_KEY`: the key itself. A 401 is an error.
2. `LINEAR_API_KEY_CMD`: a command that prints the key, such as a secret manager's read command. It runs through `sh -c` and its stderr is discarded. The trimmed output is cached for 24 hours in `$XDG_CONFIG_HOME/linear/api-key` (default `~/.config/linear/api-key`, under `$ATB_HOME` if set), mode 0600 in a 0700 directory; an existing directory is tightened to 0700. On a 401 the command runs once more and the request is retried once; a second 401 is an error.

To check that the key works without printing it: `atb linear query '{ viewer { id } }' >/dev/null && echo usable || echo unusable`.

### States

States are chosen by type, not by name. Among the team's states of one type the first is the one with the lowest position (the order Linear shows), ties broken by name, then id.

| Command | State |
|---|---|
| `claim` | the first `started` state |
| `release --done` | the first `completed` state |
| `release --abandon` | the state named by `states.abandoned`, which must be of type `canceled`; no default |
| `release` (giving up, or `--force`) | the state in the claim's `from:` line; if there is none (a claim written by 0.2.0) or the team no longer has a state of that name, the first `unstarted` state, else the first `backlog` state |

When `release` finds no state to restore, it has still written its comment; it then exits 1 and leaves the state alone.

### Configuration

An optional config file beside the key cache, `$XDG_CONFIG_HOME/linear/config.json` (default `~/.config/linear/config.json`, under `$ATB_HOME` if set), names the state to use for a type (`states`) and the labels `create` always adds (`default_labels`). Both keys are optional and a missing file is an empty config:

```json
{"states": {"started": "Active", "unstarted": "Ready", "abandoned": "Abandoned"}, "default_labels": ["bug"]}
```

`abandoned` is not a state type: it names the `canceled` state that `release --abandon` sets, and a team may have several canceled states, so there is no fallback. An override applies wherever its type is used, the release fallbacks included. An override naming a state the team does not have, with that type, is an error and nothing is changed (for `release`, after its comment). A config file that cannot be read or parsed is an error naming the file.

### Endpoint

`LINEAR_API_URL` overrides the endpoint (`https://api.linear.app/graphql`). Each request has a 30 s timeout and follows no redirects. A 429 is reported with its `retry-after` value and is never retried.

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
