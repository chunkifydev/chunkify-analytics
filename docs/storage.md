# Storage

Choose `local` or `s3` in the `[storage]` section of `config.toml`, or set
`COLLECTOR_STORAGE_BACKEND` in the environment.

## Local

```toml
data_dir = "./data"

[storage]
backend = "local"
path = "./data/analytics.duckdb"
```

`path` is optional; it defaults to `analytics.duckdb` inside `data_dir`.
Relative paths resolve beside the TOML file. Keep the database, its WAL and
`data_dir/keys.json` on persistent storage. Run one collector per database.

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

For AWS S3, omit `endpoint` and use the bucket's region. Omit access and secret
keys to use AWS credentials, including an ECS task role.

Credentials need list access to the prefix and object read, write and delete
access. The collector uses delete access for expiration. Run `init` once, then
`serve`. Events, projects and credentials persist in S3; keep `<prefix>keys.json`
private and include it in backups.

### Containers

A replacement container can start with an empty local data directory. Give it
the same configuration and run `serve`; do not run `init` again. `data_dir` must
be writable, but does not need a persistent volume. Large queries can use up to
1 GiB of temporary disk space. First startup needs access to DuckDB's extension
repository.

Run one collector per prefix. Stop it before starting a replacement, and run
administration commands inside the active container.

### Events and expiration

Events are compressed Parquet files, grouped by project and UTC receipt date:

```text
analytics/events/project_id=live/received_date=2026-10-27/batch-<id>.parquet
```

S3 stores every delivery. Repeated event IDs remain separate rows. Success means
all files for the batch finished uploading. A failed batch can leave some files
stored; retrying can repeat events. Requests still waiting in memory do not
survive a crash.

Expiration uses server receipt time. It defaults to 30 days, and projects can
override it. Set `seconds = 0` under `[retention]` to keep events indefinitely. Queries
hide expired rows immediately; cleanup removes a file after all its rows expire.
There is no manual event deletion command.

For busy installations, a longer `[batch].delay_ms` and larger `max_events` can
reduce small files. Both also affect write latency. Use expiration to bound
history; queries cost more as retained file counts grow.

Keep bucket lifecycle rules from removing active data or `keys.json`. With bucket
versioning, configure expiry of old versions too. Changing the backend or prefix
does not copy existing data.
