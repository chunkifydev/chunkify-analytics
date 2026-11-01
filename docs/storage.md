# Storage

Choose `local` or `s3` in the `[storage]` section of `config.toml`, or set
`CHUNKIFY_STORAGE_BACKEND` in the environment.

## Local

```toml
data_dir = "./data"

[storage]
backend = "local"
path = "./data/analytics.duckdb"
```

`path` is optional; it defaults to `analytics.duckdb` inside `data_dir`.
Relative paths resolve beside the TOML file. Keep the database, its WAL and
`data_dir/keys.json` on persistent storage. Run one analytics server per database.

Repeated event IDs keep their first payload and context. Successful requests
are committed to the database. Expiration frees space for reuse, but the database
file may keep its size.

## S3 or Cloudflare R2

Create a private bucket, then configure its URL and connection. For R2:

```toml
[storage]
backend = "s3"
url = "s3://my-bucket/analytics/"
endpoint = "ACCOUNT_ID.r2.cloudflarestorage.com"
region = "auto"
access_key_id = "YOUR_ACCESS_KEY_ID"
secret_access_key = "YOUR_SECRET_ACCESS_KEY"
```

Use an endpoint hostname with an optional port, without a scheme or path.
HTTPS and path addressing are the defaults. Set `use_ssl = false` for HTTP or
`url_style = "vhost"` for bucket subdomain addressing. Temporary credentials can
include `session_token`. See [environment overrides](configuration.md#environment-overrides)
to supply connection settings and secrets through your deployment.

For AWS S3, omit `endpoint` and set the bucket's region through `storage.region`
or `CHUNKIFY_S3_REGION`. Omit access and secret keys to use AWS credentials,
including an ECS task role.

Credentials need bucket list access and object read, write and delete access
under the analytics prefix. Bucket list access lets S3 return 404 for a registry
that does not exist yet. The analytics server uses delete access for expiration.
Run `init` once and save the keys returned in its JSON output, then run `serve`.
Events, projects and key hashes persist in S3. `<prefix>keys.json` is the
project registry, not a backup; keep it private and include it in backups.
No local `s3-keys.json` cache is created.

### Containers

A replacement container can start with an empty local data directory. Give it
the same configuration and run `serve`; do not run `init` again. `data_dir` must
be writable, but does not need a persistent volume. Startup rebuilds
`query-cache.duckdb` from retained S3 event objects before accepting requests.
Reserve disk for this index, which contains all retained event rows, plus up to
1 GiB of temporary query files. Rebuilding time and disk requirements depend on
the retained dataset. First startup needs access to DuckDB's extension repository.

Reports use the local index without listing or downloading S3 objects. An upload
updates the index only after S3 stores the object. Index update failures make
reads and health checks unavailable until restart rebuilds the index. S3 remains
the durable source; deleting the local index does not delete events.

Run one analytics server per prefix. Stop it before starting a replacement, and run
administration commands inside the active container.

### Events and expiration

Events are compressed Parquet files, grouped by project and UTC receipt date:

```text
analytics/events/project_id=live/received_date=2026-10-27/batch-<id>.parquet
```

S3 stores every delivery. Repeated event IDs remain separate rows. By default,
success means all files for the batch finished uploading. A failed batch can
leave some files stored; retrying the request can repeat events. Requests still
waiting in memory do not survive a crash.

### Buffered S3 writes

To collect up to one minute of events before uploading, add this field to your
existing `[storage]` section and restart the server:

```toml
flush_interval_seconds = 60
```

The default is `0`, which waits for storage before responding. A positive value
from 1 to 3600 enables memory buffering. Requests keep the existing HTTP 201 with
`{"accepted":true,"duplicate":false}` after entering the bounded buffer, before
S3 storage completes. Buffered events can be lost if the process crashes or is
killed. With `60`, the normal unpersisted window is up to one minute, plus upload
time; storage failures can extend it.

The first request starts the timer. Later requests and reads do not reset it.
The writer uploads sooner when `[batch].max_events` or `max_bytes` fills, and
on graceful shutdown. Each upload creates one file per project and UTC receipt
date. Empty intervals create no files. Buffered S3 defaults to 10 MiB of prepared
buffers and 10,000 events per batch, with a 32 MiB queue budget and up to 4096
outstanding requests. These are memory sizes, not compressed Parquet file sizes.
Explicit `[batch]` settings override these defaults, so remove any old 100-event
or 1 MiB overrides when enabling buffering. Without buffering, the smaller local
and S3 defaults still apply.

Tune these fields in `config.toml` as traffic changes. For example:

```toml
[batch]
max_bytes = 10485760
max_events = 10000
queue_max_bytes = 33554432
queue_max_requests = 4096
```

Analytics reads and exports show uploaded events only and never force an early
upload. Failed uploads remain in memory and retry after the configured interval,
using the same object names and event ordering to avoid extra files on retry.
Queue budgets remain held until successful upload; full budgets reject new
requests with 503. A failed final upload makes graceful shutdown exit with an
error. There is no durable queue, so preserve the process during an S3 outage.

### Retention and file counts

Expiration uses server receipt time. It defaults to 30 days, and projects can
override it. Set `seconds = 0` under `[retention]` to keep events indefinitely. Queries
hide expired rows immediately; cleanup removes a file after all its rows expire.
There is no manual event deletion command.

S3 buffering and larger batch limits can reduce small files. Without buffering,
a longer `[batch].delay_ms` can group more concurrent requests but also increases
response latency. Use expiration to bound history; queries cost more as retained
file counts grow.

Keep bucket lifecycle rules from removing active data or `keys.json`. With bucket
versioning, configure expiry of old versions too. Changing the backend or prefix
does not copy existing data.
