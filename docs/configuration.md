# Configuration

Copy [config.toml.example](../config.toml.example) to `config.toml`.
The analytics server loads it from the working directory, or use `--config`:

```sh
chunkify-analytics --config /etc/chunkify/config.toml config check
chunkify-analytics --config /etc/chunkify/config.toml serve
```

Relative TOML paths resolve beside the file. CLI and environment paths resolve
from the working directory. `config check` validates without creating storage
or contacting S3. Unknown fields and invalid values fail startup. Restart the
analytics server after changing installation settings.

## Environment overrides

TOML holds application settings. Copy [.env.example](../.env.example) to `.env`
for deployment overrides, or set these variables directly in your container:

| Variable | TOML field | Use |
| --- | --- | --- |
| `CHUNKIFY_ADMIN_KEY` | Environment only | Enables project creation and key recovery through the API. Unset or empty disables these admin routes. |
| `CHUNKIFY_DATA_DIR` | `data_dir` | Container state directory. |
| `CHUNKIFY_GEOIP_DATABASE` | `server.geoip_database` | Replace the bundled City MMDB with a local file. |
| `CHUNKIFY_BIND` | `server.bind` | HTTP listening address and port, for example `0.0.0.0:4174`. |
| `CHUNKIFY_STORAGE_BACKEND` | `storage.backend` | Select `local` or `s3` storage. |
| `CHUNKIFY_S3_URL`, `CHUNKIFY_S3_ENDPOINT`, `CHUNKIFY_S3_REGION` | Corresponding `storage` fields | S3 deployment connection. |
| `CHUNKIFY_S3_ACCESS_KEY_ID`, `CHUNKIFY_S3_SECRET_ACCESS_KEY`, `CHUNKIFY_S3_SESSION_TOKEN` | Corresponding `storage` fields | Inject credentials without putting them in TOML. |
| `CHUNKIFY_S3_URL_STYLE`, `CHUNKIFY_S3_USE_SSL` | Corresponding `storage` fields | Provider addressing and TLS. |
| `CHUNKIFY_MCP_URL`, `CHUNKIFY_MCP_SECRET_KEY` | `mcp.url`, `mcp.secret_key` | Optional stdio adapter connection and secret. |

The analytics server also loads `.env` from the working directory. Use `--env-file` to
select another file. Precedence is CLI flags, process environment, environment
file, TOML, then defaults. Only the variables above configure the application.
AWS credential providers read `AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`, and
`AWS_SESSION_TOKEN` from the process environment. Export or inject these variables
directly; `.env` only loads the application overrides listed above. On ECS, omit
access and secret keys to use the task role.

The former `COLLECTOR_*` overrides have been renamed to `CHUNKIFY_*` and are no
longer recognized. Update existing environment files and deployment settings.

The Docker image sets `CHUNKIFY_DATA_DIR=/data` and
`CHUNKIFY_BIND=0.0.0.0:4174`. These override TOML; change them with container
settings or CLI flags.

## Storage

See [storage](storage.md) for local and S3/R2 examples.

| TOML field | Default | Meaning |
| --- | --- | --- |
| `storage.backend` | `local` | `local` or `s3`. |
| `storage.path` | `data_dir/analytics.duckdb` | Local database file. Relative paths resolve beside the configuration file. |
| `storage.url` | Required for S3 | `s3://bucket/optional-prefix/`, without wildcards, credentials or escapes. |
| `storage.endpoint` | AWS regional endpoint | Hostname and optional port, without a scheme or path. R2 uses `ACCOUNT_ID.r2.cloudflarestorage.com`. |
| `storage.region` | `us-east-1` | Signing region. Use `auto` for R2. |
| `storage.access_key_id` | AWS credential chain | Provide together with `secret_access_key`, or omit both for the default credential chain. |
| `storage.secret_access_key` | AWS credential chain | Private S3 credential. |
| `storage.session_token` | Unset | Optional temporary-credential token. |
| `storage.url_style` | `path` | `path` or `vhost` bucket addressing. |
| `storage.use_ssl` | `true` | HTTPS for S3 connections; `false` selects HTTP. |
| `storage.flush_interval_seconds` | `0` | S3 only. `0` waits for uploads before responding. Values 1..3600 buffer events in memory and return the existing HTTP 201 response immediately. Upload after this many seconds from the first buffered request, or sooner when a batch fills. Set `60` for a one-minute window. See [storage guarantees](storage.md#buffered-s3-writes). |

## Application settings

### Installation and data policy

| TOML field | Default | Meaning |
| --- | --- | --- |
| `data_dir` | `./data` | Directory for keys, locks and temporary files. Also holds the default local database and WAL. Environment paths are relative to the working directory. |
| `server.bind` | `127.0.0.1:4174` | Listening IP address and port. Use `0.0.0.0:4174` behind an ingress when needed. |
| `server.public_url` | Unset | External HTTP(S) analytics server base URL for export download links. Set it behind HTTPS ingress or a URL prefix. No credentials, query or fragment. |
| `server.allowed_origins` | Empty | Shared website origins, without paths or trailing slashes. Each project can allow additional origins. Server requests without Origin still require the public key. |
| `retention.seconds` | `2592000` | Default retention by server receipt time, in seconds. Default is 30 days. Projects may override it; `0` disables expiration for inheriting projects. Must be a non-negative whole number no larger than `9223372036854775`. |
| `retention.interval_seconds` | `3600` | Seconds between completed retention sweeps while serving. Whole number from 1 to 86400. The timer stays active for live project changes; disabled policies delete nothing. |
| `server.geoip_database` | `../share/chunkify-analytics/dbip-city-lite.mmdb` relative to the executable | Local City MMDB loaded at startup. Continent, country and city lookup is always enabled; request IPs are discarded. See [geolocation](geolocation.md). |
| `server.trusted_proxies` | Empty | Array of literal IPv4/IPv6 addresses or CIDRs allowed to supply `X-Forwarded-For`. Empty trusts no proxies. No hostnames, ports, or wildcards. Use IPv4 CIDRs for IPv4-mapped addresses. |

### Admission limits

| TOML field | Default | Meaning |
| --- | --- | --- |
| `server.max_body_bytes` | `65536` | Maximum JSON request body size in bytes. Must be positive. |
| `server.ingest_per_second` | `100` | Global ingestion token bucket. Capacity and refill rate both use this value. Must be positive. |
| `batch.queue_max_bytes` | `16777216`; `33554432` for buffered S3 | Maximum bytes held by admitted prepared write buffers, including the active batch. Must be at least the batch byte limit, at most 268435456. |
| `batch.queue_max_requests` | `256`; `4096` for buffered S3 | Database channel capacity, shared by writes, reads and maintenance. Also bounds per-project outstanding writes. Range 1..4096. The byte budget also applies to writes. |

### Advanced performance settings

| TOML field | Default | Meaning |
| --- | --- | --- |
| `batch.max_events` | `100`; `10000` for buffered S3 | Maximum records per write batch, including duplicates. Range 1..10000. A whole request must fit. |
| `batch.max_bytes` | `1048576`; `10485760` for buffered S3 | Maximum prepared request-buffer bytes per batch. Range 1..67108864. See write batching below. |
| `batch.delay_ms` | `10` | Maximum time the worker spends collecting a batch. Range 0..1000 ms. `0` gives each request its own batch. |
| `duckdb.threads` | Automatic | Optional DuckDB execution thread count, from 1..256. Applies to the shared database used by writes and reads. HTTP runs separately. |
| `duckdb.checkpoint_threshold_mib` | `16` | DuckDB's automatic checkpoint threshold, in MiB. Integer from 1..1024. Local storage only. Larger values allow a larger write-ahead log between checkpoints; see below. |

### Diagnostics

| TOML field | Default | Meaning |
| --- | --- | --- |
| `logging.level` | `info` | Server batch logs to stderr. Accepts `off`, `error`, `warn`, `info`, `debug` or `trace`. `warn` suppresses successful batch summaries; `debug` also reports batch starts. |
| `batch.insert_mode` | `bulk` | `bulk` uses an appender and one event INSERT per transaction. `row` uses cached INSERT statements per event for diagnostic comparisons. Both share the same local batching and durability rules. S3 always uses an appender and Parquet output. |
| `mcp.enabled` | `true` | Serve the authenticated `/mcp` endpoint in the analytics server process. |
| `mcp.allowed_hosts` | `localhost,127.0.0.1,[::1]` | Allowed HTTP hostnames or host:port authorities for MCP. Add the ingress hostname for remote clients. |
| `profile` | `false` | Emit one aggregate database timing report to stderr when the worker drains on graceful shutdown. Accepts only `true` or `false`. No event values or credentials are included. Intended for local performance diagnosis. |

Each completed write batch emits one timestamped `INFO batch_saved` line, without
requiring profiling or a shutdown. For example, with the timestamp omitted:

```text
INFO batch_saved backend="duckdb" requests=100 received=100 inserted=98 duplicates=2 bytes=243200 queue_ms=10 duration_ms=4
```

`received` counts validated events admitted to this batch, including duplicates;
requests rejected before entering the writer are not included. `inserted` counts
events saved after a successful local transaction commit or S3 publication.
Local storage skips duplicates; S3 retains every delivery and reports zero
duplicates. `bytes` measures prepared buffers, not HTTP body size. `queue_ms` is
the oldest request's wait before writing, including batch collection time;
`duration_ms` measures the write operation. Both use whole milliseconds.

A failed batch emits one `ERROR batch_failed` line with counts and a fixed
`stage`, without claiming any events were saved. S3 failures can leave partial
uploads as described in [storage](storage.md). Logs contain no event values,
project IDs, SQL, request headers, or credentials. Dependency logs are disabled,
including at `debug` and `trace`. Existing startup, retention, administration,
and optional shutdown-profile output is independent of `logging.level`.

### Batching and overload

Concurrent writes share batches up to the event, byte or delay limit. Reads can
flush a pending batch early. A request larger than either batch limit returns
413. Increase both limits when accepting larger events.

With S3 buffering enabled, `storage.flush_interval_seconds` replaces the batch
collection delay. Reads do not flush buffered events or reset the deadline.
The event, byte and queue limits still apply; full batches upload early.

Ingestion rate limits return 429 with `Retry-After: 1`. Full request or write-buffer budgets
return 503. The byte budget covers prepared writes, not total process memory.
Batch delay excludes time waiting for storage, so it is not a response-time limit.
Graceful shutdown finishes admitted writes. See [storage guarantees](storage.md)
for duplicate and failure behavior.

### Read and export limits

Override these settings in the `[rate_limits]` section of `config.toml` and restart
the analytics server. They apply to the server, including requests through the optional
MCP companion. They have no environment-variable overrides.

| TOML field | Default | Meaning |
| --- | --- | --- |
| `rate_limits.read_per_minute` | `120` | Average analytics reads per minute per project. Range 1..1000000. |
| `rate_limits.read_burst` | `20` | Maximum burst of analytics reads per project. Range 1..1000000. |
| `rate_limits.read_concurrent_per_project` | `2` | Maximum queued or running analytics reads per project. Range 1..256. |
| `rate_limits.read_concurrent` | `8` | Maximum queued or running analytics reads across the server. Range 1..256. |
| `rate_limits.export_links_per_minute` | `10` | Average export links created per minute per project, with the same burst capacity. Range 1..1000000. |
| `rate_limits.export_downloads_per_minute` | `2` | Average CSV download starts per minute per project, with the same burst capacity. Range 1..1000000. |
| `rate_limits.export_concurrent_per_project` | `1` | Maximum active CSV generations per project. Range 1..64. |
| `rate_limits.export_concurrent` | `4` | Maximum active CSV generations across the server. Range 1..64. |

Per-project concurrency cannot exceed the corresponding server setting. Rates
refill continuously using governor; for example, 120 reads/minute replenishes
one request every half-second, up to the configured read burst.

Each project shares its budgets across HTTP, MCP, private keys and export links.
Key creation, rotation and live project settings updates do not reset them.
Budgets live in memory and reset when the analytics server restarts. Ingestion uses its
separate limits. Health checks, MCP discovery/documentation and export HEAD
requests do not run database queries and do not consume the read/download budgets.

Excess requests return 429 immediately, without waiting for a slot or starting a
query. `Retry-After` gives the rounded-up time until the next rate allowance;
concurrency rejections use one second because the finish time is unknown.
MCP exposes the delay as `error.retry_after_seconds`. A cancelled query holds its
concurrency permits until its queued or running database job finishes. Accepted
exports still include all matching rows, without an export row cap.

Larger local checkpoint thresholds allow a larger WAL and more crash recovery
work. Measure with your traffic before changing performance settings.

### Separate reader

`experimental.read_worker = true` allows local analytics queries to run alongside
writes. It is off by default and unavailable with S3. It accepts one running
query and up to 64 waiting queries; further reads return 503.

Each response sees committed data when its query begins. Await an ingestion
response before reading if the result must include it. Reads still share CPU,
memory and disk with writes. `profile = true` reports reader and writer timings
on graceful shutdown.
