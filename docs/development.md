# Development and testing

Use Rust 1.99.0, a C++17 compiler, and Node.js 24. The collector embeds DuckDB;
its first build can take substantially longer than later builds. The lockfiles
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

The pinned `libduckdb-sys` 1.20000.0 bundles DuckDB's exact v2.0.0 release commit,
`d54cbbe3df17e8e5ce68312f483b4155836112aa`, but its generated version file calls
it `v2.0.0-dev88614`. `.cargo/config.toml` sets the release version macros so
extension lookup and validation use the matching official signed release.
Keep those defines when overriding `CXXFLAGS`. Recheck this workaround when
updating the dependency. Parquet is bundled into the binary; S3 startup downloads
`httpfs` and, for default credentials, `aws` into `data_dir/extensions`.
Signature and version checks remain enabled.

## Docker checks

```sh
npm run test:collector:docker
```

This builds `chunkify-analytics:test` and exercises the container lifecycle,
credential management, persistence, and benchmark runner. The image runs as UID
10001 and supports a read-only root filesystem with writable `/data` and `/tmp`.


Docker caches dependencies and build output. An empty cache recompiles DuckDB.
Linux uses `build.rs` to register statically linked extensions.

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
