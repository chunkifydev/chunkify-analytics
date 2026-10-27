# Administration

Run commands with the collector's configuration and data directory:

```sh
chunkify-analytics --config /path/to/collector.toml project list
```

In Docker, run them inside the active container:

```sh
docker exec chunkify-analytics chunkify-analytics project list
```

On Linux and macOS, key management and retention changes work while the collector
runs. Stop it before creating or renaming projects, or replacing project settings.
Other platforms require stopping the collector for all administration.

## Projects and named keys

Each project has its own events, credentials and limits. Use separate projects
for customers, websites or environments.

```sh
chunkify-analytics project create live --name "Production website"
chunkify-analytics project rename live "Customer website"
chunkify-analytics key create live ingest "Website"
chunkify-analytics key create live read "Dashboard"
chunkify-analytics key rotate live KEY_ID
chunkify-analytics key revoke live KEY_ID
```

Creating a project returns `ingest_key` and `read_token`. Creating or rotating a
named key returns its `key_id` and `secret`. Save the secret when returned; named
key secrets cannot be retrieved later. `project list` shows settings and key IDs
without secrets.

Ingest keys send events; read tokens read analytics and MCP. A token belongs to
one project. Multiple keys for that project share its limits and events.
Rotation keeps the key ID and immediately replaces its secret. For an update
without interruption, create a second key, switch clients, then revoke the old key.

Changes take effect before a successful live command returns. Requests already
authenticated may finish. If a command times out, check `project list` before
retrying. If you lose a new key's secret, rotate or revoke it.

The initial default credentials remain available in the private key registry:
`data_dir/keys.json` for local storage or `data_dir/s3-keys.json` for S3. S3 keeps
the registry in the bucket as well. Do not edit it while the collector runs.

To replace a default primary credential:

```sh
chunkify-analytics key rotate-primary ingest
chunkify-analytics key rotate-primary read
```

Read the replacement from the key registry, then update clients. These commands
do not print the secret or change stored events.

## Project limits and origins

Projects inherit installation origins and retention. Rate, in-flight request
and write-buffer allowances divide the installation limits across projects.
To replace a project's overrides, save a JSON file:

```json
{
  "allowed_origins": ["https://player.example.com"],
  "ingest_per_second": 100,
  "queue_bytes": 4194304,
  "retention_seconds": 604800
}
```

Then stop the collector and apply it:

```sh
chunkify-analytics project configure live /path/to/settings.json
```

Omitted or null values inherit defaults. `allowed_origins: []` denies browser
origins; `retention_seconds: 0` disables expiration. `queue_bytes` accepts
4096 to 268435456 bytes, capped by the installation limit. It bounds pending writes,
not retained storage. Installation limits still apply across projects.

Excess requests return 429, full write buffers return 503, and a request larger
than its buffer budget returns 413. Origin checks use the ingest key's project.

## Retention

Expiration uses server receipt time, not the client's event timestamp. The
default is 30 days. Cleanup runs at startup and hourly while serving.
S3 reads hide expired rows before cleanup; local reads can include them until
the next sweep. Expired events stop contributing to reports.

Change a project's duration without replacing its other settings:

```sh
# Seven days:
chunkify-analytics project retention live 604800
# Keep indefinitely:
chunkify-analytics project retention live 0
# Use the installation default:
chunkify-analytics project retention live inherit
```

Changes apply to existing and new events. Shortening retention makes older data
eligible for cleanup; extending it cannot recover removed data. Changing the
installation default or cleanup interval in TOML requires a restart.
A failed scheduled cleanup retries at the next interval; a failed startup cleanup
prevents startup. See [storage](storage.md) for S3 file expiration.

## Backup and restore

Stop the collector before taking or restoring a backup. Save the TOML configuration
and deployment credentials separately. Keep backups private.

For local storage, copy the complete data directory. Include a custom
`[storage].path` database and its WAL if they live elsewhere:

```sh
cp -a ./data /srv/backups/analytics-backup
```

Restore to a new directory, adjust any custom database path, and start without
running `init`:

```sh
cp -a /srv/backups/analytics-backup /srv/analytics-restored
chunkify-analytics --data-dir /srv/analytics-restored serve
```

For S3, back up and restore the complete prefix, including `keys.json`. A local
credential cache alone is not a backup. Start the replacement with the same S3
configuration.

Use the same collector version or one supporting the saved database schema.
Check `/healthz`, events and summaries before sending traffic. A restore also
restores old credentials and retention settings; reapply later changes as needed.
Startup expiration still applies to restored data.

### Docker volumes

Back up a stopped local installation:

```sh
docker stop --time 20 chunkify-analytics
umask 077
docker run --rm \
  --mount type=volume,src=chunkify-analytics-data,dst=/data,readonly \
  --entrypoint tar chunkify-analytics:local -C /data -czf - . \
  > analytics-backup.tar.gz
docker start chunkify-analytics
```

Restore into a new volume:

```sh
docker volume create chunkify-analytics-restored
docker run --rm -i \
  --mount type=volume,src=chunkify-analytics-restored,dst=/data \
  --entrypoint tar chunkify-analytics:local -C /data -xzpf - \
  < analytics-backup.tar.gz
```

Start with the restored volume and the saved TOML file, without running `init`.
A separately mounted TOML file and databases outside `/data` are not included
in this volume backup.
