---
name: atb-pr-review
description: Review increments specific to agent-toolbox; load when reviewing any PR in this repository.
---

# agent-toolbox PR review increments

What is specific to this repository.

- Could a credential value reach output, logs, error messages, panics or test fixtures? Tests must use synthetic files under an overridden home.
- Does the change add a daemon, retry loop or polling inside the binary? Those belong to consuming repositories.
- New HTTP calls: a fixed, bounded set of requests per command (pagination bounded by the server's `hasNextPage`), explicit timeout, no redirects, `429` surfaced with `retry-after` and exit status 2, no retry. The only allowed retry is `atb linear`'s single key refresh and resend after a 401.
