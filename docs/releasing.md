# Releases

The [Release Please workflow](../.github/workflows/release-please.yml) opens and
updates a release PR as changes merge into `main`. Merging that release PR
starts the [Release workflow](../.github/workflows/release.yml), which publishes
executable packages and a multi-platform image in GitHub Container Registry.
Both workflows use the repository's `GITHUB_TOKEN`; no registry password or
personal access token is required.

## Automatic release PRs

1. Merge changes into `main` using Conventional Commit titles. Release Please
   collects unreleased changes into one release PR, updating it as more changes
   merge. A `fix` bumps the patch version; `feat` bumps the minor version.
   Breaking changes bump the minor version before 1.0.0 and the major version
   afterward. Commits such as `docs` or `ci` alone do not normally require a new
   release.
2. Review the generated PR. Its Rust strategy updates `Cargo.toml`, `Cargo.lock`,
   `.release-please-manifest.json`, and `docs/changelog.md`. The empty initial
   manifest makes the first release `v0.1.0`, including the existing unreleased
   history. CI runs on the release PR through an explicit workflow dispatch.
3. Merge the release PR when ready. Release Please creates the version tag and
   a draft GitHub Release. It calls the publishing workflow directly, building
   and testing the tagged commit even if `main` advances. After all checks pass,
   the workflow attaches the archives, checksums, and sample configuration,
   publishes the container images, and publishes the draft release.

GitHub does not trigger tag workflows for tags created with `GITHUB_TOKEN`, so
the explicit workflow call is required. The manual tag path below remains
available for releases managed without Release Please.

Repository Settings > Actions > General must allow GitHub Actions to create
pull requests. The Release Please job needs permission to write contents,
issues, and pull requests. Its CI dispatch job also needs permission to write
Actions. Organization policies must allow these permissions.

## Check the release workflow

In GitHub Actions, select **Release**, then **Run workflow** and choose a branch
or tag. Leave **publish** unchecked for a build-only run. The optional
**release-tag** selects an existing version tag to build using the workflow
from the chosen branch. Otherwise, the selected branch or tag supplies both
the workflow and source. Build-only runs test all four executable packages and
both Linux container images. They upload a `release-assets` artifact containing
the archives, `config.toml.example`, and `SHA256SUMS`, and publish nothing.

The workflow reuses the full CI checks, builds optimized executables on native
runners, runs the CLI tests for each target, and tests an extracted package in a
temporary directory outside the checkout. Linux container tests use those same
executables through the Dockerfile's named build context. No release is
published until all checks, native packages, and container tests pass.

## Publish a version manually

1. Set the package version in `Cargo.toml` and update its entry in `Cargo.lock`.
   Use `MAJOR.MINOR.PATCH`, optionally followed by a SemVer prerelease such as
   `0.2.0-rc.1`. Build metadata such as `+build.1` is unsupported because Docker
   tags cannot contain `+`.
2. Merge the version change and other release changes into `main` using a
   Conventional Commit title, for example `chore(release): prepare v0.1.0`.
3. Tag the merged commit and push the tag:

   ```sh
   git switch main
   git pull --ff-only
   git tag -a v0.1.0 -m "chore(release): release v0.1.0"
   git push origin v0.1.0
   ```

The tag must exactly match `v` followed by the version in `Cargo.toml`. A
mismatch fails before builds start. The initial package version is `0.1.0`, so
`v0.1.0` can be the first release without a version bump.

The publishing jobs need permission to write repository contents and packages.
If organization policy restricts those permissions, allow them for this
repository. GitHub creates new container packages as private by default. After
the first release, make the `chunkify-analytics` package public in its package
settings if users should pull images without signing in.

## Published files and images

Each GitHub Release includes release notes, a separately downloadable
`config.toml.example`, `SHA256SUMS` covering the configuration and archives, and
these archives, with `VERSION` replaced by the Cargo version:

| Archive | Built on |
| --- | --- |
| `chunkify-analytics-VERSION-x86_64-unknown-linux-gnu.tar.gz` | Ubuntu 22.04 amd64 |
| `chunkify-analytics-VERSION-aarch64-unknown-linux-gnu.tar.gz` | Ubuntu 22.04 arm64 |
| `chunkify-analytics-VERSION-x86_64-apple-darwin.tar.gz` | macOS 15 Intel |
| `chunkify-analytics-VERSION-aarch64-apple-darwin.tar.gz` | macOS 15 Apple Silicon |

Each archive contains `bin/chunkify-analytics`, its matching DuckDB shared
library under `lib`, the example configuration, OpenAPI specification, README,
documentation, and license notices. It contains no data directory or
credentials. Keep `bin` and `lib` beside each other so the executable can find
DuckDB without an environment override. macOS packages are unsigned and are not
notarized.

The container image is published as:

```text
ghcr.io/chunkifydev/chunkify-analytics:VERSION
```

Each version tag supports `linux/amd64` and `linux/arm64`. Stable releases also
update `latest` and become the latest GitHub Release. Prereleases are marked as
prereleases on GitHub and do not update either latest pointer. Publishing an
older stable version also moves `latest`; use explicit version tags when
deployments must stay on a particular version.

Per-architecture images are first staged under `sha-COMMIT-amd64` and
`sha-COMMIT-arm64`. The version and `latest` manifests are created only after
both architectures pass their container tests.

If a run fails, fix the cause and rerun the failed jobs in the same Actions run.
The draft remains unpublished until the publishing job succeeds. The job can
replace assets in a draft while retrying, and refuses to replace assets in an
already published release. Do not move a published version tag.

If the workflow itself needs a fix, rerunning the old run still uses its old
workflow definition. Merge the fix, then run **Release** from `main`, set
**release-tag** to the existing tag such as `v0.1.0`, and check **publish**. This
builds the original tagged source with the corrected workflow and finishes its
draft release. Publishing without a version tag is rejected. Manually running
Release Please on `main` performs its normal PR and release processing.

## Package locally

For example, on an Apple Silicon Mac:

```sh
sh scripts/install-duckdb.sh
cargo build --locked --release --target aarch64-apple-darwin
sh scripts/package-release.sh 0.1.0 aarch64-apple-darwin
```

The script writes to the ignored `target/release-dist/` directory. Its optional
third argument selects another executable to package. Update the DuckDB license
notice in `docs/licenses/duckdb.txt` alongside the installer when changing the
DuckDB version.
