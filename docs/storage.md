# Storage backends

Select `local` or `s3` in a flat `[storage]` section of `collector.toml`.
Both use DuckDB for analytics. There is no separate database server or catalog.
Storage selection applies to the whole installation.

## Local storage

```toml
data_dir = "./data"

[storage]
backend = "local"
path = "./data/analytics.duckdb"
```

`path` is optional and defaults to `analytics.duckdb` inside `data_dir`. Relative
TOML paths resolve beside the configuration file. The collector creates missing
parent directories. Keep the database, its WAL and `data_dir/keys.json` on durable
storage. Run one collector per database and data directory.

Local storage enforces one event per `(project_id, event_id)`. The first delivery
keeps its payload, context and OTLP envelope. Concurrent requests can share a
transaction; the collector acknowledges them after commit. A failed transaction
rolls back that batch. Expiration frees database space for reuse but does not
necessarily reduce the database file's size.

## S3 and Cloudflare R2

Create a private bucket first. For R2:

```toml
data_dir = "./data"

[storage]
backend = "s3"
url = "s3://my-bucket/analytics/"
endpoint = "ACCOUNT_ID.r2.cloudflarestorage.com"
region = "auto"
access_key_id = "YOUR_ACCESS_KEY_ID"
secret_access_key = "YOUR_SECRET_ACCESS_KEY"
url_style = "path"
use_ssl = true

[retention]
seconds = 2592000
```

All storage fields stay in `[storage]`. Use an endpoint hostname with an optional
port, without `https://` or a path. `use_ssl` selects HTTPS or HTTP. Path addressing
is the default; services that need bucket subdomains can use `url_style = "vhost"`.
`url` selects the bucket and installation prefix, without wildcards. Give each
installation a different prefix. A bucket's root is also supported.

The S3 credentials need list access for this prefix and object read, write and
delete access. Deletion is used only for expiration. For temporary credentials,
add `session_token`. Keep configuration containing credentials private, or use
the environment overrides in the [configuration reference](configuration.md#storage).

For AWS S3, omit `endpoint`, use the bucket's region and omit the explicit access
and secret keys to use the default AWS credential chain, including an ECS task
role. DuckDB enables automatic credential refresh for long-running tasks, using
the [AWS extension's credential provider](https://duckdb.org/docs/current/core_extensions/aws#auto-refresh).
Both the object client and DuckDB need access to those credentials. The
collector does not create buckets or configure bucket lifecycle rules.

Initialize once, then start with the same configuration:

```sh
chunkify-analytics --config collector.toml config check
chunkify-analytics --config collector.toml init
chunkify-analytics --config collector.toml serve
```

`config check` validates settings without contacting object storage. `init`
saves installation credentials and project settings to `<prefix>keys.json`.
Keep this object private. Each startup loads it from S3 and caches it locally as
`data_dir/s3-keys.json`. Administration changes save to S3 before taking effect.
Run live administration inside the running task, using its configuration and
local data directory. An offline command must run while that task is stopped.

### Persistence and ECS Fargate

S3 is the authoritative store for events and project credentials. The local data
directory holds an optional credential cache, extension files, locks, sockets and
temporary query data. It can be empty when replacing a task. Mount the same TOML
configuration or supply the same environment settings, then run `serve` without
running `init` again.

Run exactly one writer task per prefix. There is no distributed lock. Stop the
old task before starting its replacement; configure ECS deployments to avoid an
overlap. Do not run administration from a second task while the writer is active.
Reads, writes and expiration share one worker. The optional separate read worker
is available only for local storage.

S3 mode uses an in-memory DuckDB database. Queries may spill to
`data_dir/temporary`, capped at 1 GiB; a query that needs more space fails.
The writable local directory and enough memory are still required. On first
startup DuckDB downloads its signed `httpfs` extension, and its `aws` extension
when using the credential chain. Allow network access to the DuckDB extension
repository, or prepopulate `data_dir/extensions` for the matching version and
platform. Cached extensions survive only as long as that directory does.

### Parquet layout and writes

DuckDB writes each batch directly to S3 as compressed Parquet. There is no local
Parquet archive, durable ingestion queue, manifest or compaction process.
A batch can produce one object per project and UTC receipt date:

```text
analytics/keys.json
analytics/events/project_id=default/received_date=2026-10-27/batch-<receipt>-<count>-<uuid>.parquet
analytics/events/project_id=live/received_date=2026-10-27/batch-<receipt>-<count>-<uuid>.parquet
```

The collector lists the authenticated project's files and reads them with
DuckDB. It keeps original payload and context on each row. It does not deduplicate
S3 event IDs when writing or querying. Sending the same event again stores another
row and returns success. Retries after a lost response can therefore store duplicates.

Success means every object for the batch finished uploading. Each object upload
is independent. A failed multi-object batch can leave some uploaded objects even
though its requests receive an error. Retrying can repeat those events. Pending
in-memory writes do not survive a task crash. Local storage retains its
transaction and uniqueness guarantees.

Batch settings also apply to S3. The defaults flush after at most 10 ms or 100
records, which can create many small objects. For sustained S3 ingestion, consider
`[batch].delay_ms = 1000` and `max_events = 10000`, then measure latency and file
counts with your traffic. Reads interrupt collection. There is no background
rewriting of small files; listing and querying become more expensive as the number
of retained objects grows. Use expiration to bound retained history.

### Expiration

`[retention].seconds` is the installation default, and projects can override it.
`0` keeps events indefinitely. Expiration uses the server's receipt time, not the
event's client timestamp. There is no per-viewer or manual event deletion command.

S3 queries exclude expired rows immediately. Startup and scheduled sweeps remove
an entire Parquet object once its newest row has expired. A file can therefore
remain until its last row expires and the next successful sweep runs. Sweeps
leave `keys.json` and unrecognized files untouched. They never rewrite partial
files. Avoid separate bucket lifecycle rules that delete active data or keys.
If bucket versioning is enabled, provider rules must also expire old object
versions to reclaim their storage.

Keep a backup of the complete prefix, including `keys.json`, and save deployment
configuration separately. Switching the configured backend or prefix does not
copy events or credentials between stores.
