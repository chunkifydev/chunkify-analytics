# Development and testing

Use Rust 1.99.0, your platform's C build tools, Node.js 24 and a ClickHouse
server. The lockfiles pin Rust and JavaScript dependencies. For a local server,
also run `sh scripts/install-geolocation.sh` to install the bundled City MMDB in
`target/share/chunkify-analytics`. Automated ingestion tests use small synthetic
MMDB fixtures.

## ClickHouse for development

Tests and local servers need a disposable ClickHouse server. Install and start
the pinned version on `127.0.0.1:8123`:

```sh
sh scripts/install-clickhouse.sh
sh scripts/start-clickhouse.sh
```

The scripts support Linux amd64/arm64 and macOS Intel/Apple Silicon, verify the
download's SHA-256 checksum, and keep the binary and data under the ignored
`target/clickhouse` directory. Stop it with `pkill -f "clickhouse server"`.
With Docker, this also works:

```sh
docker run -d --name chunkify-clickhouse -p 127.0.0.1:8123:8123 \
  -e CLICKHOUSE_SKIP_USER_SETUP=1 --ulimit nofile=262144:262144 \
  clickhouse/clickhouse-server:25.8
```

Tests create one database per test server, named `chunkify_*`, and use
`http://127.0.0.1:8123` unless `CHUNKIFY_TEST_CLICKHOUSE_URL` selects another
server. Use a server without other data.

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
`tests/http/clickhouse.test.mjs` runs two servers against one database.

`CHUNKIFY_BINARY` selects another executable for the Node integration tests.

## Docker checks

### Build the Docker image

To build an image from source, you need Git and Docker with BuildKit support:

```sh
git clone https://github.com/chunkifydev/chunkify-analytics.git
cd chunkify-analytics
docker build -t chunkify-analytics:local .
```

The build downloads the pinned DB-IP City Lite MMDB, then compiles the
analytics server. On a small build machine, add `--build-arg BUILD_JOBS=1` to
reduce simultaneous compilation work. To run it, follow the
[Docker installation guide](../README.md#install-with-docker) with
`CHUNKIFY_IMAGE=chunkify-analytics:local` in `.env`, or use the Compose stack
in [examples/clickhouse](../examples/clickhouse/README.md) from the checkout.

### Test the image

```sh
npm run test:collector:docker
```

This builds `chunkify-analytics:test` and exercises the container lifecycle,
credential management, container replacement, and benchmark runner. The tests
start one ClickHouse container per test process on a private Docker network.
The image runs as UID 10001 and supports a read-only root filesystem with a
read-only `/data` holding `config.toml`, and a writable `/tmp`.

The workflow runs one job per pull request or push to `main`, and cancels older
runs for the same PR or branch. A ClickHouse service container serves every test
step. It builds the analytics server and Rust test executables together with
`cargo test --profile ci --no-run`. The tests then reuse those artifacts. Native
API tests and container tests exercise the same executable. Playwright is
installed once for real-browser SDK coverage.

The `ci` profile inherits development checks. Rust dependencies use optimization
level 1; the analytics server and test harnesses use level 0 for fast compilation.
Normal Docker builds use the fully optimized release profile. The runner already
includes Chromium's shared libraries, so browser setup downloads only the
headless browser, without reinstalling system packages and optional fonts.

CI builds on Ubuntu 22.04 so the executable's glibc requirement is compatible
with the Debian Bookworm runtime image. A Docker named build context replaces
the compiler stage with that tested executable. The regular Docker build still
compiles the analytics server from source and retains local Cargo cache mounts
between source edits. Docker's GitHub Actions cache stores runtime layers
directly, without copying Cargo cache mounts into extra images.

## Benchmarks

Keep reusable tests and workload generators in Git. Write generated reports
under the ignored `target/` directory, and keep progress logs, local data,
credentials, and build output out of commits.

### Ingestion and read capacity

`tests/http/benchmark/capacity.mjs` measures one or more native analytics
servers sharing a ClickHouse database, using SDK-generated playback events
across ten projects:

```sh
cargo build --locked --release
node tests/http/benchmark/capacity.mjs
```

It runs three timed phases and writes `target/benchmarks/capacity.json`:

1. **Ingestion**: concurrent clients send events as fast as servers accept them.
2. **Reads with continuous ingestion**: concurrent dashboard users request seven
   reports while writers offer a fixed event rate, first without the report
   cache (every report queries ClickHouse) and then with a 60-second cache.
3. **Mixed saturation**: ingestion and reads both run as fast as possible.

Each phase reports offered and completed requests per second, failures by
status, and p50/p95/p99 latency. Controls: `CHUNKIFY_CAPACITY_SERVERS` (number
of analytics servers, default 1), `CHUNKIFY_CAPACITY_SECONDS` (per phase,
default 30), `CHUNKIFY_CAPACITY_WRITERS` and `CHUNKIFY_CAPACITY_READERS`
(concurrent clients, default 64), `CHUNKIFY_CAPACITY_WRITE_RATE` (offered events
per second during the read phases, default 200), `CHUNKIFY_CAPACITY_VIEWS`
(seeded playback attempts per project, default 1000), and
`CHUNKIFY_TEST_CLICKHOUSE_URL`. The load generator, servers and ClickHouse share
the machine unless you point the benchmark at a remote ClickHouse server, so
results describe that machine, not a production capacity guarantee.

### Container workloads

List the available container workloads without starting a load test:

```sh
node tests/http/docker/load.mjs --list
```

Run them with the image built by `npm run build:collector:docker`:

```sh
mkdir -p target/benchmarks
CHUNKIFY_LOAD_REPORT=target/benchmarks/run.json npm run benchmark:collector
```

Useful controls include `CHUNKIFY_LOAD_CASES`, `CHUNKIFY_LOAD_SECONDS`,
`CHUNKIFY_LOAD_CPUS`, `CHUNKIFY_LOAD_MEMORY_MIB`, and
`CHUNKIFY_LOAD_CONCURRENCY`. CPU and memory limits apply to the analytics
container; ClickHouse runs in its own container. Benchmarks require Linux
cgroup v2 and pressure counters. Some Docker VMs lack these counters and cannot
run the benchmark.

### Concurrent reports in a container

```sh
CHUNKIFY_IMAGE=chunkify-analytics:test node tests/http/docker/read-load.mjs
```

This seeds ten projects with 100 SDK playback attempts each, then runs 100 users
requesting seven dashboard reports and one MCP report concurrently. During the
70-second timed workload, SDK generators continuously offer 100 events per second
across those projects, and a successful write waits for ClickHouse to store it.
The workload crosses the 60-second report-cache refresh and checks that
acknowledged events older than the freshness bound appear in reports.

The container defaults to one CPU and 2 GiB. Set `CHUNKIFY_READ_LOAD_CPUS` and
`CHUNKIFY_READ_LOAD_MEMORY_MIB` to compare resource sizes. The JSON report under
`target/benchmarks/` includes cold and cached read latency, HTTP/MCP failures,
offered and completed writes, write failures and latency, and cgroup memory.
Set `CHUNKIFY_READ_LOAD_VIEWS` to change attempts per project,
`CHUNKIFY_READ_LOAD_SECONDS` for duration, and `CHUNKIFY_READ_LOAD_DISTINCT=1`
to add a burst of 100 distinct uncached queries. Compare concurrency using
`CHUNKIFY_READ_LOAD_CONCURRENT` and `CHUNKIFY_READ_LOAD_PER_PROJECT`.

### CSV exports

```sh
cargo test --locked --test export_benchmark -- --ignored --nocapture \
  > target/export-benchmark.log 2>&1
```

It seeds one million synthetic playback attempts directly into a new ClickHouse
database, streams raw events, views and video metrics, and checks ingestion
latency during the raw-event download. It reports time to first data, total
time, download size and peak analytics server memory. Set
`CHUNKIFY_EXPORT_BENCHMARK_ROWS` to change the dataset size, and drop the
`chunkify_export_benchmark_*` database afterward.

## Documentation

The root README is the installation and usage guide. Detailed user references
live in `docs/`; example-specific instructions live beside the examples. Update
commands and configuration examples when changing the CLI or runtime settings.
Follow the commit conventions in [AGENTS.md](../AGENTS.md).

## Releases

See [the release guide](releasing.md) for automatic release PRs, version tags,
release packages, Docker images, and a manual build that checks the release
without publishing.
