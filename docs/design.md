# Design

The current state of the code on `main`. It is edited in place, only in a design pass; history is in git and the pull requests.

## 1. What this is, and what it is not

`atb` is one Rust binary with subcommands, each a self-contained tool. Two tools exist:

- `atb quota <claude|codex|grok>` reads the subscription quota a vendor reports: window percentages and reset times. It is not a token ledger and not a bill.
- `atb linear` claims and releases Linear issues by comment, writes comments, creates issues (also as sub-issues) and projects, edits an issue's title, description and labels, puts an issue into a project, relates two issues, and runs read-only GraphQL queries.

Each command reads local files or makes a fixed, bounded set of HTTP calls. There is no daemon, no polling and no retry loop. The single retry is the `atb linear` key refresh on a 401. Anything that must run continuously belongs to a consuming repository, as a scheduled job that calls this binary. The same goes for machine-specific wiring, secret-manager references and workspace conventions: none of it is in this repository.

The tools never refresh a vendor login, never call the Codex reset-credit endpoints, and never write to a vendor credential file.

## 2. Parts and how they connect

```
atb (src/main.rs)
 |-- clap command tree, exit-status mapping (1 error, 2 rate limited)
 |-- quota (src/quota/)             linear (src/linear/)
 |     mod.rs     args, output       mod.rs      commands, protocol, client
 |     claude.rs  oauth usage        key.rs      key sources, cache, masking
 |     codex.rs   usage API,         states.rs   state choice by type
 |                rollout fallback   config.rs   config.json
 |     grok.rs    billing            document.rs GraphQL operation scanner
 |           \                       /
 |            src/common.rs: Context (credential locations), Secret, Headers,
 |            http_get_json / http_post_json, version probe, table and JSON output
 +-- src/timefmt.rs: UTC timestamps with std only

.github/workflows/ci.yml       check: gitleaks, fmt, clippy, test, doc, design length
.github/workflows/release.yml  guard -> create-release, build x4 -> upload-assets, publish
```

`main.rs` parses the command line and turns an `Error` into an exit status. `common.rs` holds everything both tools share: where credentials live (`Context`, which reads variables through one place so tests can replace them), the `Secret` and `Headers` types whose `Debug` output hides values, and two HTTP helpers that issue exactly one request, follow no redirect and hand the status back. A quota module builds its vendor's headers, makes one GET and maps the answer to a record; Codex falls back to the newest local rollout snapshot when the usage API gives no answer. The Linear client wraps one GraphQL POST, resolves the key (`LINEAR_API_KEY`, or the cached output of `LINEAR_API_KEY_CMD`), masks every key it holds in all output, and implements the claim protocol on top. The release workflow checks the tag against the manifest, builds four static or native targets, and publishes through Trusted Publishing.

## 3. Invariants

Each one names the test that guards it. "Untested" means no test in this repository guards it.

Credentials and output:

- A quota credential never appears in output: `Secret` and `Headers` hide their values under `Debug` (`common::tests::debug_output_never_shows_a_credential`), and an error about a credential names the path or variable, not the content (`tests/quota.rs`: `token_variable_without_its_id_fails_without_reading_any_file`).
- Every line `atb linear` prints is masked against every key held during the run (`tests/linear.rs`: `a_key_echoed_by_the_server_is_masked_on_stdout_and_stderr`; ordering in `linear::key::tests::masks_the_longer_value_whole_when_one_contains_the_other`). A failing key command shows only its exit status (`a_failing_key_command_reports_only_its_exit_status`).
- The `--version` child of `atb quota` gets no `ATB_*` variable (`tests/quota.rs`: `version_probe_child_does_not_inherit_credential_variables`).
- Credential locations resolve flag, then harness variable, then `ATB_HOME`, then the home; an empty variable is unset (`common::tests::harness_dir_precedence_is_flag_then_harness_variable_then_atb_home`, `each_harness_reads_its_own_variable_and_empty_counts_as_unset`; `tests/quota.rs`: `claude_credential_directory_follows_the_documented_precedence`). A token variable beats every file and no file is read (`quota::claude::tests::token_variable_wins_over_the_flag_and_reads_no_file`).

Quota requests:

- One request per quota call, no redirect followed, a 429 is exit 2 with `retry-after` and no retry (`http_get_json` and `http_agent` in `src/common.rs`): untested.
- Codex answers from the usage API, and falls back to the rollout snapshot when the API gives no answer, except on a 429 and except a token without its account id (exit 1). The fallback is covered (`tests/quota.rs`: `codex_falls_back_to_the_newest_rollout_snapshot`, `codex_without_any_snapshot_is_a_clear_error`); a response of the wrong shape is rejected so it falls back (`quota::codex::tests::usage_response_of_another_shape_is_rejected`). The 429 exception is untested.

Linear protocol:

- `claim` after another agent's unreleased claim writes `release: <agent> lost` and exits 3; after a released claim, or its own, it wins (`tests/linear.rs`: `claim_after_another_agents_unreleased_claim_loses`, `claim_after_a_released_claim_or_its_own_claim_wins`). The holder is the latest `claim:` not released by the same agent (`linear::tests::holder_is_the_latest_claim_not_released_by_its_agent`).
- `release` by anyone but the holder, or with no holder, exits 4 and writes nothing (`release_by_a_non_holder_or_without_a_holder_changes_nothing`). The comment is written before the state changes (`release_by_the_holder_comments_then_sets_the_first_completed_state`).
- The one exception to "no holder": a release this agent interrupted, where the latest claim or release comment is that agent's release (for `--force`, one forced by that agent) and the issue is still in a `started` state, is resumed: it sets the state, writes no comment and exits 0 (`an_interrupted_release_is_resumed_without_a_second_comment`, `an_interrupted_forced_release_is_resumed_only_by_the_forcing_agent`). Another agent's latest release, a state that moved on, or a later claim is not a resume (`release_without_a_holder_is_not_a_resume_unless_its_own_release_is_latest_and_started`).
- Every `release` resolves its target state before writing the comment; with no state to set it exits 1, writes nothing, and the holder keeps the issue (`release_without_a_state_to_set_writes_nothing_and_keeps_the_holder`).
- States are chosen by type, lowest position first, ties by name then id, with no state name in `src/` (`claim_picks_the_started_state_with_the_lowest_position_then_name`, `release_falls_back_to_the_first_unstarted_then_the_first_backlog_state`). `claim` records the prior state and `release` restores it (`release_restores_the_state_recorded_by_claim`). `states.abandoned` is the one key under `states` that is not a state type. A configured state must exist and have the type (`a_config_override_names_the_state_and_a_missing_one_is_an_error`); a malformed config is an error that does not echo its content (`a_malformed_config_is_an_error_naming_the_path_without_its_content`).
- `comment` writes the file verbatim and prints the comment's URL (`comment_writes_the_file_verbatim_and_prints_its_url`). It refuses an empty or whitespace-only body, or one whose first line starts with `claim:` or `release:`, before the key is read or any request is sent, because those lines drive the holder parser and come only from `claim` and `release` (`comment_refuses_a_claim_or_release_body_or_an_empty_file_before_any_request`). A missing issue writes nothing (`comment_on_a_missing_issue_writes_nothing`).
- `create --parent` looks the parent up before anything is written, sends its id, and leaves the issue on `--team`; a missing parent writes nothing (`create_with_a_parent_sends_the_parents_id`, `create_with_a_missing_parent_writes_nothing`).
- `release --abandon` sets the state named by `states.abandoned`, which must exist on the team and be of type `canceled`; there is no default and no fallback to the first canceled state. A missing or mistyped one writes nothing (`release_abandon_without_a_canceled_state_configured_writes_nothing`). The comment is `release: <agent> abandoned: <reason>`, or with `--force` names the holder (`release_abandon_by_the_holder_comments_then_sets_the_configured_canceled_state`, `forced_release_abandon_names_the_holder_and_sets_the_configured_state`). `--abandon` with `--done` is a usage error (`release_abandon_conflicts_with_done`). An `abandoned:` release still counts as a release by that agent (`linear::tests::holder_is_the_latest_claim_not_released_by_its_agent`).
- `edit` sends only the fields given, the description verbatim (`edit_sends_only_the_fields_given_and_the_description_verbatim`). Values equal to the current ones exit 0 and write nothing (`edit_with_the_current_values_writes_nothing`); the title is compared exactly, the description without trailing newlines on either side, because Linear stores it without them (`edit_compares_the_description_without_trailing_newlines`). A missing issue is exit 1 with no write (`edit_of_a_missing_issue_writes_nothing`, `edit_of_a_missing_issue_changes_no_label`). No field, an empty title or an empty description is exit 2 before any request or key command (`edit_refuses_no_change_or_an_empty_title_or_description_before_any_request`); so is an empty label name or a label both added and removed (`edit_refuses_an_empty_label_or_one_both_added_and_removed_before_the_key`), and more than 50 distinct label names, the page size of the one label read, so that read sees every label named (`edit_refuses_more_than_fifty_label_names_before_the_key`).
- `edit` changes only the labels named, by `addedLabelIds` and `removedLabelIds`, never a whole `labelIds` list, so a concurrent label change survives (`edit_adds_or_removes_only_the_labels_named`). Names compare exactly. Labels already in place write nothing and look nothing up (`edit_with_labels_already_in_place_writes_and_looks_up_nothing`); a missing label is resolved as for `create` (`edit_creates_a_missing_label_on_the_issues_team_then_adds_it`). Changed fields and labels go in one update, without fields already as given (`edit_sends_a_title_and_label_changes_in_one_write`).
- `query` refuses a document with a mutation or subscription before the key is read or anything is sent (`query_refuses_a_mutation_before_any_request_and_passes_a_query`; scanner in `linear::document::tests`).
- `create` adds no label unless asked; a missing label is created once (`create_without_labels_adds_none_and_looks_none_up`, `create_merges_flag_and_default_labels_and_creates_a_missing_one_once`).
- `project create` is idempotent. One live project of the name on the team is printed and nothing written; one on another team, several, or an archived one is exit 1 with no write; a trashed one is ignored; none is created once (`project_create_creates_a_missing_project_once_and_then_prints_it`, `project_create_prints_a_project_already_on_the_team_without_writing`, `project_create_refuses_a_project_on_another_team_or_several_with_the_name`, `project_create_refuses_an_archived_project_on_any_team`, `project_create_ignores_a_trashed_project_with_the_name`).

Linear key:

- A key from `LINEAR_API_KEY_CMD` is cached in a 0600 file inside a 0700 directory, and a fresh cache is used without running the command (`key_command_without_a_cache_runs_once_and_writes_a_private_cache`, `a_fresh_cache_is_used_without_running_the_command`). The 24-hour expiry is untested.
- A 401 on a cached key refetches once and retries once; a second 401 is an error; a run refreshes at most once (`a_401_on_a_cached_key_refetches_once_and_retries_once`, `a_second_401_is_an_error`, `a_run_refreshes_the_key_at_most_once`).

Release:

- A release runs only when the tag equals `v` plus the manifest version, and every later job needs that check (`guard` in `.github/workflows/release.yml`): untested here. `workflow_dispatch` from `main` runs the guard without releasing.

Repository:

- `docs/design.md` has at most 200 lines (`.github/scripts/design-length.test.sh`: exit 0 at 200, 1 at 201, 4 when unreadable).

## 4. Interfaces

- Commands and flags: `atb --help`, `atb quota --help`, `atb linear --help`.
- Credential sources, proxy, exit statuses and the Linear key: [README](../README.md), sections "`atb quota`" and "`atb linear`".
- The claim protocol, states and config file: [README](../README.md), section "`atb linear`".
- Contract: the `--json` output fields, the exit statuses and the `claim:` / `release:` comment formats of `atb linear` are what other tools rely on. Changing any of them is a breaking change and bumps the minor version. Details: [README](../README.md), section "`atb linear`".
- Cutting a release: [docs/knowledge/release.md](knowledge/release.md).
- Automation: [ci.yml](../.github/workflows/ci.yml), [release.yml](../.github/workflows/release.yml).

## 5. Known issues and next steps

- Whether Linear answers an invalid key with 401 or with 400 and an `AUTHENTICATION_ERROR` body is unknown; only a 401 triggers the key refresh (PR #5).
- That a trashed project comes back from `projects` only with `includeArchived: true` is assumed from the schema and untested against a real workspace (PR #7).
- If a losing claimant crashed before writing its `lost` line, `holder` names the later claimant while `claim`'s conflict check favours the earlier one (PR #5).
- Three invariants in section 3 have no test: the quota request rules, the Codex 429 exception and the release guard; the 24-hour key expiry is also untested.
- `edit` reads an issue's labels through `labels(filter: {name: {in: …}}) @include(if: …)`, taken from Linear's public schema and untested against a real workspace.
- The Codex `User-Agent` follows the upstream client's source and was not compared with a captured request (PR #1).
- Open work is tracked in Linear, not here.
