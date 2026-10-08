---
name: atb-pr-review
description: Review increments specific to agent-toolbox, layered on the user-level pr-review skill; load when reviewing any PR in this repository.
---

# agent-toolbox PR review increments

Layered on the user-level `~/.agents/skills/pr-review` (general track, priorities, output format). This file only lists what is specific here.

- Could a credential value reach output, logs, error messages, panics or test fixtures? Tests must use synthetic files under an overridden home.
- Is the repository still free of personal information, private hostnames and private vault references? This repository is public.
- Does the change add a daemon, retry loop or polling inside the binary? Those belong to consuming repositories.
- New HTTP calls: a fixed, bounded set of requests per command (pagination bounded by the server's `hasNextPage`), explicit timeout, no redirects, `429` surfaced with `retry-after` and exit status 2, no retry. The only allowed retry is `atb linear`'s single key refresh and resend after a 401.
- Ported behaviour from `lo-coding-agent-usage` must keep its numbers: same dedup keys, same weighting, same window naming; the port's tests should reuse the Python fixtures.
