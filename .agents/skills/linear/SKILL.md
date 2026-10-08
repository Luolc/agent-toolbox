---
name: linear
description: Claim and release Linear issues with `atb linear`, create issues and projects, and run read-only GraphQL queries. Use before an agent starts work on a repository whose Linear issue it must claim, when it finishes or gives up (release), when it files a new issue or creates a project for agent work, and to look up issues by assignee, label or state.
---

# Linear claims and queries

`atb linear` writes claim and release comments in a fixed format, sets the workflow state, and checks for a conflicting claim. Everything below uses placeholders: `ABC-123` for an issue, `TEAM` for a team key; state names such as `Todo` are examples, since every workspace names its states its own way.

All agents share one Linear account, so the assignee does not say who is working on an issue; the comments do.

## Rules

1. **Claim**: `atb linear claim` sets the issue to the team's first `started` state, then writes one comment: line 1 `claim: <agent> <source>`, line 2 `scope: <repo>: <paths>`, line 3 `from: <state>`, the state the issue was in before the claim. The source is the thread key or the name of the dispatching agent. The claim counts only when both steps are done. An issue in a `started` state is being worked on; the state's name does not matter.
2. **Conflict**: after writing the comment, `claim` reads back every comment, ordered by `createdAt`, then by comment id. If an earlier `claim:` by another agent has no later `release:` by that agent, the earlier claim wins: `claim` writes `release: <agent> lost` and exits 3. Stop work on that issue. An earlier claim by the same agent is not a conflict.
3. **Release**: when done or giving up, `atb linear release` writes `release: <agent> <reason>`, then sets the state: with `--done`, the first `completed` state; otherwise it restores the state the claim recorded (see [States](#states)). Only the current holder may release (see below).
4. **Stale claims**: never take over. A `claim:` older than 24 hours on an issue with no other update is a suspected leftover: report it to the dispatching agent, which decides. Only that decision justifies `release --force`.
5. **Not a lock**: two agents can still claim within the same moment. The work ends up in git, so a collision shows as a merge conflict on a pull request, visible rather than silent. A soft rule is enough.
6. **Only the commands**: claim and release only with `atb linear claim` and `atb linear release`; never write `claim:` or `release:` comments by hand.
7. **Scope does not open conflict magnets**: a scope line does not entitle an agent to the files that the global rule on conflict magnets (R59) serializes: `AGENTS.md`, `.agents/skills/`, dependency lock files stay separate tasks, run one at a time. When your scope overlaps a scope that is still held, the later claimant yields.

**Current holder**: the agent named in the latest `claim:` comment that has no later `release:` comment by the same agent. `release: <agent> lost` and `release: <agent> forced by ...` are releases by `<agent>`. No such claim: the issue has no holder.

## Commands

```sh
atb linear claim ABC-123 --agent docs-impl --source thread-42 --scope 'my-repo: src/, docs/'
atb linear release ABC-123 --agent docs-impl --reason merged --done
atb linear release ABC-123 --agent docs-impl --reason 'blocked on review'
atb linear release ABC-123 --agent my-orchestra --force 'stale for three days, holder gone'
atb linear create --team TEAM --project 'Project name' --title 'Short title' --description-file body.md --label bug
atb linear project create --team TEAM --name 'Project name' --description-file overview.md
atb linear query '{ viewer { id } }'
```

- `<ISSUE>` is the Linear identifier, such as `ABC-123`.
- `release` needs exactly one of `--reason <reason>` (the holder releases its own claim) or `--force <why>` (any agent releases the current holder's claim; the comment reads `release: <holder> forced by <agent>: <why>`). The comment comes first, then the state. When refused, nothing is written and the state is unchanged. `--todo` is deprecated: it is accepted so that 0.2.0 callers keep working and does nothing, restoring is the default.
- `create` resolves the team by key and the project by name within that team (missing or ambiguous: error) and prints `<identifier> <url>`, or JSON with `--json`. It adds the labels given with `--label <name>` (repeatable) and those in `default_labels` of the config file, each once; a label that is neither a workspace label nor the team's own is created on the team. With no labels, the issue has none.
- `project create` is idempotent by name: it looks up every project named exactly `<name>` (case-sensitive), archived ones included and projects in the trash left out, and writes only when there is none. None: it creates the project on the team, with the file's Markdown as the project's content (the long body; Linear's short `description` stays empty), and prints `<name> <url>`. Exactly one, on the team: it prints that project and changes nothing (a note on stderr says it already existed). One that is archived, whether on the team or not: exit 1, nothing written; unarchive it in Linear or choose another name (exit 0 would mean filing work into an archived project). One that is not on the team, or more than one, archived ones counted: exit 1, nothing written; the team is never added to an existing project. A project in the trash (deleted) does not count, so a deleted name can be created again. `--json` prints `{"name", "url", "id", "created"}`, where `created` is false for an existing project. Run it again after any failure: it never creates a second project with the name.
- `query` takes a file path if such a file exists, otherwise the query text, and prints the response's `data` as JSON. It is read-only: a document containing a `mutation` or `subscription` operation is refused before anything is sent.

| Exit status | Meaning |
|---|---|
| 0 | Success (`claim`: the issue is yours) |
| 1 | Error: no key, issue not found, a project with the name is archived, off the team or exists more than once, no state of the needed type or override, unreadable config, Linear refused a change, HTTP or GraphQL error |
| 2 | Usage error, or Linear answered 429 (the `retry-after` value is printed; nothing is retried) |
| 3 | `claim` lost to an earlier claim; `release: <agent> lost` is written, leave the issue alone |
| 4 | `release` refused: the issue has no holder, or the holder is another agent and `--force` was not given |

## States

States are chosen by type, never by name. Among the team's states of one type, the first is the one with the lowest position (the order Linear shows them in), ties broken by name, then id, so every agent picks the same one.

| Command | State |
|---|---|
| `claim` | the first `started` state |
| `release --done` | the first `completed` state |
| `release` (giving up, or `--force`) | the state named in the claim's `from:` line; if there is none (a claim written by 0.2.0) or the team no longer has a state of that name, the first `unstarted` state, else the first `backlog` state |

When `release` finds no state to restore, it has still written its comment; it then exits 1 and leaves the state alone. If the issue was already `started` when it was claimed (for example after a claim that lost), `from:` records that state and a release restores it.

The config file `$XDG_CONFIG_HOME/linear/config.json` (default `~/.config/linear/config.json`, under `$ATB_HOME` if set, beside the key cache) can name the state to use for a type, and labels for `create`. Both keys are optional; a missing file is an empty config. For example:

```json
{"states": {"started": "Active", "unstarted": "Ready"}, "default_labels": ["bug"]}
```

An override applies wherever its type is used, the release fallbacks included: with the example, a release without a `from:` state restores `Ready`. An override naming a state the team does not have, with that type, is an error and nothing is changed (for `release`, after its comment). A config file that cannot be read or parsed is an error naming the file.

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
# Issues with one label
atb linear query '{ issues(filter: {labels: {name: {eq: "bug"}}}) { nodes { identifier title state { name } } } }'
# Issues being worked on, whatever the state is called
atb linear query '{ issues(filter: {state: {type: {eq: "started"}}}) { nodes { identifier title state { name } } } }'
# Comments on one issue: who holds it
atb linear query '{ issue(id: "ABC-123") { comments { nodes { body createdAt } } } }'
```
