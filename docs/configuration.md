# Configuration reference

Use [collector.toml.example](../collector.toml.example) as a starting point for
`collector.toml`. The executable loads that filename from its working directory
when present, or selects a different file with `--config <path>`. Relative
`data_dir` and `[storage].path` paths in TOML are resolved beside that configuration file. Explicit
CLI and environment paths remain relative to the working directory.

```sh
chunkify-analytics --config /etc/chunkify/collector.toml config check
chunkify-analytics --config /etc/chunkify/collector.toml serve
```

Precedence is **CLI flags > process environment > .env > TOML > defaults**.
All runtime settings have TOML equivalents in the example, including optional
MCP companion connection settings. Unknown fields, wrong types and invalid
values fail startup. Missing default files are allowed; explicit file paths
must exist. Help and version do not load configuration. `config check` validates
server settings without creating state or printing credentials.

With Docker, mount the TOML file read-only and pass its container path using
`--config`. The image sets `COLLECTOR_DATA_DIR=/data` and
`COLLECTOR_BIND=0.0.0.0:4174`; these environment values override TOML.
Project settings and generated credentials live in a private `keys.json` registry,
separate from installation configuration. S3 stores it remotely and keeps a local
`s3-keys.json` cache. See [storage backends](storage.md).

The collector and MCP companion load `.env` from the current working directory.
`--env-file <path>` selects a different file instead. A missing default `.env`
is allowed; a missing explicitly selected file or malformed file fails startup.
Parent directories are not searched. The process environment overrides file
values, and explicit CLI flags override both. All existing `COLLECTOR_*` and
`CHUNKIFY_MCP_*` settings work in the file. Values are merged without modifying
the process environment.

```sh
chunkify-analytics --env-file production.env serve
chunkify-analytics --env-file production.env config check
```

Ingestion acknowledges completed storage. Requests waiting in memory do not survive
a crash. Keep the experimental separate reader disabled unless you need to evaluate it.
[collector.toml.example](../collector.toml.example) selects one DuckDB thread and
keeps experimental modes disabled. The tables below show built-in defaults;
without the example's thread override, DuckDB chooses its thread count automatically.
Administrators can change these values before starting the process. Recreate
Docker containers with the same data volume to apply configuration changes.

## Storage

Set these fields in one `[storage]` section. See the [storage guide](storage.md)
for complete local and R2 examples, expiration behavior and ECS deployment limits.

| TOML field | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `backend` | `COLLECTOR_STORAGE_BACKEND` | `local` | `local` or `s3`. |
| `path` | `COLLECTOR_LOCAL_PATH` | `data_dir/analytics.duckdb` | Local database file. Relative TOML paths resolve beside the configuration file; environment paths resolve from the working directory. |
| `url` | `COLLECTOR_S3_URL` | Required for S3 | `s3://bucket/optional-prefix/`, without wildcards, credentials or escapes. |
| `endpoint` | `COLLECTOR_S3_ENDPOINT` | AWS regional endpoint | Hostname and optional port, without a scheme or path. R2 uses `ACCOUNT_ID.r2.cloudflarestorage.com`. |
| `region` | `COLLECTOR_S3_REGION` | `us-east-1` | Signing region. Use `auto` for R2. |
| `access_key_id` | `COLLECTOR_S3_ACCESS_KEY_ID` | AWS credential chain | Provide together with `secret_access_key`, or omit both for the default credential chain. |
| `secret_access_key` | `COLLECTOR_S3_SECRET_ACCESS_KEY` | AWS credential chain | Private S3 credential. |
| `session_token` | `COLLECTOR_S3_SESSION_TOKEN` | Unset | Optional temporary-credential token. |
| `url_style` | `COLLECTOR_S3_URL_STYLE` | `path` | `path` or `vhost` bucket addressing. |
| `use_ssl` | `COLLECTOR_S3_USE_SSL` | `true` | HTTPS for S3 connections; `false` selects HTTP. |

Local storage keeps uniqueness constraints. S3 stores every accepted row without
event deduplication. Run one collector per S3 prefix and stop it before starting
its replacement. S3 mode requires a writable disposable `data_dir` but no durable
local volume.

### Installation and data policy

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_DATA_DIR` | `./data` | Directory for keys, locks and temporary files. Also holds the default local database and WAL. Environment paths are relative to the working directory. |
| `COLLECTOR_BIND` | `127.0.0.1:4174` | Listening IP address and port. Use `0.0.0.0:4174` behind an ingress when needed. |
| `COLLECTOR_ALLOWED_ORIGINS` | Empty | Comma-separated exact website origins, without paths or trailing slashes. Empty denies requests with an Origin header. Server requests without Origin still require the ingest key. |
| `COLLECTOR_RETENTION_SECONDS` | `2592000` | Default retention by server receipt time, in seconds. Default is 30 days. Projects may override it; `0` disables expiration for inheriting projects. Must be a non-negative whole number no larger than `9223372036854775`. |
| `COLLECTOR_RETENTION_INTERVAL_SECONDS` | `3600` | Seconds between completed retention sweeps while serving. Whole number from 1 to 86400. The timer stays active for live project changes; disabled policies delete nothing. |
| `COLLECTOR_STORE_IP` | `true` | Store collector-derived request IP context. Accepts only `true` or `false`. Disabling applies to new events and does not erase existing data or sanitize arbitrary fields in the original payload. |
| `COLLECTOR_TRUSTED_PROXIES` | Empty | Comma-separated literal IPv4/IPv6 addresses or CIDRs allowed to supply `X-Forwarded-For`. Empty trusts no proxies. No hostnames, ports, or wildcards. Use IPv4 CIDRs for IPv4-mapped addresses. |

### Admission limits

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_MAX_BODY_BYTES` | `65536` | Maximum request body size in bytes. OTLP gzip requests must fit this limit both before and after decompression. Must be positive. |
| `COLLECTOR_MAX_OTLP_RECORDS` | `100` | Maximum log records per OTLP request, across all resource and scope groups, including duplicates. Must be positive. |
| `COLLECTOR_INGEST_PER_SECOND` | `100` | Global ingestion token bucket. Capacity and refill rate both use this value. Must be positive. |
| `COLLECTOR_QUEUE_MAX_BYTES` | `16777216` | Maximum bytes held by admitted prepared write buffers, including the active batch. Must be at least the batch byte limit, at most 268435456. |
| `COLLECTOR_QUEUE_MAX_REQUESTS` | `256` | Database channel capacity, shared by writes, reads and maintenance. Range 1..4096. The byte budget also applies to writes. |

### Advanced performance settings

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_BATCH_MAX_EVENTS` | `100` | Maximum records per write batch, including duplicates. Range 1..10000. A whole request must fit. |
| `COLLECTOR_BATCH_MAX_BYTES` | `1048576` | Maximum prepared request-buffer bytes per batch. Range 1..67108864. See write batching below. |
| `COLLECTOR_BATCH_DELAY_MS` | `10` | Maximum time the worker spends collecting a batch. Range 0..1000 ms. `0` gives each request its own batch. |
| `COLLECTOR_DUCKDB_THREADS` | Automatic | Optional DuckDB execution thread count, from 1..256. Applies to the shared database used by writes and reads. HTTP runs separately. |
| `COLLECTOR_CHECKPOINT_THRESHOLD_MIB` | `16` | DuckDB's automatic checkpoint threshold, in MiB. Integer from 1..1024. Local storage only. Larger values allow a larger write-ahead log between checkpoints; see below. |

### Diagnostics

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_INSERT_MODE` | `bulk` | `bulk` uses an appender and one event INSERT per transaction. `row` uses cached INSERT statements per event for diagnostic comparisons. Both share the same local batching and durability rules. S3 always uses an appender and Parquet output. |
| `COLLECTOR_MCP_ENABLED` | `true` | Serve the authenticated `/mcp` endpoint in the collector process. |
| `COLLECTOR_MCP_ALLOWED_HOSTS` | `localhost,127.0.0.1,[::1]` | Allowed HTTP hostnames or host:port authorities for MCP. Add the ingress hostname for remote clients. |
| `COLLECTOR_PROFILE` | `false` | Emit one aggregate database timing report to stderr when the worker drains on graceful shutdown. Accepts only `true` or `false`. No event values or credentials are included. Intended for local performance diagnosis. |

### Limits and overload

Both ingestion endpoints share the rate limit. A batch consumes one request.
The default 100 requests/s is an adjustable admission policy, not a measured
capacity limit. Project allowances also apply; see [project limits](administration.md#project-limits-and-origins).
The service rejects excess ingestion requests with HTTP 429 and `Retry-After: 1`.
The database channel holds at most 256 waiting operations by default; a full channel
returns 503. The worker also holds its active batch and may hold one deferred
operation. All prepared write buffers, including that deferred request, share the
byte budget.

With local storage, DuckDB can perform an automatic checkpoint during a transaction commit, delaying
that transaction and subsequent queued operations. The checkpoint threshold
defaults to 16 MiB, matching the bundled database default. Changing it preserves
durable commits; it changes when WAL contents are consolidated into the database
file. A larger threshold can require more WAL disk space and more recovery work
after a crash, and does not guarantee shorter pauses. Measure with representative
traffic before changing it. See DuckDB's
[checkpoint configuration](https://duckdb.org/docs/stable/configuration/overview).

### Write batching

The worker groups concurrent requests into a bounded batch. It flushes when
the collection deadline, event limit, or prepared-buffer byte limit is reached,
or when the next queued operation is a read or maintenance task. A request that
does not fit the remaining batch waits for the next batch. An OTLP request
is never split. A request larger than either batch limit returns 413 without
storing any of its records. Raising the OTLP record limit may also require raising
the batch limits.

The collection timer starts when the worker begins that batch. It excludes time
already spent waiting and time executing storage operations, so
it is not an HTTP latency guarantee. Sparse traffic normally waits up to 10 ms
before its write starts. Reads interrupt collection in queue order; they still
wait for the preceding writes to finish. Under overload, a full job queue or
prepared-buffer byte budget returns 503. Rate limiting still applies first.

The byte budget counts the allocated strings and vectors retained for prepared
events, flat context and OTLP envelopes. It does not bound total process memory.
Concurrent HTTP bodies, decoding/preparation before admission, allocator overhead,
DuckDB buffers, indexes and temporary copies use additional memory. Keep the body
size limit and container memory limit appropriate for the deployment.

Local bulk insertion uses a connection-local temporary staging table. The persistent
schema is version 5. The first occurrence of `(project_id, event_id)` keeps its original
payload, context and optional OTLP envelope. Duplicates within or across requests
do not replace it. Every request receives its own result only after the shared
transaction commits. A failed transaction fails every request in that batch and
rolls back its writes. S3 acknowledges after all Parquet uploads finish, without
a transaction spanning objects. See [S3 write behavior](storage.md#parquet-layout-and-writes).
Graceful shutdown drains admitted requests.
The SDK continues to send once without retrying. Limits are configurable safeguards,
not a throughput guarantee. Configure connection and request timeouts at
your ingress before exposing this development service publicly.


## Experimental execution modes

### Separate database reader

Set `COLLECTOR_EXPERIMENTAL_READ_WORKER=true` to execute `/v1/events`,
`/v1/summary`, and `/v1/videos` on one dedicated thread with its own connection
to the same embedded DuckDB instance. It is off by default and requires local storage.

The reader allows one executing query and up to 64 waiting queries. Further
read requests receive HTTP 503. Canceled calls waiting in the queue are skipped;
an already executing query finishes normally. Queries still share CPU, memory,
disk and DuckDB checkpoints with ingestion. One slow read can delay other reads.

Each response uses one database transaction so its watermark and results see a
consistent snapshot. Ingestion and retention may commit during the read. A
subsequent request sees newer commits, subject to its pagination watermark;
retention can still remove events between pages. Retention and health
checks stay on the writer. Shutdown finishes reads and drains queued writes
before the writer's final checkpoint.

A separate read does not force a waiting write batch to flush. It returns the
data committed when its snapshot begins. In direct mode, await the ingestion
response before reading if the result must include that event.

`COLLECTOR_PROFILE=true` emits separate reports with `worker: "reader"` and
`worker: "writer"`. Each includes its own queue wait and execution timings.
