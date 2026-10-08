---
name: linear
description: Claim and release Linear issues with `atb linear`, create issues for agents, and run read-only GraphQL queries. Use before an agent starts work on a repository whose Linear issue it must claim, when it finishes or gives up (release), when it files a new issue for agent work, and to look up issues by assignee, label or state.
---

# Linear claims and queries

`atb linear` writes claim and release comments in a fixed format, sets the workflow state, and checks for a conflicting claim. Everything below uses placeholders: `ABC-123` for an issue, `TEAM` for a team key.

All agents share one Linear account, so the assignee does not say who is working on an issue; the comments do. Issues created by agents carry the `agent` label.

## Rules

1. **Claim**: `atb linear claim` sets the issue to In Progress, then writes one comment: line 1 `claim: <agent> <source>`, line 2 `scope: <repo>: <paths>`. The source is the thread key or the name of the dispatching agent. The claim counts only when both steps are done.
2. **Conflict**: after writing the comment, `claim` reads back every comment, ordered by `createdAt`, then by comment id. If an earlier `claim:` by another agent has no later `release:` by that agent, the earlier claim wins: `claim` writes `release: <agent> lost` and exits 3. Stop work on that issue. An earlier claim by the same agent is not a conflict.
3. **Release**: when done or giving up, `atb linear release` writes `release: <agent> <reason>`, then sets Done or Todo if asked. Only the current holder may release (see below).
4. **Stale claims**: never take over. A `claim:` older than 24 hours on an issue with no other update is a suspected leftover: report it to the dispatching agent, which decides. Only that decision justifies `release --force`.
5. **Not a lock**: two agents can still claim within the same moment. The work ends up in git, so a collision shows as a merge conflict on a pull request, visible rather than silent. A soft rule is enough.
6. **Only the commands**: claim and release only with `atb linear claim` and `atb linear release`; never write `claim:` or `release:` comments by hand.
7. **Scope does not open conflict magnets**: a scope line does not entitle an agent to the files that the global rule on conflict magnets (R59) serializes: `AGENTS.md`, `.agents/skills/`, dependency lock files stay separate tasks, run one at a time. When your scope overlaps a scope that is still held, the later claimant yields.

**Current holder**: the agent named in the latest `claim:` comment that has no later `release:` comment by the same agent. `release: <agent> lost` and `release: <agent> forced by ...` are releases by `<agent>`. No such claim: the issue has no holder.

## Commands

```sh
atb linear claim ABC-123 --agent docs-impl --source thread-42 --scope 'my-repo: src/, docs/'
atb linear release ABC-123 --agent docs-impl --reason merged --done
atb linear release ABC-123 --agent docs-impl --reason 'blocked on review' --todo
atb linear release ABC-123 --agent my-orchestra --force 'stale for three days, holder gone' --todo
atb linear create --team TEAM --project 'Project name' --title 'Short title' --description-file body.md
atb linear query '{ viewer { id } }'
```

- `<ISSUE>` is the Linear identifier, such as `ABC-123`. States are looked up by name (`In Progress`, `Done`, `Todo`) among the workflow states of the issue's team; a missing state is an error, never a guess.
- `release` needs exactly one of `--reason <reason>` (the holder releases its own claim) or `--force <why>` (any agent releases the current holder's claim; the comment reads `release: <holder> forced by <agent>: <why>`). `--done` or `--todo` sets the state after the comment; with neither, the state stays as it is. When refused, nothing is written and the state is unchanged.
- `create` resolves the team by key and the project by name within that team (missing or ambiguous: error), uses an `agent` label (a workspace label or the team's own, created on the team if there is none), and prints `<identifier> <url>`, or JSON with `--json`.
- `query` takes a file path if such a file exists, otherwise the query text, and prints the response's `data` as JSON. It is read-only: a document containing a `mutation` or `subscription` operation is refused before anything is sent.

| Exit status | Meaning |
|---|---|
| 0 | Success (`claim`: the issue is yours) |
| 1 | Error: no key, issue or state not found, Linear refused a change, HTTP or GraphQL error |
| 2 | Usage error, or Linear answered 429 (the `retry-after` value is printed; nothing is retried) |
| 3 | `claim` lost to an earlier claim; `release: <agent> lost` is written, leave the issue alone |
| 4 | `release` refused: the issue has no holder, or the holder is another agent and `--force` was not given |

If a `claim` fails after its comment was written but before the conflict check finished, it says so on stderr: a claim comment may be on the issue without a check. Run `claim` again (an earlier claim by the same agent is not a conflict), or release it.

## Key configuration

The key is read from the environment only; there is no flag. Never put the key itself in a file you commit, a command line or a message.

1. `LINEAR_API_KEY`: the key, for example injected by a secret manager's run wrapper. A 401 is an error.
2. `LINEAR_API_KEY_CMD`: a command that prints the key on stdout, typically a secret manager's read command for the item that holds it. It runs through `sh -c`; its stderr is discarded and never shown. The trimmed output is cached for 24 hours in `$XDG_CONFIG_HOME/linear/api-key` (default `~/.config/linear/api-key`, under `$ATB_HOME` if set), mode 0600 in a 0700 directory. On a 401, the command runs once more and the request is retried once; a second 401 is an error.

Check that the key works without printing it:

```sh
atb linear query '{ viewer { id } }' >/dev/null && echo usable || echo unusable
```

## Read-only queries

The filters follow Linear's schema; they have not yet been run against a real workspace.

```sh
# Open issues assigned to the shared account
atb linear query '{ viewer { assignedIssues(filter: {state: {type: {nin: ["completed", "canceled"]}}}) { nodes { identifier title state { name } } } } }'
# Issues with the agent label
atb linear query '{ issues(filter: {labels: {name: {eq: "agent"}}}) { nodes { identifier title state { name } } } }'
# Issues in one state
atb linear query '{ issues(filter: {state: {name: {eq: "In Progress"}}}) { nodes { identifier title } } }'
# Comments on one issue: who holds it
atb linear query '{ issue(id: "ABC-123") { comments { nodes { body createdAt } } } }'
```
