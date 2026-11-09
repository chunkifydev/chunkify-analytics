# Configuration

Copy [config.toml.example](../config.toml.example) to `config.toml`.
The analytics server loads it from the working directory, or use `--config`:

```sh
chunkify-analytics --config /etc/chunkify/config.toml config check
chunkify-analytics --config /etc/chunkify/config.toml serve
```

Relative TOML paths resolve beside the file. CLI and environment paths resolve
from the working directory. `config check` validates without contacting
ClickHouse. Unknown fields and invalid values fail startup. Restart the
analytics server after changing installation settings, and give every server
sharing a database the same settings.

## Environment overrides

TOML holds application settings. Copy [.env.example](../.env.example) to `.env`
for deployment overrides, or set these variables directly in your container:

| Variable | TOML field | Use |
| --- | --- | --- |
| `CHUNKIFY_ADMIN_KEY` | Environment only | Enables project creation and key recovery through the API. Unset or empty disables these admin routes. |
| `CHUNKIFY_GEOIP_DATABASE` | `server.geoip_database` | Replace the bundled City MMDB with a local file. |
| `CHUNKIFY_BIND` | `server.bind` | HTTP listening address and port, for example `0.0.0.0:4174`. |
| `CHUNKIFY_CLICKHOUSE_URL`, `CHUNKIFY_CLICKHOUSE_DATABASE` | `clickhouse.url`, `clickhouse.database` | ClickHouse connection. |
| `CHUNKIFY_CLICKHOUSE_USER`, `CHUNKIFY_CLICKHOUSE_PASSWORD` | `clickhouse.user`, `clickhouse.password` | ClickHouse credentials, without putting the password in TOML. |
| `CHUNKIFY_CLICKHOUSE_NODE_ID` | `clickhouse.node_id` | Optional distinct number for each server sharing a database; leased automatically when unset. |
| `CHUNKIFY_MCP_URL`, `CHUNKIFY_MCP_SECRET_KEY` | `mcp.url`, `mcp.secret_key` | Optional stdio adapter connection and secret. |

The analytics server also loads `.env` from the working directory. Use `--env-file` to
select another file. Precedence is CLI flags, process environment, environment
file, TOML, then defaults. Only the variables above configure the application.

The former `COLLECTOR_*` overrides have been renamed to `CHUNKIFY_*` and are no
longer recognized. Update existing environment files and deployment settings.

The Docker image sets `CHUNKIFY_BIND=0.0.0.0:4174`, which overrides TOML; change
it with container settings or CLI flags. Containers load `/data/config.toml`.

## Storage

The `[clickhouse]` section selects the database shared by every analytics
server. See [storage](storage.md) for each field, scaling and replication.

## Application settings

### Installation and data policy

| TOML field | Default | Meaning |
| --- | --- | --- |
| `server.bind` | `127.0.0.1:4174` | Listening IP address and port. Use `0.0.0.0:4174` behind an ingress when needed. |
| `server.public_url` | Unset | External HTTP(S) analytics server base URL for export download links. Set it behind HTTPS ingress or a URL prefix. No credentials, query or fragment. |
| `server.allowed_origins` | Empty | Shared website origins, without paths or trailing slashes. Each project can allow additional origins. Server requests without Origin still require the public key. |
| `retention.seconds` | `2592000` | Default retention by server receipt time, in seconds. Default is 30 days. Projects may override it; `0` disables expiration for inheriting projects. Must be a non-negative whole number no larger than `9223372036854775`. |
| `retention.interval_seconds` | `3600` | Seconds between completed retention sweeps while serving. Whole number from 1 to 86400. The timer stays active for live project changes; disabled policies delete nothing. |
| `server.geoip_database` | `../share/chunkify-analytics/dbip-city-lite.mmdb` relative to the executable | Local City MMDB loaded at startup. Continent, country and city lookup is always enabled; request IPs are discarded. See [geolocation](geolocation.md). |
| `server.trusted_proxies` | Empty | Array of literal IPv4/IPv6 addresses or CIDRs allowed to supply `X-Forwarded-For`. Empty trusts no proxies. No hostnames, ports, or wildcards. Use IPv4 CIDRs for IPv4-mapped addresses. |

### Admission limits

These limits apply to each analytics server.

| TOML field | Default | Meaning |
| --- | --- | --- |
| `server.max_body_bytes` | `65536` | Maximum JSON request body size in bytes. Must be positive. |
| `server.ingest_per_second` | `100` | Ingestion token bucket for this server. Capacity and refill rate both use this value. Must be positive. |
| `batch.queue_max_bytes` | `33554432` | Maximum bytes held by admitted prepared write buffers, including the batch being saved. Must be at least the batch byte limit, at most 268435456. |
| `batch.queue_max_requests` | `4096` | Maximum write requests waiting for a batch. Also bounds per-project outstanding writes. Range 1..4096. |

### Advanced performance settings

| TOML field | Default | Meaning |
| --- | --- | --- |
| `batch.max_events` | `10000` | Maximum records per write batch, including duplicates. Range 1..10000. A whole request must fit. |
| `batch.max_bytes` | `10485760` | Maximum prepared request-buffer bytes per batch. Range 1..67108864. See write batching below. |
| `batch.delay_ms` | `10` | Maximum time spent collecting a batch when no other requests are waiting. Range 0..1000 ms. `0` gives each request its own batch unless others are already queued. |

### Diagnostics

| TOML field | Default | Meaning |
| --- | --- | --- |
| `logging.level` | `info` | Server batch logs to stderr. Accepts `off`, `error`, `warn`, `info`, `debug` or `trace`. `warn` suppresses successful batch summaries; `debug` also reports batch starts. |
| `mcp.enabled` | `true` | Serve the authenticated `/mcp` endpoint in the analytics server process. |
| `mcp.allowed_hosts` | `localhost,127.0.0.1,[::1]` | Allowed HTTP hostnames or host:port authorities for MCP. Add the ingress hostname for remote clients. |

Each completed write batch emits one timestamped `INFO batch_saved` line. For
example, with the timestamp omitted:

```text
INFO batch_saved backend="clickhouse" requests=100 received=100 inserted=98 duplicates=2 bytes=243200 queue_ms=10 duration_ms=4
```

`received` counts validated events admitted to this batch, including duplicates;
requests rejected before entering the batch are not included. `inserted` counts
events stored by ClickHouse, and `duplicates` counts repeated event IDs that
were not stored again. `bytes` measures prepared buffers, not HTTP body size.
`queue_ms` is the oldest request's wait before saving, including batch
collection time; `duration_ms` measures the duplicate check and insert. Both
use whole milliseconds.

A failed batch emits one `ERROR batch_failed` line with counts and a fixed
`stage`, and none of its events are acknowledged. Logs contain no event values,
project IDs, SQL, request headers, or credentials. Dependency logs are disabled,
including at `debug` and `trace`. Existing startup, retention and
administration output is independent of `logging.level`.

### Batching and overload

Each server saves one batch at a time. Requests that arrive while a batch is
saved form the next batch, up to the event and byte limits, so batches grow with
load. A request larger than either batch limit returns 413. Increase both limits
when accepting larger events.

Ingestion rate limits return 429 with `Retry-After: 1`. Full request or write-buffer budgets
return 503. The byte budget covers prepared writes, not total process memory.
Batch delay excludes time waiting for ClickHouse, so it is not a response-time
limit. Graceful shutdown finishes admitted writes. See
[duplicate deliveries](storage.md#duplicate-deliveries).

### Read and export limits

Override these settings in the `[rate_limits]` section of `config.toml` and restart
the analytics server. They apply to each server, including requests through the
optional MCP companion. They have no environment-variable overrides.

| TOML field | Default | Meaning |
| --- | --- | --- |
| `rate_limits.read_per_minute` | `120` | Average analytics reads per minute per project. Range 1..1000000. |
| `rate_limits.read_burst` | `20` | Maximum burst of analytics reads per project. Range 1..1000000. |
| `rate_limits.read_concurrent_per_project` | `16` | Maximum queued or running analytics reads per project. Range 1..256. |
| `rate_limits.read_concurrent` | `32` | Maximum executing analytics reads across the server. Up to 1024 additional unique reads can wait for a slot. Range 1..256. |
| `rate_limits.export_links_per_minute` | `10` | Average export links created per minute per project, with the same burst capacity. Range 1..1000000. |
| `rate_limits.export_downloads_per_minute` | `2` | Average CSV download starts per minute per project, with the same burst capacity. Range 1..1000000. |
| `rate_limits.export_concurrent_per_project` | `1` | Maximum active CSV generations per project. Range 1..64. |
| `rate_limits.export_concurrent` | `4` | Maximum active CSV generations across the server. Range 1..64. |

Per-project concurrency cannot exceed the corresponding server setting. Rates
refill continuously using governor; for example, 120 reads/minute replenishes
one request every half-second, up to the configured read burst.

Each project shares its budgets across HTTP, MCP, private keys and export links
on one server. Key creation, rotation and live project settings updates do not
reset them. Budgets live in memory and reset when the analytics server restarts.
Ingestion uses its separate limits. Health checks, MCP discovery/documentation
and export HEAD requests do not run database queries and do not consume the
read/download budgets.

Cache hits and callers sharing an in-flight report do not consume read budgets.
Unique reads wait for a server execution slot in a bounded queue. Per-project
concurrency, a full server queue, or rate exhaustion return 429 without starting a
query. `Retry-After` gives the rounded-up time until the next rate allowance;
concurrency rejections use one second because the finish time is unknown.
MCP exposes the delay as `error.retry_after_seconds`. When a client disconnects,
its permits are released and ClickHouse cancels the query. Accepted exports
still include all matching rows, without an export row cap.

Each report is an independent ClickHouse query, so a slow report does not delay
ingestion or another project's reads beyond the shared server slots. Reports
see events stored when their query begins. Await an ingestion response before
reading if the result must include it.

### Report cache

`cache.report_ttl_seconds` caches reports on each server for 0 to 60 seconds.
The default is 0, which disables the cache. Reports can lag stored events by
this interval. Raw events, session lists and histories, and export downloads
bypass the cache. Authentication and authorization run on every HTTP and MCP
request, including cache hits.

Cache keys include the project, metric, complete query parameters, cursor and
retention policy. Concurrent identical requests share one query. Successful
reports up to 64 KiB are retained, with at most 1024 entries across the server
and 64 per project, bounding retained payloads to 64 MiB. Errors are not retained.
A full cache bypasses storage for new keys; existing hot entries remain usable.
