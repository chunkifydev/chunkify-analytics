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

The HTTP tests start real collector processes with temporary data directories
and use the published analytics SDK. The MCP tests use the official Rust client
against both the embedded HTTP endpoint and the stdio adapter. No external
collector installation is required.

`COLLECTOR_BINARY` selects another executable for the Node integration tests.
Two optional upgrade checks require `COLLECTOR_PREVIOUS_BINARY`; they are
skipped when it is absent.

## Docker checks

```sh
npm run test:collector:docker
```

This builds `chunkify-analytics:test` and exercises the container lifecycle,
credential management, persistence, and benchmark runner. The image runs as UID
10001 and supports a read-only root filesystem with writable `/data` and `/tmp`.
CI builds a local test image; the workflow does not publish one.

The Docker build caches Cargo dependencies and compiled output. Changing source
rebuilds the application; an empty or invalidated dependency cache can require
recompiling DuckDB. Linux builds use the linker setting in `build.rs` to register
the statically linked DuckDB extensions.

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
`COLLECTOR_LOAD_CONCURRENCY`. The runner validates their ranges and records the
selected workload and resource limits in its output. Use `--list` to inspect
those limits before running.

The runner requires Docker and Linux resource counters, including pressure
metrics. Some local Docker VMs do not expose all required counters. A resource
accounting failure is not a performance result; use a supported environment.
Results from short synthetic runs do not establish sustained production
capacity. Measure with representative data, concurrent reads, and retention.

Keep reusable tests and workload generators in Git. Keep generated reports,
progress logs, local data, credentials, and build output out of commits.

## Documentation

The root README is the installation and usage guide. Detailed user references
live in `docs/`; example-specific instructions live beside the examples. Update
commands and configuration examples when changing the CLI or runtime settings.
Follow the commit conventions in [AGENTS.md](../AGENTS.md).
