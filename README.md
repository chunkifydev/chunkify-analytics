# Chunkify analytics collector

A Rust service that stores Chunkify playback events in an embedded DuckDB file.
The implementation follows [issue #381](https://github.com/chunkifydev/chunkify/issues/381).

## Implementation stages

1. Native HTTP ingestion, durable storage, separate ingest/read keys, event queries,
   and summary metrics. Implemented.
2. OTLP/HTTP JSON ingestion, including batched logs and gzip. Implemented.
3. Retention, viewer deletion, key rotation, and backup/restore procedures. Implemented.
4. Docker packaging, container lifecycle tests, and measured capacity guidance.

This is a development version. Docker packaging and capacity measurements remain
for stage 4. MCP is a separate potential follow-up.

## Build and run

Install Rust and a C++ compiler. CI uses Rust 1.99.0 and Node.js 24. DuckDB is compiled into the executable, so the
first build takes longer than later builds. No separate database server is needed.
The lockfile records the exact Rust dependencies.

From this directory:

```sh
cargo build --locked
export COLLECTOR_DATA_DIR="$PWD/data"
export COLLECTOR_ALLOWED_ORIGINS="http://127.0.0.1:4173"
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
The collector and administration commands hold an exclusive `collector.lock`
file lock for their lifetime. Stop the collector before running administration
commands. The OS releases the lock after a crash; leave the lock file in place.
Startup upgrades a stage 1 database to schema version 2 without dropping events
or changing keys. Stop the old service before starting the new executable.

## Send analytics

Use the analytics package from this branch:

```js
import { monitor } from "@chunkify/analytics";
import { createHttpAdapter } from "@chunkify/analytics/http";

monitor(document.querySelector("video"), {
  videoId: "video-123",
  destinations: [createHttpAdapter("http://127.0.0.1:4174/v1/events", {
    headers: { "X-Chunkify-Ingest-Key": "COPY_YOUR_PUBLIC_INGEST_KEY" },
  })],
});
```

Send the complete SDK event. Custom mappings that remove required fields cannot
be accepted. The collector stores the complete JSON object, including additional
fields, and deduplicates by `event_id`. Repeating a valid event does not replace
the first payload or count it twice.

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

| Environment variable | Default | Meaning |
| --- | --- | --- |
| `COLLECTOR_DATA_DIR` | `./data` | Directory for keys, database, and write-ahead log, relative to the working directory. |
| `COLLECTOR_BIND` | `127.0.0.1:4174` | Listening IP address and port. Use `0.0.0.0:4174` behind an ingress when needed. |
| `COLLECTOR_ALLOWED_ORIGINS` | Empty | Comma-separated exact website origins, without paths or trailing slashes. Empty denies requests with an Origin header. Server requests without Origin still require the ingest key. |
| `COLLECTOR_MAX_BODY_BYTES` | `65536` | Maximum request body size in bytes. OTLP gzip requests must fit this limit both before and after decompression. Must be positive. |
| `COLLECTOR_MAX_OTLP_RECORDS` | `100` | Maximum log records per OTLP request, across all resource and scope groups, including duplicates. Must be positive. |
| `COLLECTOR_INGEST_PER_SECOND` | `100` | Global ingestion token bucket. Capacity and refill rate both use this value. Must be positive. |
| `COLLECTOR_RETENTION_SECONDS` | `2592000` | Retention by server receipt time, in seconds. Default is 30 days. `0` disables expiration. Must be a non-negative whole number no larger than `9223372036854775`. |
| `COLLECTOR_RETENTION_INTERVAL_SECONDS` | `3600` | Seconds between completed retention sweeps while serving. Whole number from 1 to 86400. Ignored when retention is disabled. |

Both ingestion endpoints share the rate limit. A batch consumes one request.
The service rejects excess ingestion requests with HTTP 429 and `Retry-After: 1`.
The database queue holds at most 64 waiting operations; a full queue returns 503.
The SDK continues to send once without retrying. Limits are configurable safeguards,
not a measured throughput guarantee. Configure connection and request timeouts at
your ingress before exposing this development service publicly.

## HTTP API

| Method and path | Authorization | Response |
| --- | --- | --- |
| `GET /healthz` | None | `{"status":"ok"}` after a database check, or 503 if unavailable. |
| `POST /v1/events` | `X-Chunkify-Ingest-Key` | 201 with `{"accepted":true,"duplicate":false}` after commit; 200 with `{"accepted":false,"duplicate":true}` for an existing event. |
| `POST /v1/logs` | `X-Chunkify-Ingest-Key` | OTLP JSON logs. 200 with `{}` after the whole batch commits, including empty or duplicate-only batches. |
| `GET /v1/events` | `Authorization: Bearer READ_TOKEN` | `{"events":[{"received_at":"...","event":{...}}],"next_cursor":null}` |
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

### Filters and pagination

All read endpoints accept `from`, `to`, `video_id`, and `viewer_id`. Timestamps use
RFC3339 with a timezone and are compared in UTC, with `from` inclusive and `to`
exclusive. The raw event endpoint also accepts `event_name`.

`/v1/events` and `/v1/videos` accept `limit` from 1 to 1000, defaulting to 100.
Follow `next_cursor` until it is null. Keep the same filters when following a
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
- Summary filters select views by the occurrence time and identity of their start
  event. Each selected view contributes its entire retained cumulative playing
  time, including a final event that arrives after the selected date range. Raw
  queries instead filter each event's own `occurred_at`.
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
Operational messages use stdout/stderr and contain no keys.

## Retention and administration

All commands use `COLLECTOR_DATA_DIR`. Administration requires local filesystem
access to that directory and a stopped collector; there are no remote admin endpoints.
Invalid commands or configuration return a nonzero exit status.

### Retention

By default, the collector removes events received more than 30 days ago. It runs
a sweep before accepting requests at startup and then once an hour. Expired
events can remain until the next successful sweep. A failed scheduled sweep logs
an error and retries at the next interval; a failed startup sweep prevents startup.

Retention uses the stored server `received_at`, independently of the client's
`occurred_at`. An old client timestamp does not cause immediate removal, and a
future client timestamp does not extend storage. Expiration removes the canonical
event and its OTLP log/context in one transaction. Summaries use only retained
events, so expiration also removes their contributions.

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

The command matches the exact supplied `viewer_id` across all videos and both
ingestion formats. It removes the matching events and OTLP context atomically,
then prints a count such as `{"deleted_events":42}`. Other viewers and anonymous
events remain. Repeating the command succeeds with zero deletions. Restart the
collector to resume ingestion; queries and totals reflect the deletion.

This deletes stored records. It does not block new events for that viewer or
erase existing backups. Apply the customer's collection policy in the application
and manage backup retention separately. Deletion does not promise an immediate
reduction in the size of the DuckDB file.

### Rotate a credential

Stop the collector and run one of:

```sh
./target/debug/chunkify-analytics-collector rotate-key ingest
./target/debug/chunkify-analytics-collector rotate-key read
```

Each command replaces only the selected credential. It writes a complete new
`keys.json` through a temporary file and rename, with mode `0600` on Unix. It
does not print credential values or modify event data.

Read the new value from `keys.json`. For an ingest-key rotation, update the
browser adapter's `X-Chunkify-Ingest-Key`. For a read-token rotation, update the
dashboard backend's bearer token. Restart the collector. The old selected
credential is no longer accepted, with no overlap period. Pages still using the
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

The collector is a Cargo project under `services/`, separate from the npm packages.
Its integration tests use the real JavaScript analytics package and HTTP/OTLP adapters,
then query a running service before and after a restart. Tests use temporary data
directories and require no external services.
They also exercise scheduled/startup retention, disabled retention, offline viewer
deletion, key rotation, and backup/restore through the public CLI and HTTP interfaces.

From the repository root, with Cargo available on PATH:

```sh
npm ci
npm run test:collector
cargo fmt --manifest-path services/analytics-collector/Cargo.toml --check
cargo clippy --locked --manifest-path services/analytics-collector/Cargo.toml -- -D warnings
```

The dedicated collector CI job runs these checks. Existing npm and browser checks
continue in the main CI job. `COLLECTOR_BINARY` can point the Node integration
tests at another built collector executable.

To exercise an upgrade, also set `COLLECTOR_PREVIOUS_BINARY` to a previously built
collector executable when running `npm run test:collector`. The upgrade test writes
through that service, stops it abruptly, starts the current service with the same
data directory, and checks the old records, keys, new OTLP writes, and another restart.
Without this setting, the upgrade test is skipped.
