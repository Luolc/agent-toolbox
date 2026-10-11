# Releasing

A release is one tag push. `.github/workflows/release.yml` then checks the tag against `Cargo.toml`, checks that the tagged commit is on `main` and that its CI run on `main` passed at the first attempt, creates the GitHub Release, attaches the binaries and publishes the crate to crates.io. The exception is the very first version: crates.io only accepts Trusted Publishing for a crate that already exists, so 0.1.0 goes to crates.io by hand, once (section 2).

Merging a pull request does not ship it. A change reaches users only in a release, and a machine that already has `atb` installed picks it up only after it is upgraded to that version.

After v0.2.0, `atb linear` changes are released as patch versions (0.2.1, 0.2.2, ...).

The workflow was written after limae's; its [release manual](https://github.com/Luolc/limae/blob/main/docs/knowledge/release.md) (Chinese) explains the design choices in more detail.

## 1. Preconditions

On the machine you release from, in a clean checkout of this repository:

```sh
git fetch origin && git status --porcelain && git rev-parse HEAD origin/main
```

Done when `git status --porcelain` prints nothing and the two SHAs are equal. Then check the CI run that the push of that commit to `main` started, with the script of the `release-ci-green` action that the workflow's `ci-green` job runs. Run it as its README's ["Run it locally"](https://github.com/Luolc/ci-actions#run-it-locally) says, at the SHA `release.yml` pins, with `RELEASE_CI_GREEN_REPO=Luolc/agent-toolbox`.

Done when it exits 0 after printing `conclusion success, run_attempt 1`. Any other exit means this commit is not released. When the run passed only at a later attempt, read the first attempt's log to find out why it failed; then release a later commit on `main` that passes.

The tag is `v` followed by `version` in `Cargo.toml`, character for character, on a commit that meets the above. The workflow checks all three before anything is created: `guard` the tag, `on-main` that the commit is on `main`, and `ci-green` its CI run on `main` (waiting up to 30 minutes for a run still going); any of them failing stops the whole run.

## 2. First crates.io publish (once, for 0.1.0)

Trusted Publishing cannot be set up before the crate exists; in the crates.io docs' words, "initial publish requires an API token". So the first `cargo publish` runs on the maintainer's machine with an API token.

1. In the checkout from section 1, a local packaging check (it cannot tell whether crates.io will accept the manifest; only the real publish can):

   ```sh
   cargo publish --dry-run --locked
   ```

   Done when it exits 0 and ends with `aborting upload due to dry run`.

2. Publish, with the token passed as `CARGO_REGISTRY_TOKEN` in that one command's environment, injected by the maintainer's secret manager. Never `cargo login` (it writes the token to `~/.cargo/credentials.toml`) and never `--token <value>` (it lands in shell history and the process list).

   Done when `cargo publish` exits 0 and <https://crates.io/crates/agent-toolbox> shows the version from `Cargo.toml`.

## 3. Trusted Publisher (once, after the first publish)

In a browser, on <https://crates.io/crates/agent-toolbox>: Settings → Trusted Publishing → Add → GitHub, with these four values:

| Field | Value |
|---|---|
| Repository owner | `Luolc` |
| Repository name | `agent-toolbox` |
| Workflow filename | `release.yml` |
| Environment | leave empty (the workflow uses no environment) |

Done when the Trusted Publishing list shows an entry for `Luolc/agent-toolbox`. From then on the workflow's `publish` job exchanges its OIDC identity for a short-lived token, and no crates.io secret lives in the repository.

## 4. Push the tag

```sh
git tag v0.1.0 && git push origin v0.1.0
```

(Use the version from `Cargo.toml`.) Done when `git ls-remote --tags origin v0.1.0` prints one line.

For v0.1.0 the `publish` job is expected to fail, because 0.1.0 is already on crates.io from section 2. The Release and its binaries do not depend on that job.

## 5. Watch the workflow

```sh
gh run watch --exit-status "$(gh run list --workflow=release.yml --event=push --limit=1 --json databaseId --jq '.[0].databaseId')"
```

Done when it exits 0. On v0.1.0 it exits non-zero because `publish` fails (section 4); there, done when `gh run view <id> --json jobs --jq '.jobs[] | "\(.name): \(.conclusion)"'` shows `success` for every job except `publish`. If a job failed and was re-run, `gh run list` shows only the last attempt; read `gh api repos/Luolc/agent-toolbox/actions/runs/<id> --jq .run_attempt` before calling the run green.

## 6. Verify the assets

```sh
gh release view v0.1.0 --json assets --jq '.assets[].name'
```

Done when it lists eight names: `atb-<target>.tar.gz` and `atb-<target>.sha256` for each of `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`, `x86_64-apple-darwin` and `aarch64-apple-darwin`.

Then, in an empty directory, download everything and check each archive against its sidecar:

```sh
gh release download v0.1.0 --repo Luolc/agent-toolbox
for f in atb-*.sha256; do shasum -a 256 -c "$f"; done
```

Done when every line ends in `OK`. For each Linux archive, unpack it and check the binary is static, as a positive chain: `readelf -l` exits 0, its output contains `Program Headers`, and that output contains no `INTERP`:

```sh
mkdir x86_64 && tar -xzf atb-x86_64-unknown-linux-musl.tar.gz -C x86_64
readelf -l x86_64/atb > headers.txt; echo "readelf exit=$?"
grep -c 'Program Headers' headers.txt
grep -c INTERP headers.txt
```

Done when readelf exits 0, the first count is 1 and the second is 0. Repeat for `aarch64-unknown-linux-musl`. Last, on a machine that can execute it, `./atb --version` prints `atb` and the version.

## 7. Dry run without releasing

The same workflow has a `workflow_dispatch` entry that runs the guard against a tag given as input, runs `on-main` and `ci-green` against the commit it runs on, and builds, packages and checks all four targets, but creates no Release, uploads nothing and does not publish. Run it from `main`, as below; on a commit that is not on `main`, `on-main` fails.

```sh
gh workflow run release.yml --ref main -f tag=v0.1.0
```

GitHub dispatches a workflow only once it exists on the default branch. A pull request that edits `release.yml` runs the build half of the same dry run (all four targets, none of the three checks: there is no tag to compare, and the pull request's commit is not on `main`).
