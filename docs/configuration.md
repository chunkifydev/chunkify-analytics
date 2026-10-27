# Configuration

Copy [collector.toml.example](../collector.toml.example) to `collector.toml`.
The collector loads it from the working directory, or use `--config`:

```sh
chunkify-analytics --config /etc/chunkify/collector.toml config check
chunkify-analytics --config /etc/chunkify/collector.toml serve
```

Relative TOML paths resolve beside the file. CLI and environment paths resolve
from the working directory. `config check` validates without creating storage
or contacting S3. Unknown fields and invalid values fail startup. Restart the
collector after changing installation settings.

## Environment overrides

TOML holds application settings. These deployment variables can override it:

| Variable | TOML field | Use |
| --- | --- | --- |
| `COLLECTOR_DATA_DIR` | `data_dir` | Container state directory. |
| `COLLECTOR_BIND` | `server.bind` | HTTP listening address and port, for example `0.0.0.0:4174`. |
| `COLLECTOR_S3_URL`, `COLLECTOR_S3_ENDPOINT`, `COLLECTOR_S3_REGION` | Corresponding `storage` fields | S3 deployment connection. |
| `COLLECTOR_S3_ACCESS_KEY_ID`, `COLLECTOR_S3_SECRET_ACCESS_KEY`, `COLLECTOR_S3_SESSION_TOKEN` | Corresponding `storage` fields | Inject credentials without putting them in TOML. |
| `COLLECTOR_S3_URL_STYLE`, `COLLECTOR_S3_USE_SSL` | Corresponding `storage` fields | Provider addressing and TLS. |
| `CHUNKIFY_MCP_COLLECTOR_URL`, `CHUNKIFY_MCP_SECRET_KEY` | `mcp.collector_url`, `mcp.secret_key` | Optional stdio adapter connection and secret. |

The collector also loads `.env` from the working directory. Use `--env-file` to
select another file. Precedence is CLI flags, process environment, environment
file, TOML, then defaults. Only the variables above configure the application.
AWS credential providers also read standard AWS credential variables.

The Docker image sets `COLLECTOR_DATA_DIR=/data` and
`COLLECTOR_BIND=0.0.0.0:4174`. These override TOML; change them with container
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

## Application settings

### Installation and data policy

| TOML field | Default | Meaning |
| --- | --- | --- |
| `data_dir` | `./data` | Directory for keys, locks and temporary files. Also holds the default local database and WAL. Environment paths are relative to the working directory. |
| `server.bind` | `127.0.0.1:4174` | Listening IP address and port. Use `0.0.0.0:4174` behind an ingress when needed. |
| `server.allowed_origins` | Empty | Shared website origins, without paths or trailing slashes. Each project can allow additional origins. Server requests without Origin still require the public key. |
| `retention.seconds` | `2592000` | Default retention by server receipt time, in seconds. Default is 30 days. Projects may override it; `0` disables expiration for inheriting projects. Must be a non-negative whole number no larger than `9223372036854775`. |
| `retention.interval_seconds` | `3600` | Seconds between completed retention sweeps while serving. Whole number from 1 to 86400. The timer stays active for live project changes; disabled policies delete nothing. |
| `server.store_ip` | `true` | Store collector-derived request IP context. Accepts only `true` or `false`. Disabling applies to new events and does not erase existing data or sanitize arbitrary fields in the original payload. |
| `server.trusted_proxies` | Empty | Array of literal IPv4/IPv6 addresses or CIDRs allowed to supply `X-Forwarded-For`. Empty trusts no proxies. No hostnames, ports, or wildcards. Use IPv4 CIDRs for IPv4-mapped addresses. |

### Admission limits

| TOML field | Default | Meaning |
| --- | --- | --- |
| `server.max_body_bytes` | `65536` | Maximum request body size in bytes. OTLP gzip requests must fit this limit both before and after decompression. Must be positive. |
| `server.max_otlp_records` | `100` | Maximum log records per OTLP request, across all resource and scope groups, including duplicates. Must be positive. |
| `server.ingest_per_second` | `100` | Global ingestion token bucket. Capacity and refill rate both use this value. Must be positive. |
| `batch.queue_max_bytes` | `16777216` | Maximum bytes held by admitted prepared write buffers, including the active batch. Must be at least the batch byte limit, at most 268435456. |
| `batch.queue_max_requests` | `256` | Database channel capacity, shared by writes, reads and maintenance. Range 1..4096. The byte budget also applies to writes. |

### Advanced performance settings

| TOML field | Default | Meaning |
| --- | --- | --- |
| `batch.max_events` | `100` | Maximum records per write batch, including duplicates. Range 1..10000. A whole request must fit. |
| `batch.max_bytes` | `1048576` | Maximum prepared request-buffer bytes per batch. Range 1..67108864. See write batching below. |
| `batch.delay_ms` | `10` | Maximum time the worker spends collecting a batch. Range 0..1000 ms. `0` gives each request its own batch. |
| `duckdb.threads` | Automatic | Optional DuckDB execution thread count, from 1..256. Applies to the shared database used by writes and reads. HTTP runs separately. |
| `duckdb.checkpoint_threshold_mib` | `16` | DuckDB's automatic checkpoint threshold, in MiB. Integer from 1..1024. Local storage only. Larger values allow a larger write-ahead log between checkpoints; see below. |

### Diagnostics

| TOML field | Default | Meaning |
| --- | --- | --- |
| `batch.insert_mode` | `bulk` | `bulk` uses an appender and one event INSERT per transaction. `row` uses cached INSERT statements per event for diagnostic comparisons. Both share the same local batching and durability rules. S3 always uses an appender and Parquet output. |
| `mcp.enabled` | `true` | Serve the authenticated `/mcp` endpoint in the collector process. |
| `mcp.allowed_hosts` | `localhost,127.0.0.1,[::1]` | Allowed HTTP hostnames or host:port authorities for MCP. Add the ingress hostname for remote clients. |
| `profile` | `false` | Emit one aggregate database timing report to stderr when the worker drains on graceful shutdown. Accepts only `true` or `false`. No event values or credentials are included. Intended for local performance diagnosis. |

### Batching and overload

Concurrent writes share batches up to the event, byte or delay limit. Reads can
flush a pending batch early. An OTLP request is never split; a request larger
than either batch limit returns 413. Increase both limits when accepting larger
exports.

Rate limits return 429 with `Retry-After: 1`. Full request or write-buffer budgets
return 503. The byte budget covers prepared writes, not total process memory.
Batch delay excludes time waiting for storage, so it is not a response-time limit.
Graceful shutdown finishes admitted writes. See [storage guarantees](storage.md)
for duplicate and failure behavior.

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
