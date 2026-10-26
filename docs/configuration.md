# Configuration reference

Use [collector.toml.example](../collector.toml.example) as a starting point for
`collector.toml`. The executable loads that filename from its working directory
when present, or selects a different file with `--config <path>`. Relative
`data_dir` paths in TOML are resolved beside that configuration file. Explicit
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
Project settings and generated credential state remain in the private
`keys.json` registry, separate from installation configuration.

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

Use the standard direct-ingestion mode for normal installations. Keep experimental
execution modes disabled unless you need to evaluate them.
[collector.toml.example](../collector.toml.example) selects one DuckDB thread and
keeps experimental modes disabled. The tables below show built-in defaults;
without the example's thread override, DuckDB chooses its thread count automatically.
Administrators can change these values before starting the process. Recreate
Docker containers with the same data volume to apply configuration changes.

### Installation and data policy

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_DATA_DIR` | `./data` | Directory for keys, database, and write-ahead log, relative to the working directory. |
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
| `COLLECTOR_BATCH_MAX_EVENTS` | `100` | Maximum records per write transaction, including duplicates. Range 1..10000. A whole request must fit. |
| `COLLECTOR_BATCH_MAX_BYTES` | `1048576` | Maximum prepared request-buffer bytes per transaction. Range 1..67108864. See write batching below. |
| `COLLECTOR_BATCH_DELAY_MS` | `10` | Maximum time the worker spends collecting a batch. Range 0..1000 ms. `0` gives each request its own transaction. |
| `COLLECTOR_DUCKDB_THREADS` | Automatic | Optional DuckDB execution thread count, from 1..256. Applies to the shared database used by writes and reads. HTTP and replay threads are separate. |
| `COLLECTOR_CHECKPOINT_THRESHOLD_MIB` | `16` | DuckDB's automatic checkpoint threshold, in MiB. Integer from 1..1024. Larger values allow a larger write-ahead log between checkpoints; see below. |

### Diagnostics

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_INSERT_MODE` | `bulk` | `bulk` uses an appender and one event INSERT per transaction. `row` uses cached INSERT statements per event for diagnostic comparisons. Both share the same batching and durability rules. |
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

DuckDB can perform an automatic checkpoint during a transaction commit, delaying
that transaction and subsequent queued operations. The checkpoint threshold
defaults to 16 MiB, matching the bundled database default. Changing it preserves
durable commits; it changes when WAL contents are consolidated into the database
file. A larger threshold can require more WAL disk space and more recovery work
after a crash, and does not guarantee shorter pauses. Measure with representative
traffic before changing it. See DuckDB's
[checkpoint configuration](https://duckdb.org/docs/stable/configuration/overview).

### Write batching

The worker groups concurrent requests into a bounded transaction. It flushes when
the collection deadline, event limit, or prepared-buffer byte limit is reached,
or when the next queued operation is a read or maintenance task. A request that
does not fit the remaining batch waits for the next transaction. An OTLP request
is never split. A request larger than either batch limit returns 413 without
storing any of its records. Raising the OTLP record limit may also require raising
the batch limits.

The collection timer starts when the worker begins that batch. It excludes time
already spent in the queue and time executing or committing the transaction, so
it is not an HTTP latency guarantee. Sparse traffic normally waits up to 10 ms
before its write starts. Reads interrupt collection in queue order; they still
wait for the preceding writes to commit. Under overload, a full job queue or
prepared-buffer byte budget returns 503. Rate limiting still applies first.

The byte budget counts the allocated strings and vectors retained for prepared
events, flat context and OTLP envelopes. It does not bound total process memory.
Concurrent HTTP bodies, decoding/preparation before admission, allocator overhead,
DuckDB buffers, indexes and temporary copies use additional memory. Keep the body
size limit and container memory limit appropriate for the deployment.

Bulk insertion uses a connection-local temporary staging table. The persistent
schema is version 5. The first occurrence of `(project_id, event_id)` keeps its original
payload, context and optional OTLP envelope. Duplicates within or across requests
do not replace it. Every request receives its own result only after the shared
transaction commits. A failed transaction fails every request in that batch and
rolls back its writes. Graceful shutdown drains admitted requests.
The SDK continues to send once without retrying. Limits are configurable safeguards,
not a throughput guarantee. Configure connection and request timeouts at
your ingress before exposing this development service publicly.


## Experimental execution modes

### Durable ingestion queue

This opt-in prototype evaluates whether durable receipt can remain responsive
while DuckDB commits pause. Enable it with
`COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE=true`. Direct ingestion remains the default.

In queue mode, native ingestion returns HTTP 202 with
`{"accepted":true,"queued":true}` after a SQLite WAL transaction commits with
`synchronous=FULL`. The response does not classify duplicates. OTLP retains its
HTTP 200 export-success response after durable queue acceptance. Reads see events
only after background replay into DuckDB. They can temporarily lag acknowledgements.

The queue uses embedded SQLite in `ingestion.sqlite`, alongside DuckDB in the same
data directory and server process. Keep its WAL and other SQLite files with the
data directory when backing up. A single producer groups disk commits for up to
one millisecond; a separate consumer feeds bounded batches into the existing
DuckDB writer. The queue lock is released while DuckDB processes a batch. Entries
are deleted only after DuckDB commits. Replay after a crash may deliver an event
again; DuckDB's event ID uniqueness preserves the first payload and context.
Set `COLLECTOR_EXPERIMENTAL_REPLAY_PIPELINE=true` to let the consumer prepare
one following batch while DuckDB writes the current batch. This requires durable
queue mode and is off by default; sequential replay remains the default.
Both batches reserve space in `COLLECTOR_QUEUE_MAX_BYTES`; a budget that fits
only one batch uses sequential replay. A failed write leaves its queue entries
and the prepared successor available for recovery.

Use `COLLECTOR_DUCKDB_THREADS` to control database parallelism independently of
HTTP handling and replay. On a CPU-limited server, compare one or two database
threads with the automatic setting before enabling the pipeline. Fewer database
threads can slow dashboard queries. Check response latency, admission rejections,
backlog and recovery on your workload; these options do not guarantee a request
rate. Unsetting the thread option preserves DuckDB's automatic selection.

`COLLECTOR_SPOOL_MAX_BYTES` bounds pending serialized payloads, default 64 MiB,
range 4 KiB through 256 MiB. This is a logical payload limit; SQLite pages, WAL,
and prepared rows consume additional disk and memory. SQLite database allocation
is limited to approximately twice this budget plus 8 MiB, and WAL automatic
checkpointing remains enabled. Queue pages are reused after draining. A full queue
returns HTTP 503. The existing request and prepared-write limits still apply.
A storage-worker failure stops admission, makes health fail, and retains pending
records for recovery. There is no automatic retry of a failed storage worker in
this prototype; restart after resolving the underlying problem.

`GET /v1/ingestion` requires the read token. It reports the ingestion mode and,
when enabled, pending request/event/byte counts, age of the oldest request, and
worker failure. Startup drains recovered events before serving requests or
running retention/viewer deletion. Graceful shutdown drains the queue. Disabling
the mode while pending events exist fails startup rather than skipping them.
Retain the same or larger prepared-write limits when recovering a queue.

The prototype changes acknowledgement visibility and adds a storage component.
Evaluate processing backlog and drain time as well as HTTP latency before using it.

### Separate database reader

Set `COLLECTOR_EXPERIMENTAL_READ_WORKER=true` to execute `/v1/events`,
`/v1/summary`, and `/v1/videos` on one dedicated thread with its own connection
to the same embedded DuckDB instance. This works with direct ingestion or the
durable queue. It is off by default.

The reader allows one executing query and up to 64 waiting queries. Further
read requests receive HTTP 503. Canceled calls waiting in the queue are skipped;
an already executing query finishes normally. Queries still share CPU, memory,
disk and DuckDB checkpoints with ingestion. One slow read can delay other reads.

Each response uses one database transaction so its watermark and results see a
consistent snapshot. Ingestion and retention may commit during the read. A
subsequent request sees newer commits, subject to its pagination watermark;
retention can still remove events between pages. Queue acknowledgements retain
their documented delay before query visibility. Retention, deletion and health
checks stay on the writer. Shutdown finishes reads and drains queued writes
before the writer's final checkpoint.

A separate read does not force a waiting write batch to flush. It returns the
data committed when its snapshot begins. In direct mode, await the ingestion
response before reading if the result must include that event.

`COLLECTOR_PROFILE=true` emits separate reports with `worker: "reader"` and
`worker: "writer"`. Each includes its own queue wait and execution timings.
