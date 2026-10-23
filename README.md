# Chunkify analytics

A Rust service that stores Chunkify playback events in an embedded DuckDB file.
The executable is `chunkify-analytics-collector`; it also provides the local MCP companion.
The JavaScript SDK remains in [chunkify-player](https://github.com/chunkifydev/chunkify-player/tree/main/packages/analytics).
See [repository extraction notes](MIGRATION.md) for history and the test SDK dependency.
The implementation follows [issue #381](https://github.com/chunkifydev/chunkify/issues/381).

## Implementation stages

1. Native HTTP ingestion, durable storage, separate ingest/read keys, event queries,
   and summary metrics. Implemented.
2. OTLP/HTTP JSON ingestion, including batched logs and gzip. Implemented.
3. Retention, viewer deletion, key rotation, and backup/restore procedures. Implemented.
4. Docker packaging, container lifecycle tests, and measured capacity guidance.
   Implemented. See the [initial capacity report](capacity.md) and the subsequent
   [write batching measurements](batching.md) and
   [checkpoint investigation](checkpointing.md). The
   [local retest](local-retest-2026-10-19.md) records results after reducing host
   memory pressure. The [bounded buffering comparison](buffering.md) tests a
   larger request queue at 750 and 1,000 requests/s. The
   [verification ledger report](verification-ledger.md) records the benchmark
   memory reduction and three-minute probes, including a preloaded database.
   The [higher concurrency probe](concurrency.md) records a failed 1,000 requests/s
   run during concurrent host activity. The [comparison after the call](postcall.md)
   records two full-delivery passes at 1,000 requests/s for three minutes with a
   1,024-request generator cap and server queue. The [higher-rate probes](throughput.md)
   test 2,000 and 3,000 requests/s with a larger queue and checkpoint threshold.
   The [read-impact comparison](read-impact.md) finds that ingestion latency still
   fails its target with concurrent analytics reads disabled. Deployment defaults
   are unchanged.
5. Browser/device context through `createChunkifyAdapter`. Implemented in the SDK
   and preserved in raw-event storage. Request IP collection, its opt-out, and
   explicit trusted-proxy configuration are implemented in the collector.
   Browser, OS and device-type filters are available on all read endpoints.

6. Isolated projects, named ingest/read keys, project origin policies and ingestion
   limits, with CLI administration. Implemented. Hosted account membership remains
   a separate follow-up.
7. A local, read-only MCP companion for AI-assisted dashboard development. See
   [MCP setup and verification](mcp.md). MCP is required for the first release;
   the [Codex CLI dashboard walkthrough](mcp-walkthrough.md) passed on 2026-10-20.

The [administrator and Docker walkthrough](administrator-walkthrough.md) passed
on 2026-10-21, including project isolation, credential management, container
replacement, backup/restore and a legacy database upgrade to schema 5. Capacity
validation and final release checks remain pending. The
[recommended configuration](configuration.md) selects direct ingestion with
optional administrator tuning; experimental execution modes remain opt-in.

This is a development version.

## API contract

The [OpenAPI 3.1.1 document](openapi.json) describes ingestion, read endpoints,
authentication, filters, pagination, event/context schemas and error responses.
It also documents the experimental durable queue's acknowledgement and read
visibility behavior and project isolation. Project administration is CLI-only.

`npm run check:collector:openapi` validates the document, schemas and examples.
`npm run test:collector` builds the collector, then checks the contract
against real HTTP responses alongside the integration suite. CI runs both checks.

## Build and run

Install Rust and a C++ compiler. CI uses Rust 1.99.0 and Node.js 24. DuckDB is compiled into the executable, so the
first build takes longer than later builds. No separate database server is needed.
The lockfile records the exact Rust dependencies.

From this directory:

```sh
cargo build --locked
export COLLECTOR_DATA_DIR="$PWD/data"
export COLLECTOR_ALLOWED_ORIGINS="http://127.0.0.1:4173"
export COLLECTOR_DUCKDB_THREADS=1
./target/debug/chunkify-analytics-collector init
./target/debug/chunkify-analytics-collector serve
```

`init` creates `keys.json` in the data directory and refuses to overwrite existing
keys. On Unix, a new data directory has mode `0700` and the key file has mode `0600`.
Read the file locally to configure your application. The public `ingest_key`
permits event submission. Keep `read_token` on your dashboard backend.
Both credentials are random opaque tokens, not JWTs. They remain valid until
rotated. `Authorization: Bearer` describes how the read token is sent.

One process owns `analytics.duckdb` and its write-ahead log. Keep the entire data
directory. Accepted events survive restarts, including an abrupt process stop.
SIGINT and SIGTERM stop accepting new connections, finish active requests, and
close the database. A second writer using the same database file fails at startup.
The collector and offline administration commands hold an exclusive
`collector.lock` file lock for their lifetime. Live key commands use the running
collector's private local connection. Project configuration and other offline
administration still require stopping the collector. The OS releases the lock
after a crash; leave the lock file in place.
Startup upgrades schema versions 1–4 to version 5 without dropping events
or changing existing keys. Existing data and credentials belong to the `default`
project; historical events have `ingest_key_id: null`. Existing events receive no invented IP context. Stop the old
service and take a backup before starting the new executable. An older executable
cannot open schema version 5; restore its pre-upgrade backup to roll back.

### Projects and named keys

A project is the boundary for ownership and access: use separate projects for
customers, or for staging and production. Each has a stable `id` and an editable
`name`. Names live once in `keys.json`; events reference the stable `project_id`.
There is no additional tenant/account layer in this version.

Stop the collector, keep `COLLECTOR_DATA_DIR` set, then run:

```sh
./target/debug/chunkify-analytics-collector project-create live "Production website"
./target/debug/chunkify-analytics-collector project-create staging "Staging website"
./target/debug/chunkify-analytics-collector project-list
./target/debug/chunkify-analytics-collector project-rename live "Customer A — Production"
./target/debug/chunkify-analytics-collector key-create live ingest "Website"
./target/debug/chunkify-analytics-collector key-create live read "Dashboard backend"
# Use the safe key ID from project-list or key-create:
./target/debug/chunkify-analytics-collector key-rotate live KEY_ID
./target/debug/chunkify-analytics-collector key-revoke live KEY_ID
```

`project-create <id> [name]` returns `ingest_key` and `read_token`. The name defaults
to the ID when omitted. `key-create` and `key-rotate` return `key_id` and `secret`.
Save these secrets when the command returns: newly created project credentials
are stored as SHA-256 hashes and cannot be retrieved later. `project-list` returns
project IDs, names, settings and key IDs/names/permissions, without secrets or
hashes.

On Linux and macOS, `project-list`, `key-create`, `key-rotate`, `key-revoke` and
`rotate-key` also work while the collector is running. Keep `COLLECTOR_DATA_DIR`
pointed at the same directory and run the CLI as the collector's operating-system
user. Successful live changes return `restart_required: false` and take effect
before the command returns. Already authenticated requests may finish. Project
creation, renaming, settings changes and viewer deletion still require stopping
the collector. On other operating systems, key administration remains offline.

The CLI connects through a private `admin.sock` in the data directory, with mode
`0600` and a peer-user check. For paths that exceed the Unix socket length limit,
it uses a randomly named private directory under `/tmp`. A private
`admin.socket-id` file in the data directory lets the CLI find it automatically.
`/tmp` must be writable in that case. No public HTTP administration route or admin
token is added. Keep the data directory private and do not edit `keys.json` while the
collector runs. A failed write leaves the active keys unchanged. If the file was
replaced but its directory could not be synced, the command returns a `warning`;
the new keys are active, but their survival after a system crash is uncertain.
If a command loses its connection or times out, check `project-list` before
retrying. A named secret whose response was lost cannot be recovered; rotate or
revoke that key.

For Docker, run the same commands inside the running container:

```sh
docker exec chunkify-analytics chunkify-analytics-collector project-list
docker exec chunkify-analytics chunkify-analytics-collector key-create live ingest "Website v2"
docker exec chunkify-analytics chunkify-analytics-collector key-revoke live OLD_KEY_ID
```

For a transition without interrupting delivery, create a second key, deploy it
to clients, then revoke the old key once those clients have switched. `key-rotate`
replaces the secret immediately and retains its key ID; it has no overlap period.
These commands do not reset project quotas or change stored events.

The initial `default` project's plaintext `ingest_key` and `read_token` exports
remain in the private `keys.json` file for the existing init/rotate-key workflow.
Once a `projects` registry is saved, its hashes are authoritative; a revoked
primary credential is not re-enabled by the legacy export. Rotating an existing
default primary key updates that export too. Back up the entire file.

The browser sends only `X-Chunkify-Ingest-Key`. An in-memory hash lookup resolves
its project and safe key ID; no per-request database lookup is needed. The project
ID is not encoded in the token. Native and OTLP records receive `project_id` and
`ingest_key_id` as trusted response metadata. Client-supplied properties remain in
the original event but cannot redirect storage. Multiple keys for one project
share deduplication and quotas; rotation keeps the key ID and historical events.
An ingest key identifies the destination/integration, not a verified sender or
viewer. `viewer_id` remains an optional client-supplied identifier.

A read token permits access to exactly one project on all read endpoints,
including queue counts. Cursors are bound to that project; a cursor never grants
access. The same event/view/viewer IDs can occur independently in other projects.
There is no cross-project query token or account membership API yet.

#### Project limits and origins

Defaults divide the installation's ingestion rate, in-flight write slots, memory
queue budget and durable payload budget evenly across the configured projects
(including `default`). Rate and slot defaults have a minimum of one. Each project
inherits `COLLECTOR_ALLOWED_ORIGINS`. Keys within a project share these limits.
The installation caps still apply, so these are ceilings, not throughput or latency
guarantees. Read queries, writer scheduling and retained database storage are
shared; retention is installation-wide in this first implementation.

To override one project's limits, save a JSON file and run
`project-configure live /path/to/settings.json`, then restart:

```json
{
  "allowed_origins": ["https://player.example"],
  "ingest_per_second": 100,
  "queue_bytes": 4194304,
  "spool_bytes": 16777216
}
```

The command replaces that project's overrides. Omitted/null values inherit the
default; `allowed_origins: []` disables browser origins for that project. Queue
limits accept 4096–268435456 bytes and are capped by the installation limit.
`spool_bytes` applies only with the experimental durable queue and bounds pending
serialized payloads, not retained DuckDB storage. Restarting with lower limits
still recovers previously acknowledged events before accepting new traffic.

A full queue returns 503; exceeding a project rate returns 429; a single request
that cannot fit its byte budget returns 413. Origin checks on POST use the
resolved project's policy. Unauthenticated preflight uses the union of allowed
origins, since browsers do not send the ingest key value during preflight.

### Database layout

| Table | Contents |
| --- | --- |
| `events` | Typed event/browser columns plus a lossless JSON remainder, versioned context reconstruction, and columns for filtering, deduplication and summaries. `project_id` identifies ownership and `ingest_key_id` records the first accepted source key. `(project_id, event_id)` is unique; `view_id` groups events for a viewing attempt within a project. An internal `ingest_id` orders pagination. |
| `event_otlp` | Original OTLP log/resource/scope JSON, keyed by `(project_id, event_id)`, for events first accepted through OTLP. |
| `collector_schema` | The database schema version. |

All data for an accepted event is written in one transaction. Known scalar event
and browser fields use typed columns. Unknown properties, arrays, objects,
explicit nulls and numeric values that cannot be represented exactly remain in
`event_remainder`. The event retrieval API reconstructs the complete event,
including custom fields and the original timestamp string.

The collector stores browser context once per event. `context_remainder` holds
the differences between the returned `event_context` and the browser context,
including request IP fields. `context_format` versions that reconstruction;
a full-context fallback preserves historical overrides and absence. Browser,
OS and device filters use three typed columns derived from the actual returned
context. Context remains event-specific, so viewport and IP changes are retained.
Original OTLP objects remain in `event_otlp`. Generated ingest and read keys stay
in `keys.json`, outside the database.

The upgrade rebuilds `events` within one transaction, preserving ingestion IDs,
event ordering, receipt timestamps and the sequence. An error rolls back the
migration. A direct `ADD COLUMN` triggered a WAL recovery error in the pinned
DuckDB version during forced-restart testing, so migrations use a table rebuild.
Allow extra disk space for a copy of the event table and its WAL during migration.
Versions 1–2 backfill only browser context already present in the event; no IP is
invented. Version 3 preserves its stored context. The integration suite checks
fresh databases and upgrades after forced restarts.

A smaller representation reduces storage growth. Rebuilding an existing database
can leave reusable blocks in its file, so the file need not shrink immediately.
Fresh-file benchmark savings do not promise the same immediate reduction after
an in-place upgrade. Durable requests from an older collector have their prepared
row sizes recalculated before replay. Very small writer budgets may need to be
increased if an already queued request no longer fits the new representation;
pending records remain in SQLite if replay fails. See [the storage comparison](storage-layout-comparison.md)
for measurements and workload limitations.

## Run with Docker

Docker Engine 25 or later with BuildKit is required. From the repository root,
copy [collector.env.example](collector.env.example) to `.env` and edit the website
origin and installation policies before starting:

```sh
cp collector.env.example .env
```

This example uses the [recommended configuration](configuration.md). Then run:

```sh
docker build -t chunkify-analytics-collector:local .
docker volume create chunkify-analytics-data
docker run --rm \
  --mount type=volume,src=chunkify-analytics-data,dst=/data \
  chunkify-analytics-collector:local init
docker run -d --name chunkify-analytics --restart unless-stopped \
  --mount type=volume,src=chunkify-analytics-data,dst=/data \
  -p 127.0.0.1:4174:4174 \
  --env-file .env \
  chunkify-analytics-collector:local
```

The image runs as UID/GID `10001:10001`. A new named volume inherits the private
`/data` directory from the image, allowing `init` and `serve` to run without root.
Always mount the same volume at `/data` for the server and administrative commands.
Deleting a container keeps its named volume; deleting the volume removes the data.
See Docker's [volume documentation](https://docs.docker.com/engine/storage/volumes/).

The first build compiles bundled DuckDB and needs several minutes, sufficient free
disk space, and memory for a C++ compiler. It defaults to two compilation jobs;
use `--build-arg BUILD_JOBS=1` on a smaller build machine. Rust, Debian base images,
and Cargo dependencies are pinned. Runtime packages use a dated
[Debian snapshot](https://snapshot.debian.org/). Update those pins deliberately for
dependency and security updates. The final image contains no Rust/C++ toolchain.
No prebuilt image is published by this change.

Read the generated credentials locally:

```sh
docker run --rm \
  --mount type=volume,src=chunkify-analytics-data,dst=/data,readonly \
  --entrypoint cat chunkify-analytics-collector:local /data/keys.json
```

Keep this output private. Only `ingest_key` belongs in browser code.
Configuration uses the same environment variables as the executable. The image
defaults to `COLLECTOR_BIND=0.0.0.0:4174` and `COLLECTOR_DATA_DIR=/data`.
The example publishes the port on host loopback for a host-based HTTPS ingress.
An ingress in another container can instead share a Docker network and reach
`chunkify-analytics:4174` without a published host port.

Check health and operational logs with:

```sh
docker inspect --format '{{.State.Health.Status}}' chunkify-analytics
docker logs chunkify-analytics
curl --fail http://127.0.0.1:4174/healthz
```

The image's health check calls `/healthz`, including its database check, at the
configured bind address. `unhealthy` alone does not restart a Docker container.
The collector runs directly as PID 1, receives SIGTERM, drains requests, and
checkpoints its database. Allow at least 20 seconds when stopping it, and longer
if a larger database needs more time to checkpoint:

```sh
docker stop --time 20 chunkify-analytics
```

For an upgrade, stop the old container, take a backup, remove that stopped
container, and run the new image with the same volume and environment settings.
Run `init` only for a new installation. Keep one running writer per volume.

The service supports a read-only root filesystem when `/data` stays writable.
Optional run settings are `--read-only --tmpfs /tmp:rw,nosuid,noexec,size=64m
--cap-drop=ALL --security-opt=no-new-privileges`. For a host bind mount instead of
a named volume, create its directory with owner `10001:10001` and mode `0700`
before mounting it at `/data`.

### Container administration and backups

Rotate a key inside the running container:

```sh
docker exec chunkify-analytics chunkify-analytics-collector rotate-key ingest
```

Use `rotate-key read` in the same position for the read token. Read and distribute
the replacement credential as described below. Named key commands also work
through `docker exec`.

Project changes and viewer deletion require stopping the server first:

```sh
docker stop --time 20 chunkify-analytics
docker run --rm \
  --mount type=volume,src=chunkify-analytics-data,dst=/data \
  chunkify-analytics-collector:local delete-viewer 'viewer-123'
docker start chunkify-analytics
```

To back up the named volume, keep the server stopped until the archive is complete.
Choose a new backup filename and keep its contents private:

```sh
docker stop --time 20 chunkify-analytics
umask 077
docker run --rm \
  --mount type=volume,src=chunkify-analytics-data,dst=/data,readonly \
  --entrypoint tar chunkify-analytics-collector:local -C /data -czf - . \
  > analytics-backup.tar.gz
docker start chunkify-analytics
```

Restore to a new, unused named volume:

```sh
docker volume create chunkify-analytics-restored
docker run --rm -i \
  --mount type=volume,src=chunkify-analytics-restored,dst=/data \
  --entrypoint tar chunkify-analytics-collector:local -C /data -xzpf - \
  < analytics-backup.tar.gz
```

Start the collector using that restored volume without running `init`. Follow the
same version, retention, deletion, and credential rules in the backup section below.

## Send analytics

Use the analytics package from this branch:

```js
import { monitor } from "@chunkify/analytics";
import { createChunkifyAdapter } from "@chunkify/analytics/chunkify";

monitor(document.querySelector("video"), {
  videoId: "video-123",
  destinations: [createChunkifyAdapter("http://127.0.0.1:4174/v1/events", {
    ingestKey: "COPY_YOUR_PUBLIC_INGEST_KEY",
  })],
});
```

Send the complete SDK event. Custom mappings that remove required fields cannot
be accepted. The collector stores the complete JSON object, including additional
fields, and deduplicates by `(project_id, event_id)`. Repeating a valid event does not replace
the first payload or count it twice.

The Chunkify adapter adds `browser_context` to every event by default. Read it at
`events[].event.browser_context` through `GET /v1/events`. Set
`collectBrowserContext: false` to send playback events only. See the SDK's
[field definitions and PostHog comparison](https://github.com/chunkifydev/chunkify-player/blob/main/packages/analytics/README.md#browser-context-fields).
Browser context uses the typed storage described above. Browser/OS/device filters
use the collector-derived event context. The collector adds request IP context
separately below.

Generic `createHttpAdapter(endpoint, { headers })` remains supported when you
manage your own context. HTTP and OTLP adapters do not collect browser context.

For OTLP/HTTP JSON logs, use the same key with the OTLP adapter:

```js
import { createOtlpAdapter } from "@chunkify/analytics/otlp";

const destination = createOtlpAdapter("http://127.0.0.1:4174/v1/logs", {
  headers: { "X-Chunkify-Ingest-Key": "COPY_YOUR_PUBLIC_INGEST_KEY" },
});
```

Pass this destination to `monitor` in the same way as the HTTP adapter. The SDK
sends one log per request. A gateway can batch logs and gzip its requests.

The ingest key is visible to browsers. CORS limits which websites browsers may
submit from; it does not prove that a request came from a trusted viewer.
Serve public traffic through your HTTPS ingress. Internal HTTP is supported.

## Configuration

Start with the [recommended mode, settings and tuning policy](configuration.md).
[collector.env.example](collector.env.example) selects one DuckDB thread and
keeps experimental modes disabled. The tables below show built-in defaults;
without the example's thread override, DuckDB chooses its thread count automatically.
Administrators can change these values before starting the process. Docker
containers must be recreated to pick up edits to an environment file.

### Installation and data policy

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_DATA_DIR` | `./data` | Directory for keys, database, and write-ahead log, relative to the working directory. |
| `COLLECTOR_BIND` | `127.0.0.1:4174` | Listening IP address and port. Use `0.0.0.0:4174` behind an ingress when needed. |
| `COLLECTOR_ALLOWED_ORIGINS` | Empty | Comma-separated exact website origins, without paths or trailing slashes. Empty denies requests with an Origin header. Server requests without Origin still require the ingest key. |
| `COLLECTOR_RETENTION_SECONDS` | `2592000` | Retention by server receipt time, in seconds. Default is 30 days. `0` disables expiration. Must be a non-negative whole number no larger than `9223372036854775`. |
| `COLLECTOR_RETENTION_INTERVAL_SECONDS` | `3600` | Seconds between completed retention sweeps while serving. Whole number from 1 to 86400. Ignored when retention is disabled. |
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
| `COLLECTOR_PROFILE` | `false` | Emit one aggregate database timing report to stderr when the worker drains on graceful shutdown. Accepts only `true` or `false`. No event values or credentials are included. Intended for local performance diagnosis. |

### Limits and overload

Both ingestion endpoints share the rate limit. A batch consumes one request.
The default 100 requests/s is an adjustable admission policy, not a measured
capacity limit. Project allowances also apply; see [project limits](#project-limits-and-origins).
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

## HTTP API

| Method and path | Authorization | Response |
| --- | --- | --- |
| `GET /healthz` | None | `{"status":"ok"}` after a database check, or 503 if unavailable. |
| `POST /v1/events` | `X-Chunkify-Ingest-Key` | 201 with `{"accepted":true,"duplicate":false}` after commit; 200 with `{"accepted":false,"duplicate":true}` for an existing event. |
| `POST /v1/logs` | `X-Chunkify-Ingest-Key` | OTLP JSON logs. 200 with `{}` after the whole batch commits, including empty or duplicate-only batches. |
| `GET /v1/events` | `Authorization: Bearer READ_TOKEN` | `{"events":[{"project_id":"default","ingest_key_id":"primary-ingest","received_at":"...","event":{...}}],"next_cursor":null}` |
| `GET /v1/summary` | `Authorization: Bearer READ_TOKEN` | `{"views":3,"unique_viewers":1,"total_playing_time_ms":6500}` |
| `GET /v1/videos` | `Authorization: Bearer READ_TOKEN` | `{"videos":[{"video_id":"video-123","views":3,"unique_viewers":1,"total_playing_time_ms":6500}],"next_cursor":null}` |

Native ingestion accepts `Content-Type: application/json` and one complete schema
version 1 event. It validates all seven event shapes emitted by the SDK. Malformed
or invalid payloads return 400; unsupported media types return 415; oversized bodies
return 413. Missing or invalid keys return 401. Disallowed origins return 403.

Only the ingest endpoints enable CORS, including preflight for the ingest header.
Read endpoints are intended for the customer's backend and do not enable CORS.
Keys are never accepted in a query string. Responses are marked `no-store`.

### OTLP format and storage

`POST /v1/logs` accepts `application/json`, with no content encoding or with
`Content-Encoding: gzip`. It supports multiple `resourceLogs`, `scopeLogs`, and
`logRecords` using the lower camel case field names in the
[OTLP JSON specification](https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding).
Unknown OTLP message fields are ignored during normalization and preserved in
the stored log and context. This endpoint accepts Chunkify analytics logs only.
Binary Protobuf, gRPC, metrics, traces, and arbitrary log events are outside its scope.

The canonical event comes from each log's `attributes`. The decoder supports the
SDK's string, boolean, integer, finite number, array, and key-value list values.
Integer values may be decimal strings or JSON integers. Unsupported value types,
duplicate attribute keys, and invalid Chunkify fields reject the export. Custom
`mapAttributes` functions must retain all required schema fields. The collector
restores known nullable fields omitted by the SDK, including nullable rendition
fields. It cannot reconstruct additional null fields that a custom mapper omitted.

Every log is validated before storage. An invalid record rejects the entire
batch with HTTP 400. Wire size, decompressed size, or record count violations return
413. Unsupported content types or encodings return 415. OTLP errors use the JSON
Status shape `{"message":"..."}`, including authentication and rate-limit errors.
No partial export is accepted, and success is sent only after the transaction commits.

The raw-event API returns the canonical `event` and, for an event first received
through OTLP, an additional `otlp` object:

```json
{
  "log_record": { "attributes": [], "eventName": "..." },
  "resource_logs": { "resource": {}, "schemaUrl": "..." },
  "scope_logs": { "scope": {}, "schemaUrl": "..." }
}
```

`log_record` retains the original log object. `resource_logs` and `scope_logs`
retain their original group fields, excluding the nested record/group arrays.
Objects preserve JSON values, not original whitespace or key order. Native events
have no `otlp` field. Deduplication is shared across formats: the first accepted
payload and context win, and later duplicates do not enrich or replace them.
Both formats feed the same filters, pagination, and summary calculations.

### Flat event context and request IP

`GET /v1/events` returns a flat `event_context` alongside the original `event` and
optional `otlp`. For example:

```json
{
  "schema_version": 1,
  "browser_name": "Firefox",
  "os_name": "Linux",
  "device_type": "desktop",
  "language": "fr-FR",
  "peer_ip": "10.20.1.2",
  "client_ip": "198.51.100.8",
  "client_ip_source": "x_forwarded_for"
}
```

The collector copies the supported scalar or null fields from schema version 1
`event.browser_context` into this object. Unsupported fields and nested values
remain only in the original payload. Browser values are client-reported, not
verified identity. Only the collector supplies `peer_ip`, `client_ip`, and
`client_ip_source`. When IP collection is disabled, these fields are absent and
browser context is still available. Without either source, `event_context` is
omitted. This context schema has its own `schema_version: 1`.

`peer_ip` comes from the TCP connection. Without a trusted proxy, `client_ip`
equals `peer_ip` and `client_ip_source` is `connection`. Browser-provided fields
cannot replace this collector-derived context. IPv4-mapped IPv6 addresses are
stored as IPv4. The collector makes no outbound IP lookup and does not infer
viewer identity or location from the address.

For a reverse proxy, configure only addresses or CIDRs you control. For example,
`COLLECTOR_TRUSTED_PROXIES=10.20.1.2,2001:db8:abcd::/48`. Ensure each trusted proxy
overwrites incoming `X-Forwarded-For` with the actual source IP, or correctly
appends that source to the existing chain. Restrict direct access to the collector
where possible. Broad trust ranges can allow clients to forge their address.

The collector reads `X-Forwarded-For` only when the direct peer is trusted. It
combines all header lines in order, walks from right to left, skips configured
trusted hops, and selects the first untrusted address. This follows the
[trusted-proxy selection described by MDN](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/X-Forwarded-For#selecting_an_ip_address).
Only literal IPv4 or bare IPv6 entries are accepted, with at most 32 addresses
and 4096 bytes across the header values. Missing, malformed, excessive, or entirely
trusted chains leave `client_ip` and `client_ip_source` null while retaining
`peer_ip`. The event is still accepted. `Forwarded`, `X-Real-IP`, and
`CF-Connecting-IP` are ignored; configure the proxy to supply `X-Forwarded-For`.

The IP describes the HTTP sender. When a gateway batches multiple viewers' events,
the request context describes that gateway for every event in the batch. It cannot
recover those viewers' individual IPs. NAT, Docker networking and VPNs can also
change the visible address. Supplied `viewer_id` remains the basis for unique-viewer
counts. IP data is available only through the private raw-event API, with no IP
filter or GeoIP enrichment in this stage.

Set `COLLECTOR_STORE_IP=false` and restart to omit IP fields for new events.
Raw forwarding headers are neither stored nor logged, and normal request handling
does not log IP values. First accepted delivery wins, including whether context
was collected; retries never add or replace it. Retention and viewer deletion
remove the event's context with its row.

### Filters and pagination

All read endpoints accept `from`, `to`, `video_id`, `viewer_id`, `browser_name`,
`os_name`, and `device_type`. Timestamps use
RFC3339 with a timezone and are compared in UTC, with `from` inclusive and `to`
exclusive. The raw event endpoint also accepts `event_name`.

Browser and OS names use exact, case-sensitive matching and accept 1–64 Unicode
characters. Device type accepts `desktop`, `mobile`, `tablet`, or `unknown`.
Multiple filters combine with AND. Missing, null, and nonstring context values
do not match an explicit filter. `device_type=unknown` matches only a stored
`unknown` value; it does not include events with no device information.

`/v1/events` and `/v1/videos` accept `limit` from 1 to 1000, defaulting to 100.
Follow `next_cursor` until it is null. Keep the same project and filters when following a
cursor. Invalid cursors, changed filters, unknown parameters, and invalid ranges
return 400. Treat cursors as opaque.

Events are ordered by their server ingestion sequence, oldest first. Videos are
ordered by `video_id`. A pagination sequence is limited to events already committed
at its first page, so later arrivals do not shift pages or change video totals
within that sequence. Start without a cursor to refresh. This is not an indefinite
export snapshot; retention and deletion can remove records between requests.

For example, run this from your dashboard backend or terminal, using a local
`READ_TOKEN` environment variable containing the private token:

```sh
curl -H "Authorization: Bearer $READ_TOKEN" \
  'http://127.0.0.1:4174/v1/summary?video_id=video-123'
```

### Metric definitions

- A view is a distinct `view_id` with a retained `view_started` event. It represents
  a playback attempt, including attempts that fail before playback.
- Summary filters select views by the occurrence time, identity and context of
  their first retained start event. Browser/device changes on later events do not
  move the view into another filtered group. Each selected view contributes its entire retained cumulative playing
  time, including a final event that arrives after the selected date range. Raw
  queries instead filter each event's own `occurred_at` and context.
- Playing time uses the final `view_ended` value when present, otherwise the latest
  retained `view_snapshot`, otherwise zero. Records are ranked by SDK sequence,
  then occurrence time and ingestion order. Snapshots are never added together.
- A view without a retained start event is excluded from summaries. Its events
  remain available through raw queries. Duplicate starts for one view use the
  first start by SDK sequence, occurrence time, then ingestion order.
- Unique viewers are distinct non-null supplied `viewer_id` values on selected
  starts. Anonymous views still count as views. This does not identify verified
  people, and per-video viewer counts should not be summed for a global count.
- Final events do not imply video completion. Missing events can make metrics
  incomplete. Client occurrence times and metric values are supplied by the SDK;
  an ingest key does not verify their authenticity.

The server stores receipt time separately for retention and operations.
Server operational messages use stdout/stderr and contain no keys. CLI commands
that create or rotate project credentials return new secrets once on stdout.

## Retention and administration

All commands use `COLLECTOR_DATA_DIR`. Administration requires local filesystem
access to that directory; there are no remote admin endpoints. Key commands and
`project-list` work live on Linux and macOS. Other administration requires a
stopped collector.
Invalid commands or configuration return a nonzero exit status.

### Retention

By default, the collector removes events received more than 30 days ago. It runs
a sweep before accepting requests at startup and then once an hour. Expired
events can remain until the next successful sweep. A failed scheduled sweep logs
an error and retries at the next interval; a failed startup sweep prevents startup.

Retention uses the stored server `received_at`, independently of the client's
`occurred_at`. An old client timestamp does not cause immediate removal, and a
future client timestamp does not extend storage. Expiration removes the canonical
event, its OTLP log/context, and its request context in one transaction.
Summaries use only retained events, so expiration also removes their contributions.

For 60 days, set `COLLECTOR_RETENTION_SECONDS=5184000`. For indefinite storage,
set `COLLECTOR_RETENTION_SECONDS=0`. Restart the collector after changing settings.
Disabling retention preserves remaining events; it does not recover deleted ones.
Duplicate delivery does not refresh an existing event's receipt time. After an
event expires or is deleted, receiving its ID again creates a new stored event.

### Delete a viewer's stored events

Stop the collector, then run:

```sh
./target/debug/chunkify-analytics-collector delete-viewer 'viewer-123'
```

The one-argument form targets the `default` project. To target another project,
use `delete-viewer live viewer-123`. The command matches the exact supplied
`viewer_id` within that project, across all videos and both ingestion formats. It removes the matching events, OTLP context, and request
context atomically, then prints a count such as `{"deleted_events":42}`.
Other viewers and anonymous
events remain. Repeating the command succeeds with zero deletions. Restart the
collector to resume ingestion; queries and totals reflect the deletion.

This deletes stored records. It does not block new events for that viewer or
erase existing backups. Apply the customer's collection policy in the application
and manage backup retention separately. Deletion does not promise an immediate
reduction in the size of the DuckDB file.

### Rotate a credential

With `COLLECTOR_DATA_DIR` set, run one of:

```sh
./target/debug/chunkify-analytics-collector rotate-key ingest
./target/debug/chunkify-analytics-collector rotate-key read
```

These legacy commands rotate the `default` project’s primary credential.
Each command replaces only the selected credential. It writes a complete new
`keys.json` through a temporary file and rename, with mode `0600` on Unix. It
does not print credential values or modify event data.

Read the new value from `keys.json`. For an ingest-key rotation, update the
browser adapter's `X-Chunkify-Ingest-Key`. For a read-token rotation, update the
dashboard backend's bearer token. A running collector applies the change
immediately on Linux and macOS; if stopped, start it to use the new key. The old
selected credential is no longer accepted, with no overlap period. Pages still using the
old ingest key will fail to deliver events until they load the new configuration.

### Backup and restore

Use an offline copy of the complete data directory. Stop the collector gracefully
and wait for it to exit, then keep it stopped until the copy finishes. Shutdown
runs a DuckDB [checkpoint](https://duckdb.org/docs/current/sql/statements/checkpoint)
and closes the database. Include the database, any WAL file, keys, and lock file.
Keep the backup private because it contains the read token and analytics data.
Save your deployment environment settings separately.

For example, with a backup destination that does not already exist:

```sh
# After stopping the collector and waiting for it to exit:
cp -a "$COLLECTOR_DATA_DIR" /srv/backups/analytics-backup
# Restart the collector using its original data directory.
```

To restore, stop the collector and copy the backup into a new directory:

```sh
cp -a /srv/backups/analytics-backup /srv/analytics-restored
export COLLECTOR_DATA_DIR=/srv/analytics-restored
./target/debug/chunkify-analytics-collector serve
```

The restore destination must not already exist. Use the same collector version,
or a version that supports upgrading its database schema. Preserve directory
permissions and ensure the account running the collector can write there. Do not
run `init` on restored data. Check `/healthz` and the authenticated event and
summary endpoints before sending traffic to the restored installation.

Restoring also restores the keys and events from the backup date. Changes after
that date are absent. Reapply later viewer deletions and key rotations as needed.
Startup retention still applies, so an older backup can lose expired events
when it starts. To inspect it without expiration, explicitly disable retention
on the restored copy before starting it.

## Development

The collector is a Cargo project at the repository root. Node.js is needed only
for integration tests and benchmarks. `npm ci` installs the published `@chunkify/analytics@0.4.0` SDK and the test
tools pinned in `package-lock.json`.
Its integration tests use the real JavaScript analytics package and
Chunkify/HTTP/OTLP adapters, then query a running service before and after a restart.
Tests use temporary data directories and require no external services.
They also exercise scheduled/startup retention, disabled retention, offline viewer
deletion, key rotation, and backup/restore through the public CLI and HTTP interfaces.

From the repository root, with Cargo available on PATH:

```sh
npm ci
npx playwright install --with-deps --only-shell chromium
npm run test:collector
cargo fmt --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
```

The collector CI job runs these checks and Rust/MCP tests. The container CI job
runs Docker lifecycle and benchmark-runner checks. `COLLECTOR_BINARY` can point
the Node integration tests at another built collector executable.

CI reports builds and integration tests as separate steps. Native builds cache
Cargo dependencies for the selected toolchain. Docker builds persist both image
layers and the registry/target cache mounts, following [Docker's cache-mount
guidance](https://docs.docker.com/build/ci/github-actions/cache/#cache-mounts).
The Docker Cargo cache key includes the runner platform, Dockerfile, manifest and
lockfile. The build removes only this package's release artifacts before compiling,
so source changes and reverts rebuild the app while reusing its dependencies.
CI changes a build argument to run Cargo on every attempt without clearing its
cache mounts, so an image-layer hit cannot hide an empty mount. A cold cache still requires compiling bundled DuckDB. CI builds a
local test image and does not publish it.

To exercise an upgrade, also set `COLLECTOR_PREVIOUS_BINARY` to a previously built
collector executable when running `npm run test:collector`. The upgrade test writes
through that service, stops it abruptly, starts the current service with the same
data directory, and checks the old records, keys, new OTLP writes, and another restart.
Without this setting, the upgrade test is skipped.

### Container checks and capacity measurements

With Docker running, build and exercise the image from the repository root:

```sh
npm run test:collector:docker
```

This uses the real native and OTLP adapters, verifies non-root execution and health
reporting, stops the container gracefully, and creates a replacement with the same
volume. It checks the original credentials, raw events, OTLP and IP context,
deduplication, and summary after replacement. The dedicated CI container job runs this test.
Test resources use unique names and are removed afterward. To test another prebuilt
image, set `COLLECTOR_IMAGE` when running the Node test directly.

Run capacity measurements separately; throughput is not a CI pass/fail gate:

```sh
COLLECTOR_LOAD_REPORT=/tmp/collector-capacity.json npm run benchmark:collector
```

The command builds the image, then runs the matrix below. To reuse an
already built image, run `node tests/http/docker/load.mjs` directly. Set
`COLLECTOR_IMAGE` to select a different local image. The JSON report records its
immutable image ID separately from the current checkout's source revision.

List the plan without contacting Docker:

```sh
node tests/http/docker/load.mjs --list
```

| Cases | Workload |
| --- | --- |
| `native-100`, `native-200`, `native-400`, `native-500`, `native-750`, `native-800`, `native-1000`, `native-1600`, `native-2000`, `native-3000`, `native-4000` | One native event per request, at the named requests/s |
| `preloaded-native-750`, `preloaded-native-1000`, `preloaded-native-2000`, `preloaded-native-3000` | The corresponding native traffic after inserting 50,000 events through OTLP |
| `otlp-100`, `otlp-200`, `otlp-400`, `otlp-800`, `otlp-1000` | One OTLP record per request, at the named requests/s |
| `otlp-batch10`, `otlp-batch25`, `otlp-batch25-gzip` | 100 requests/s with 10 or 25 records, optionally compressed |
| `otlp-batch10-low`, `otlp-batch25-low` | 250 records/s using 25 requests/s of 10 records or 10 requests/s of 25 records |
| `mixed` | Alternating native and 10-record OTLP requests, 100 requests/s, every fifth request repeats acknowledged events |
| `mixed-large` | Mixed requests at 50 requests/s; every fourth new event has a synthetic 16 KiB mapper property; OTLP uses gzip |
| `preloaded-mixed` | Same traffic as `mixed`, after inserting 50,000 events through OTLP |
| `burst-recovery` | Native traffic at 100, then 1,600, then 100 requests/s; 20%, 40%, 40% of the duration |
| `rate-cap` | 400 OTLP requests/s with 10 records each, against the normal 100-request/s admission limit |

Each case has its own temporary volume and a container with two CPUs, 2 GiB RAM,
and no extra swap. The default duration is 30 seconds per case, excluding preload,
startup, restart, and correctness checks. The complete matrix takes several
minutes. Other running containers are counted in the report and left running.

### Benchmark controls

| Environment variable | Default | Allowed values |
| --- | --- | --- |
| `COLLECTOR_LOAD_SECONDS` | `30` | 5..300 seconds per case |
| `COLLECTOR_LOAD_CPUS` | `2` | 1..16 CPUs, no more than the Docker engine provides |
| `COLLECTOR_LOAD_MEMORY_MIB` | `2048` | 1024..16384 MiB hard container limit with swap disabled. Must leave at least 512 MiB for the Docker VM; concurrent workloads require additional headroom. |
| `COLLECTOR_LOAD_CASES` | all cases | Comma-separated names from the table |
| `COLLECTOR_LOAD_PRELOAD_EVENTS` | `50000` | 0..100000, used by `preloaded-mixed` and `preloaded-native-*`; preload and timed traffic share the 200,000 unique-event guard |
| `COLLECTOR_LOAD_CONCURRENCY` | `128` | 1..4096 requests in flight; higher concurrency still uses the same memory and disk guards |
| `COLLECTOR_LOAD_PROFILE` | `0` | `1` enables the collector's aggregate shutdown profile; `0` disables it |
| `COLLECTOR_LOAD_QUERIES` | `1` | `0` disables concurrent dashboard reads to isolate ingestion; retention and health checks remain enabled |
| `COLLECTOR_LOAD_BROWSER_CONTEXT` | `0` | `1` captures browser context once through the real Chunkify adapter in Chromium, closes the browser, then replays it in native and OTLP fixtures. The replay omits null browser fields, which the OTLP adapter cannot retain. Requires Playwright Chromium. |
| `COLLECTOR_LOAD_INSERT_MODE` | server default | Override `COLLECTOR_INSERT_MODE` for a row/bulk comparison |
| `COLLECTOR_LOAD_CHECKPOINT_THRESHOLD_MIB` | server default | Override the automatic checkpoint threshold; 1..1024 MiB |
| `COLLECTOR_LOAD_BATCH_MAX_EVENTS`, `COLLECTOR_LOAD_BATCH_MAX_BYTES`, `COLLECTOR_LOAD_BATCH_DELAY_MS`, `COLLECTOR_LOAD_QUEUE_MAX_BYTES`, `COLLECTOR_LOAD_QUEUE_MAX_REQUESTS` | server defaults | Override the corresponding write-batching settings; effective overrides are recorded in the report |
| `COLLECTOR_LOAD_REPORT` | stdout | Output JSON path, saved after each completed case |
| `COLLECTOR_IMAGE` | `chunkify-analytics-collector:test` | Existing local image name |

For a longer measurement around a candidate rate, using the existing image:

```sh
COLLECTOR_LOAD_CASES=native-200,otlp-200 \
COLLECTOR_LOAD_SECONDS=120 \
COLLECTOR_LOAD_REPORT=/tmp/collector-longer.json \
node tests/http/docker/load.mjs
```

The collector's benchmark settings raise the rate cap to 10,000 requests/s,
except in `rate-cap`, and raise the request body bound to 1 MiB for OTLP batches.
The record limit remains 100. Retention scans run every five seconds with the
normal 30-day lifetime. This exercises concurrent scans, but does not benchmark
large expired-data deletions. These settings do not change deployment defaults.

### Workload and measurements

The runner records all seven event types from a real SDK playback with nonzero
playing time, and captures the public HTTP and OTLP adapters' actual wire output.
It replays those templates with fresh event/view IDs, video/viewer IDs, and event
occurrence timestamps. There are ten video IDs and up to 1,000 viewer IDs. OTLP
batching and compression model a gateway; neither SDK adapter batches events.
The optional larger property is repeatable synthetic text, not browser metadata.
The verification ledger retains each event's template selection, original dynamic
fields and first submitted format. It reconstructs the complete expected event
and OTLP record during restart verification. This avoids retaining full event
and OTLP copies for every submission while preserving full-payload comparisons.

Traffic follows a schedule independent of response completion. If the generator
is full, or more than 50 ms behind schedule, it records skipped requests instead
of accumulating an unbounded queue. The report separates scheduled, submitted,
successful, and rejected traffic; requests/s and records/s; and newly acknowledged
events from duplicate acknowledgements. HTTP errors, timeouts, and uncertain
records have separate counts. Persistence checks report committed records whose
response was not received.

A generator pause longer than five seconds invalidates the case and stops the
run, for example if the machine sleeps. Keep the host awake during measurement;
on macOS, `caffeinate -i node tests/http/docker/load.mjs` holds a temporary
idle-sleep assertion for the command's lifetime.

Request latency measures HTTP delivery after serialization and compression.
Preparation time and scheduling lag are reported separately. Distributions include
p50, p95, and p99; successful-request latency is also separate, because fast error
responses can make an overloaded collector appear faster. Summary, per-video,
and paginated raw-event reads run concurrently. Their latency distributions cover
successful responses, and failures have separate counts. Burst phases drain
outstanding requests between phases and report the drain duration.

Resource sampling requires Linux cgroup v2 and memory pressure accounting,
including inside Docker Desktop. Samples include process RSS, cgroup memory,
anonymous memory, file cache, inactive file pages, dirty/writeback pages, memory
limit/OOM counters and cumulative `some`/`full` pressure stall time in microseconds.
These categories can overlap; do not add them together or assume all inactive
file pages are immediately reclaimable. The
[Linux cgroup documentation](https://docs.kernel.org/admin-guide/cgroup-v2.html)
defines these counters, and [PSI documentation](https://docs.kernel.org/accounting/psi.html)
explains stall accounting. Samples also include
container CPU, data-directory and WAL sizes, free host/volume disk, and generator
RSS, V8 heap usage/capacity, external memory and array-buffer memory. The generator
also reports CPU and event-loop delay. Sampling incurs Docker
exec overhead and can miss peaks. Enable `COLLECTOR_LOAD_PROFILE=1` to include
`collector_profile` in each result. The profile records count, total milliseconds,
and maximum milliseconds for queue wait and execution by database operation, plus
ingestion transaction begin, request preparation, event append/insert, OTLP insert,
staging cleanup and commit. Divide total time by count for the mean. Version 2
profiles count queue wait per request and worker execution per shared transaction.
Ingest counters describe committed transactions and requests, received records
including duplicates, inserted records, and total/maximum prepared bytes and
maximum records/requests per batch.
Current profiles also record DuckDB's version and effective checkpoint threshold,
thread count and memory limit in `database`. These settings are read once at
worker startup when profiling is enabled.

Profiles cover the process lifetime, including preload, health checks, startup
retention, and work drained after traffic ends. Schema initialization and the final
checkpoint are excluded. Stage timing counts include failed attempts; ingest
counters count only committed transactions. Event-insert timing includes SQL
preparation or cache lookup, parameter binding, and execution. `prepare_request`
measures payload/context/envelope serialization and timestamp parsing before queue
admission; it is outside worker execution time. Queue wait includes the collection
delay. Profiles do not include HTTP parsing or
validation, queue rejection counts, queue depth, or internal timing percentiles.
Queue waits overlap across requests and must not be added to worker execution time.
The worker reports before its final checkpoint, so a profile alone does not prove
clean shutdown. The runner also verifies the exit status and persisted records.
SIGKILL produces no shutdown profile. Compare runs with profiling enabled on both
versions, because instrumentation itself has a cost.

By default, the runner stops at a sampled 512 MiB data directory, less than 4 GiB free disk
on either the host or volume, 1,800 MiB container memory, 1 GiB generator RSS, or
200,000 submitted unique events in one case. HTTP requests time out after ten
seconds. Docker also enforces the configured hard memory limit without swap.
Changing `COLLECTOR_LOAD_MEMORY_MIB` scales the early-stop guard by the same
1,800/2,048 ratio, rounded down. A 4 GiB limit therefore has a 3,600 MiB guard.
The report records both limits. Safety stops and
correctness failures make the command exit unsuccessfully and retain the partial
report. Temporary containers and volumes are removed in cleanup. SIGINT/SIGTERM
stop admission and trigger cleanup; a forced termination can leave test resources.

Before the matrix runs, probe thresholds are fixed at 99% of scheduled requests
sent, 99% HTTP success, successful-ingest p95 at most 250 ms, query p95 at most
500 ms with no query failures, and scheduling-lag p95 at most 20 ms. An overload
or rate-limit case is expected to miss these thresholds. Missing a performance
threshold is reported, but does not fail CI or invalidate the measured result.
A short passing probe is only a candidate operating rate, not a sustained limit.
The separate `meets_full_delivery_thresholds` verdict requires those same latency
and query checks plus every scheduled request being submitted and succeeding.
An omitted or failed ingestion request makes that stricter verdict false, even
when `meets_probe_thresholds` is true.

After each case, the runner replaces the container and reads every event page.
It checks acknowledged IDs, full canonical payloads, original OTLP context,
deduplication, global/per-video views, unique viewers, and cumulative playing time.
The compressed 25-record case uses SIGKILL; other cases use graceful shutdown.
Shutdown and startup times and final checkpointed storage size are reported.
The small benchmark-accounting and lifecycle tests run in Docker CI without
asserting a machine-specific throughput target.

The [write-path performance report](performance.md) covers profiling and the first
prepared-statement optimization on the current schema, with IP storage enabled.
The [batching report](batching.md) compares shared transactions and bulk insertion,
including follow-up measurements with browser context and a preloaded database.
The [expanded capacity report](capacity-expanded.md) contains the earlier results
and raw JSON reports, including the rejected interrupted measurement.
The [initial capacity report](capacity.md) preserves the earlier six-case baseline.
The expanded matrix has different playback fixtures, scheduling, queries, and
host activity, so it should not be treated as a direct speed comparison with that
baseline. Compare future collector changes using the same runner, settings,
hardware, retained dataset, and workload. Thirty-day capacity needs a larger,
representative retained dataset and deployment-specific measurements. These
earlier capacity reports predate browser/IP enrichment and the flat context column.
Use the later performance and batching reports for measurements of those changes.

## Experimental durable queue

The [prototype comparison](durable-queue.md) records the implementation, recovery
tests and measured results.

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
It is not a production recommendation or a throughput guarantee. The comparison
must check processing backlog and drain time as well as HTTP latency.

For a matched benchmark, select one workload and set
`COLLECTOR_LOAD_COMPARE_QUEUE=1`. The runner preloads once, stops the collector,
and copies that checkpointed volume into a temporary fixture. Both runs use the
same starting data. The second run enables the queue. All fixtures are disposable;
if verification of queued acknowledgements fails, its data volume is retained
and its name is printed for investigation.

## Experimental separate reader

The [matched comparison](read-worker.md) records correctness checks and results.

Set `COLLECTOR_EXPERIMENTAL_READ_WORKER=true` to execute `/v1/events`,
`/v1/summary`, and `/v1/videos` on one dedicated thread with its own connection
to the same embedded DuckDB instance. This works with direct ingestion or the
durable queue. It is off by default while the comparison is evaluated.

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
The benchmark records these as `reader_profile` and `collector_profile`.

To compare one selected workload with the same preloaded data, set
`COLLECTOR_LOAD_COMPARE_READS=1`. Both conditions use the same image and settings;
only the separate-reader option changes. Set `COLLECTOR_LOAD_DURABLE_QUEUE=1`
to enable durable ingestion in both. Do not combine this with
`COLLECTOR_LOAD_COMPARE_QUEUE`, which tests a different change.

## Longer stability runs

The benchmark defaults to a 200,000-event ceiling and a 512 MiB data-directory
ceiling. Explicit overrides allow `COLLECTOR_LOAD_MAX_EVENTS=1000000` and
`COLLECTOR_LOAD_MAX_DATA_MIB=3072` for a five-minute, 3,000-request/s run with a
50,000-event preload. Event limits cannot exceed one million, and the data limit
cannot exceed 4 GiB. Preload and reused fixtures count toward the event limit.
Before starting, free host disk must cover the data budget plus the 4 GiB floor.
Runtime disk, proportional container-memory and 1,024 MiB generator-memory guards
remain active. The default container limit is 2 GiB without swap; set
`COLLECTOR_LOAD_MEMORY_MIB=4096` to compare a 4 GiB budget without changing the
CPU limit or collector workload.

The public `load.mjs` command launches its worker with a 512 MiB old-generation
budget and a 64 MiB semi-space budget. On the tested Node 24 runtime this gives
a 704 MiB V8 heap limit, leaving room below the RSS guard for other allocations.
The worker refuses an effective heap limit above 768 MiB before starting Docker.
Reports record the effective heap limit and Node flags. Use the public command;
remove conflicting heap settings from `NODE_OPTIONS` if the startup check fails.

The verifier stores encoded replay descriptions with compact UUID strings and
releases each description after checking that event. It accumulates only the
per-view fields needed for independent summaries. Every acknowledged event,
complete payload, original OTLP envelope, browser/IP context and pagination
check remains mandatory. Resource guards also run during restart verification;
unverified queued acknowledgements retain their test volume for investigation.

Reports include one-minute request and query latency windows, progress snapshots
about every 30 seconds, and verification resource samples. Windows group requests
by submission time and successful queries by start time. Scheduled/omitted
requests use their intended submission time. Overall latency and full-delivery
thresholds remain unchanged. `COLLECTOR_LOAD_SECONDS=300` sets five minutes;
enable both `COLLECTOR_LOAD_DURABLE_QUEUE=1` and `COLLECTOR_LOAD_READ_WORKER=1` to
exercise the two optional features together.

Report version 6 also distinguishes completed queue drains from interrupted waits.
`durable_queue.drain_completed` is true only after an empty queue sample.
`after_drain_wait` preserves the last sample, `drain_wait_ms` records elapsed wait
time in either case, and `drain_ms` is null when the wait stops before observing
an empty queue. Graceful shutdown and restart verification remain separate checks.

The [five-minute stability attempt](stability.md) stopped after 101 seconds when
the generator exceeded its memory guard. All acknowledged events survived
restart, but request omissions and dashboard latency also failed the targets.
It does not establish five-minute capacity; the report records the next steps.
The subsequent [generator memory investigation](generator-memory.md) reproduced
and removed that RSS failure in isolated sender tests, then checked the launcher
against Docker. A sustained collector capacity run is still required.

The [retry with the bounded generator](stability-retry.md) reached 2,989.71
successful requests/s for 109 seconds before crossing the collector's 1,800 MiB
container-memory guard. All 377,112 acknowledged events survived restart, and
aggregate latency targets passed. It still failed the planned five-minute run;
the report distinguishes cgroup memory from process RSS and identifies the next
memory measurements and resource comparison.

The subsequent [4 GiB comparison](capacity-4g.md) completed five minutes with
2 CPUs, but failed the capacity targets. It averaged 2,832.35 successful
requests/s, returned 48,411 HTTP 503 responses as the durable queue approached
its byte limit, and exceeded the dashboard p95 targets. All 900,890 acknowledged
events survived restart. Memory counters, queue growth and database profiles
identify the next optimization work in that report.
