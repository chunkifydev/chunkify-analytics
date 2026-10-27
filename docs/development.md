# Development and testing

Use Rust 1.99.0, your platform's C build tools, and Node.js 24. Install the
prebuilt DuckDB library once with `sh scripts/install-duckdb.sh`. The lockfiles
pin Rust and JavaScript dependencies.

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

Tests start temporary collectors and use the analytics SDK and MCP client.
No separate collector installation is required.

`COLLECTOR_BINARY` selects another executable for the Node integration tests.
The optional database upgrade check requires `COLLECTOR_PREVIOUS_BINARY` and is
skipped when it is absent.

## S3 integration checks

The S3 tests require an existing disposable bucket and an S3-compatible endpoint.
They store test prefixes containing events and private test credentials. Use an
isolated service; remove its data after the run. For example, with LocalStack:

```sh
docker run -d --name chunkify-s3-test -p 127.0.0.1:19000:4566 \
  -e SERVICES=s3 localstack/localstack:4.14.0
docker exec chunkify-s3-test awslocal s3 mb s3://chunkify-analytics-test
COLLECTOR_S3_TEST_ENDPOINT=127.0.0.1:19000 node --test tests/http/s3.test.mjs
docker rm -f chunkify-s3-test
```

Wait for LocalStack to be healthy before creating the bucket. Tests default to
bucket `chunkify-analytics-test`, region `us-east-1`, access and secret keys `test`,
path addressing and HTTP. Override these with `COLLECTOR_S3_TEST_BUCKET`,
`COLLECTOR_S3_TEST_REGION`, `COLLECTOR_S3_TEST_ACCESS_KEY_ID`,
`COLLECTOR_S3_TEST_SECRET_ACCESS_KEY` and `COLLECTOR_S3_TEST_USE_SSL=true`.
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

```sh
npm run test:collector:docker
```

This builds `chunkify-analytics:test` and exercises the container lifecycle,
credential management, persistence, and benchmark runner. The image runs as UID
10001 and supports a read-only root filesystem with writable `/data` and `/tmp`.

The image includes the shared DuckDB library and requires no host installation.
The workflow runs one job per pull request or push to `main`, and cancels older
runs for the same PR or branch. It builds the collector and Rust test executables
together with `cargo test --profile ci --no-run`. The tests then reuse those
artifacts. Native API tests and container tests exercise the same
executable. Playwright is installed once for real-browser SDK coverage.

The `ci` profile inherits development checks and uses optimization level 1 to
keep builds fast and the load smoke tests practical. Normal Docker builds use
the fully optimized release profile.

CI builds on Ubuntu 22.04 so the executable's glibc requirement is compatible
with the Debian Bookworm runtime image. A Docker named build context replaces
the compiler stage with that tested executable and its DuckDB library. The
regular Docker build still compiles the collector from source and retains local
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
COLLECTOR_LOAD_REPORT=target/benchmarks/run.json npm run benchmark:collector
```

Useful controls include `COLLECTOR_LOAD_CASES`, `COLLECTOR_LOAD_SECONDS`,
`COLLECTOR_LOAD_CPUS`, `COLLECTOR_LOAD_MEMORY_MIB`, and
`COLLECTOR_LOAD_CONCURRENCY`. Use `--list` to inspect workloads and limits before running.

Benchmarks require Linux cgroup v2 and pressure counters. Some Docker VMs lack
these counters and cannot run the benchmark. Measure production capacity with
representative traffic, reads and retention.

Keep reusable tests and workload generators in Git. Keep generated reports,
progress logs, local data, credentials, and build output out of commits.

## Documentation

The root README is the installation and usage guide. Detailed user references
live in `docs/`; example-specific instructions live beside the examples. Update
commands and configuration examples when changing the CLI or runtime settings.
Follow the commit conventions in [AGENTS.md](../AGENTS.md).
