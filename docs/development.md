# Development and testing

Use Rust 1.99.0, your platform's C build tools, and Node.js 24. Install the
prebuilt DuckDB library once with `sh scripts/install-duckdb.sh`. The lockfiles
pin Rust and JavaScript dependencies. For a local server, also run
`sh scripts/install-geolocation.sh` to install the bundled City MMDB in
`target/share/chunkify-analytics`. Automated ingestion tests use small synthetic
MMDB fixtures.

## Run the checks

From the repository root:

```sh
npm ci
npx playwright install --with-deps --only-shell chromium
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
npm run test:collector
node --test examples/dashboard/*.test.* examples/demo/*.test.mjs
```

Tests start temporary analytics servers and use the analytics SDK and MCP client.
No separate analytics server installation is required.

`CHUNKIFY_BINARY` selects another executable for the Node integration tests.

## S3 integration checks

The S3 tests require an existing disposable bucket and an S3-compatible endpoint.
They store test prefixes containing events and test credential hashes. Use an
isolated service; remove its data after the run. For example, with LocalStack:

```sh
docker run -d --name chunkify-s3-test -p 127.0.0.1:19000:4566 \
  -e SERVICES=s3 localstack/localstack:4.14.0
docker exec chunkify-s3-test awslocal s3 mb s3://chunkify-analytics-test
CHUNKIFY_S3_TEST_ENDPOINT=127.0.0.1:19000 node --test tests/http/s3.test.mjs
docker rm -f chunkify-s3-test
```

Wait for LocalStack to be healthy before creating the bucket. Tests default to
bucket `chunkify-analytics-test`, region `us-east-1`, access and secret keys `test`,
path addressing and HTTP. Override these with `CHUNKIFY_S3_TEST_BUCKET`,
`CHUNKIFY_S3_TEST_REGION`, `CHUNKIFY_S3_TEST_ACCESS_KEY_ID`,
`CHUNKIFY_S3_TEST_SECRET_ACCESS_KEY` and `CHUNKIFY_S3_TEST_USE_SSL=true`.
Without an endpoint, remote tests skip; the custom local-path test still runs.


## DuckDB build and extensions

Local builds, CI, and Docker link the official DuckDB 2.0.0 shared library.
The setup script supports Linux amd64/arm64 and macOS Intel/Apple Silicon,
verifies the release archive's pinned SHA-256 checksum, and installs it under
`target/lib`. Cargo reads that path from `.cargo/config.toml`. Executables look
for DuckDB in `../lib`, relative to their own location, so they can run directly
without a loader environment variable. Installed executables and demo bundles
must include the library in that layout.

Keep the installer version and checksums in sync with the `duckdb` dependency
when upgrading it. To build DuckDB from source instead:

```sh
cargo build --locked --features bundled-duckdb
```

That optional feature needs a C++17 compiler and statically includes JSON and
Parquet. The version defines in `.cargo/config.toml` correct the bundled
source's `v2.0.0-dev88614` label to `v2.0.0` for signed extension validation.
Keep those defines when overriding `CXXFLAGS`. The prebuilt library already
reports the release version. S3 startup downloads `httpfs` and, for default
credentials, `aws` into `data_dir/extensions`. Signature and version checks
remain enabled.

## Docker checks

### Build the Docker image

To build an image from source, you need Git and Docker with BuildKit support:

```sh
git clone https://github.com/chunkifydev/chunkify-analytics.git
cd chunkify-analytics
docker build -t chunkify-analytics:local .
```

The build downloads the pinned DuckDB library and DB-IP City Lite MMDB, then
compiles the analytics server.
On a small build machine, add `--build-arg BUILD_JOBS=1` to reduce simultaneous
compilation work. Follow the [Docker installation guide](../README.md#install-with-docker)
with `chunkify-analytics:local` in place of
`ghcr.io/chunkifydev/chunkify-analytics:latest`, and copy `config.toml.example`
from the checkout instead of downloading it.

### Test the image

```sh
npm run test:collector:docker
```

This builds `chunkify-analytics:test` and exercises the container lifecycle,
credential management, persistence, and benchmark runner. The image runs as UID
10001 and supports a read-only root filesystem with writable `/data` and `/tmp`.

The image includes the shared DuckDB library and requires no host installation.
The workflow runs one job per pull request or push to `main`, and cancels older
runs for the same PR or branch. It builds the analytics server and Rust test executables
together with `cargo test --profile ci --no-run`. The tests then reuse those
artifacts. Native API tests and container tests exercise the same
executable. Playwright is installed once for real-browser SDK coverage.

The `ci` profile inherits development checks. Rust dependencies use optimization
level 1; the analytics server and test harnesses use level 0 for fast compilation.
DuckDB uses its optimized upstream library. Normal Docker builds use the fully
optimized release profile. The runner already includes Chromium's shared
libraries, so browser setup downloads only the headless browser, without
reinstalling system packages and optional fonts.

CI builds on Ubuntu 22.04 so the executable's glibc requirement is compatible
with the Debian Bookworm runtime image. A Docker named build context replaces
the compiler stage with that tested executable and its DuckDB library. The
regular Docker build still compiles the analytics server from source and retains local
Cargo cache mounts between source edits. Docker's GitHub Actions cache
stores runtime layers directly, without copying Cargo cache mounts into extra
images.

## Run a benchmark

List the available workloads without starting a load test:

```sh
node tests/http/docker/load.mjs --list
```

Write generated reports under the ignored `target/` directory:

```sh
mkdir -p target/benchmarks
CHUNKIFY_LOAD_REPORT=target/benchmarks/run.json npm run benchmark:collector
```

Useful controls include `CHUNKIFY_LOAD_CASES`, `CHUNKIFY_LOAD_SECONDS`,
`CHUNKIFY_LOAD_CPUS`, `CHUNKIFY_LOAD_MEMORY_MIB`, and
`CHUNKIFY_LOAD_CONCURRENCY`. Use `--list` to inspect workloads and limits before running.

Benchmarks require Linux cgroup v2 and pressure counters. Some Docker VMs lack
these counters and cannot run the benchmark. Measure production capacity with
representative traffic, reads and retention.

To measure CSV downloads separately, run the ignored local export benchmark:

```sh
cargo test --locked --test export_benchmark -- --ignored --nocapture \
  > target/export-benchmark.log 2>&1
```

It seeds one million synthetic playback attempts, streams raw events, views and
video metrics, and checks ingestion latency during the raw-event download. It
reports time to first data, total time, download size and peak analytics server memory.
Set `CHUNKIFY_EXPORT_BENCHMARK_ROWS` to change the dataset size. The fixture uses
local storage and removes its temporary database after the run. It does not
measure S3 transfer speed or production capacity.

Keep reusable tests and workload generators in Git. Keep generated reports,
progress logs, local data, credentials, and build output out of commits.

## Documentation

The root README is the installation and usage guide. Detailed user references
live in `docs/`; example-specific instructions live beside the examples. Update
commands and configuration examples when changing the CLI or runtime settings.
Follow the commit conventions in [AGENTS.md](../AGENTS.md).

## Releases

See [the release guide](releasing.md) for automatic release PRs, version tags,
release packages, Docker images, and a manual build that checks the release
without publishing.

## Concurrent report benchmark

With a disposable S3 bucket reachable from Docker and an ARM64 image built from
this checkout, run:

```sh
CHUNKIFY_IMAGE=chunkify-analytics:tenant-reads CHUNKIFY_S3_TEST_ENDPOINT=host.docker.internal:4566 \
  node tests/http/docker/read-load.mjs
```

This seeds ten projects with 100 SDK playback attempts each, then runs 100 users
requesting seven dashboard reports and one MCP report concurrently. During the
70-second timed workload, SDK generators continuously offer 100 events per second
across those projects. S3 buffering is disabled, so a successful write waits for
publication. The workload crosses the 60-second report-cache refresh and checks
that acknowledged events older than the freshness bound appear in reports.

The ARM64 container defaults to one CPU and 2 GiB. Set
`CHUNKIFY_READ_LOAD_CPUS` and `CHUNKIFY_READ_LOAD_MEMORY_MIB` to compare
resource sizes. The write client uses up to 128 connections and bounds outstanding
requests at 256; missed schedules count as failures. Dashboard bounds advance
with the clock, so each new minute also exercises a cold cohort. The JSON report under
`target/benchmarks/` includes cold and cached read latency, HTTP/MCP failures,
actual query counts, offered and completed writes, write failures and latency,
and cgroup memory. Set `CHUNKIFY_READ_LOAD_VIEWS` to change attempts per project,
`CHUNKIFY_READ_LOAD_SECONDS` for duration, and `CHUNKIFY_READ_LOAD_DISTINCT=1`
to add a burst of 100 distinct uncached queries. Compare concurrency using
`CHUNKIFY_READ_LOAD_CONCURRENT` and `CHUNKIFY_READ_LOAD_PER_PROJECT`.
These results describe this synthetic dataset and local S3 endpoint, not a
production capacity guarantee. Test prefixes remain in the disposable bucket.
