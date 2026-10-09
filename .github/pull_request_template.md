## What

## Why

Decisions and their reasons live here, not in `docs/design.md`.

## Design impact

The section or invariant of `docs/design.md` this changes and the test that guards it, or "none". Do not edit the design document outside a design pass.

## Checks

Each command run bare, with its exit code:

- `pre-commit run --all-files`
- `cargo clippy --all-targets --locked -- -D warnings`
- `cargo test --locked`
- Local leak pre-review: reviewer OK, head <sha>
