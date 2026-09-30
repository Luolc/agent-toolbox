# Releasing

A release is one tag push. `.github/workflows/release.yml` then checks the tag against `Cargo.toml`, creates the GitHub Release, attaches the binaries and publishes the crate to crates.io. The exception is the very first version: crates.io only accepts Trusted Publishing for a crate that already exists, so 0.1.0 goes to crates.io by hand, once (section 2).

Releasing is the maintainer's call: an agent pushes a tag or publishes only on the maintainer's explicit instruction.

The workflow was written after limae's; its [release manual](https://github.com/Luolc/limae/blob/main/docs/knowledge/release.md) (Chinese) explains the design choices in more detail.

## 1. Preconditions

On the machine you release from, in a clean checkout of this repository:

```sh
git fetch origin && git status --porcelain && git rev-parse HEAD origin/main
```

Done when `git status --porcelain` prints nothing and the two SHAs are equal. Then run the checks from `AGENTS.md` bare (no pipes) and read each exit code; all must be 0.

The tag is `v` followed by `version` in `Cargo.toml`, character for character. The workflow's `guard` job stops the whole run otherwise, before anything is created.

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

Done when it exits 0 (on v0.1.0: every job but `publish` succeeded). If a job failed and was re-run, `gh run list` shows only the last attempt; read `gh api repos/Luolc/agent-toolbox/actions/runs/<id> --jq .run_attempt` before calling the run green.

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

The same workflow has a `workflow_dispatch` entry that runs the guard against a tag given as input and builds, packages and checks all four targets, but creates no Release, uploads nothing and does not publish.

```sh
gh workflow run release.yml --ref main -f tag=v0.1.0
```
