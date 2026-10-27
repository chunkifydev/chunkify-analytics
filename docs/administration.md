# Administration

## Projects and named keys

A project is the boundary for ownership and access: use separate projects for
customers, or for staging and production. Each has a stable `id` and an editable
`name`. Names live once in `keys.json`; events reference the stable `project_id`.
With S3, this registry lives at the configured bucket prefix and has a local
`s3-keys.json` cache. Use the same storage configuration for every command.
There is no additional tenant/account layer in this version.

Stop the collector, then run these commands from the directory containing
`collector.toml`. Use `--config /path/to/collector.toml` when running elsewhere:

```sh
chunkify-analytics project create live --name "Production website"
chunkify-analytics project create staging --name "Staging website"
chunkify-analytics project list
chunkify-analytics project rename live "Customer A Production"
chunkify-analytics key create live ingest "Website"
chunkify-analytics key create live read "Dashboard backend"
# Use the safe key ID from project list or key create:
chunkify-analytics key rotate live KEY_ID
chunkify-analytics key revoke live KEY_ID
```

`project create <id> [--name <name>]` returns `ingest_key` and `read_token`. The name defaults
to the ID when omitted. `key create` and `key rotate` return `key_id` and `secret`.
Save these secrets when the command returns: newly created project credentials
are stored as SHA-256 hashes and cannot be retrieved later. `project list` returns
project IDs, names, settings, effective retention in `effective_retention_seconds`,
and key IDs/names/permissions, without secrets or hashes.

On Linux and macOS, `project list`, `project retention`, `key create`, `key rotate`,
`key revoke` and `key rotate-primary` also work while the collector is running. Use
the same configuration and data directory, and run the CLI as the collector's operating-system
user. Successful live changes return `restart_required: false` and take effect
before the command returns. Already authenticated requests may finish. Project
creation, renaming and full `project configure` changes still
require stopping the collector. Use `project retention` for a live retention-only
change. On other operating systems, administration remains offline.

The CLI connects through a private `admin.sock` in the data directory, with mode
`0600` and a peer-user check. For paths that exceed the Unix socket length limit,
it uses a randomly named private directory under `/tmp`. A private
`admin.socket-id` file in the data directory lets the CLI find it automatically.
`/tmp` must be writable in that case. No public HTTP administration route or admin
token is added. Keep the data directory private and do not edit the key registry while the
collector runs. A failed authoritative write leaves active settings unchanged.
S3 changes save remotely before updating the local cache; a cache failure returns
a warning, and the remote change remains active. For local storage, if the file was
replaced but its directory could not be synced, the command returns a `warning`;
the new configuration is active, but its survival after a system crash is uncertain.
If a command loses its connection or times out, check `project list` before
retrying. A named secret whose response was lost cannot be recovered; rotate or
revoke that key.

For Docker, run the same commands inside the running container:

```sh
docker exec chunkify-analytics chunkify-analytics project list
docker exec chunkify-analytics chunkify-analytics key create live ingest "Website v2"
docker exec chunkify-analytics chunkify-analytics key revoke live OLD_KEY_ID
```

For a transition without interrupting delivery, create a second key, deploy it
to clients, then revoke the old key once those clients have switched. `key rotate`
replaces the secret immediately and retains its key ID; it has no overlap period.
These commands do not reset project quotas or change stored events.

The initial `default` project's plaintext `ingest_key` and `read_token` exports
remain in the private key registry for the init/key rotate-primary workflow.
Once a `projects` registry is saved, its hashes are authoritative; a revoked
primary credential is not re-enabled by the legacy export. Rotating an existing
default primary key updates that export too. Back up the entire file.

The browser sends only `X-Chunkify-Ingest-Key`. An in-memory hash lookup resolves
its project and safe key ID; no per-request database lookup is needed. The project
ID is not encoded in the token. Native and OTLP records receive `project_id` and
`ingest_key_id` as trusted response metadata. Client-supplied properties remain in
the original event but cannot redirect storage. Multiple keys for one project
share quotas and, with local storage, deduplication. Rotation keeps the key ID
and historical events.
An ingest key identifies the destination/integration, not a verified sender or
viewer. `viewer_id` remains an optional client-supplied identifier.

A read token permits access to exactly one project on all read endpoints,
including ingestion status. Cursors are bound to that project; a cursor never grants
access. The same event/view/viewer IDs can occur independently in other projects.
There is no cross-project query token or account membership API yet.

### Project limits and origins

Defaults divide the installation's ingestion rate, in-flight write slots, memory
buffer budget evenly across the configured projects
(including `default`). Rate and slot defaults have a minimum of one. Each project
inherits `COLLECTOR_ALLOWED_ORIGINS`. Keys within a project share these limits.
The installation caps still apply, so these are ceilings, not throughput or latency
guarantees. Read queries, writer scheduling and retained database storage are
shared. Each project inherits the installation retention default unless it has
an override. See [retention](#retention) for live changes.

To override one project's limits, save a JSON file and run
`project configure live /path/to/settings.json`, then restart:

```json
{
  "allowed_origins": ["https://player.example"],
  "ingest_per_second": 100,
  "queue_bytes": 4194304,
  "retention_seconds": 604800
}
```

The command replaces that project's overrides. Omitted/null values inherit the
default; `retention_seconds: 0` disables expiration for that project, and
`allowed_origins: []` disables browser origins for that project. Queue
limits accept 4096–268435456 bytes and are capped by the installation limit.
These limits bound in-memory write buffers, not retained storage. Successful
requests have already completed storage; incomplete requests are not recovered after restart.

A full queue returns 503; exceeding a project rate returns 429; a single request
that cannot fit its byte budget returns 413. Origin checks on POST use the
resolved project's policy. Unauthenticated preflight uses the union of allowed
origins, since browsers do not send the ingest key value during preflight.



All commands use the configured `data_dir`. Administration requires local filesystem
access to that directory; there are no remote admin endpoints. Key commands,
`project list` and `project retention` work live on Linux and macOS. Other
administration requires a stopped collector.
Invalid commands or configuration return a nonzero exit status.

### Retention

By default, the collector removes events received more than 30 days ago. Each
project can override that duration. The collector runs a sweep before accepting
requests at startup and then once an hour. Expired
events in local storage can remain until the next successful sweep. S3 reads
hide expired rows immediately; cleanup removes an entire Parquet file only once
all its rows expire. See [S3 expiration](storage.md#expiration). A failed scheduled sweep logs
an error and retries at the next interval; a failed startup sweep prevents startup.

Retention uses the stored server `received_at`, independently of the client's
`occurred_at`. An old client timestamp does not cause immediate removal, and a
future client timestamp does not extend storage. Local expiration removes the
event, its OTLP log/context, and request context in one transaction.
Summaries use only retained events, so expiration also removes their contributions.

`[retention].seconds` in TOML sets the installation default, initially 2592000
seconds, or 30 days. `0` disables expiration for projects inheriting that default.
Changing the installation default or sweep interval requires a restart.

To change one project without restarting on Linux or macOS, run the CLI as the
collector user with the same configuration and data directory:

```sh
# Keep the live project's events for seven days:
chunkify-analytics project retention live 604800
# Keep its events indefinitely:
chunkify-analytics project retention live 0
# Restore the installation default:
chunkify-analytics project retention live inherit
chunkify-analytics project list
```

Docker uses the same command inside the running container:

```sh
docker exec chunkify-analytics chunkify-analytics project retention live 604800
```

The command changes only retention, preserving origins, quotas and credentials.
It saves the override in the project configuration and returns `project_id`,
`retention_seconds` (null when inheriting), `effective_retention_seconds` and
`restart_required`. Live success returns `restart_required: false`; offline
changes take effect when the collector starts. Values must be non-negative whole
seconds up to 9223372036854775, or `inherit`. The JSON settings field accepts
`null` or omission for inheritance through the existing offline `project configure`
command. That command replaces all overrides, so retain any settings you need.

Changes apply to existing and new events. Shortening retention makes older records
eligible for the next scheduled cleanup; it does not trigger an immediate sweep.
An in-progress sweep finishes before a live change is confirmed. Sweeps queued
behind other database work read the latest policy when they execute. Extending or
disabling retention preserves remaining events but cannot recover deleted ones.
A positive project override still expires events when the installation default is
`0`. The running server's default determines inheritance for live commands.

The administrator chooses the policy and the collector performs deletion.
Automatic synchronization with Chunkify customer/project policies, historical
summaries are future work tracked in
[issue #381](https://github.com/chunkifydev/chunkify/issues/381). Current dashboard
history is limited to retained events; changing retention does not create an
archive or permanent totals.

In local storage, duplicate delivery does not refresh the event's receipt time.
After expiration, receiving its ID again creates a new stored event. In S3, each
delivery is a separate row with a new receipt time. Expiration is the only event
removal mechanism; there is no viewer deletion command.

### Rotate a credential

With the same configuration and data directory, run one of:

```sh
chunkify-analytics key rotate-primary ingest
chunkify-analytics key rotate-primary read
```

These legacy commands rotate the `default` project’s primary credential.
Each command replaces only the selected credential. It writes a complete new
local key registry through a temporary file and rename, with mode `0600` on Unix.
S3 saves the registry remotely and refreshes the local `s3-keys.json` cache. It
does not print credential values or modify event data.

Read the new value from `keys.json` for local storage or `s3-keys.json` for S3. For an ingest-key rotation, update the
browser adapter's `X-Chunkify-Ingest-Key`. For a read-token rotation, update the
dashboard backend's bearer token. A running collector applies the change
immediately on Linux and macOS; if stopped, start it to use the new key. The old
selected credential is no longer accepted, with no overlap period. Pages still using the
old ingest key will fail to deliver events until they load the new configuration.

### Backup and restore

For S3, stop the collector and back up the configured prefix, including `keys.json`.
Restore the complete prefix before starting a replacement task with the same
configuration. Local caches alone are not an S3 backup. Keep configuration and
credentials private and save them separately.

For local storage, use an offline copy of the complete data directory. Stop the collector gracefully
and wait for it to exit, then keep it stopped until the copy finishes. Shutdown
runs a DuckDB [checkpoint](https://duckdb.org/docs/current/sql/statements/checkpoint)
and closes the database. Include the database, any WAL file, keys, and lock file. If `[storage].path`
points outside `data_dir`, copy that database and its WAL too, and update its
configuration path when restoring.
Keep the backup private because it contains the read token and analytics data.
Save `collector.toml` and any environment overrides separately.

For example, with a backup destination that does not already exist:

```sh
# After stopping the collector and waiting for it to exit:
# Replace ./data if your configuration uses another data directory.
cp -a ./data /srv/backups/analytics-backup
# Restart the collector using its original data directory.
```

To restore, stop the collector and copy the backup into a new directory:

```sh
cp -a /srv/backups/analytics-backup /srv/analytics-restored
chunkify-analytics --data-dir /srv/analytics-restored serve
```

The restore destination must not already exist. Use the same collector version,
or a version that supports upgrading its database schema. Preserve directory
permissions and ensure the account running the collector can write there. Do not
run `init` on restored data. Check `/healthz` and the authenticated event and
summary endpoints before sending traffic to the restored installation.

Restoring also restores the keys and events from the backup date. Changes after
that date are absent. Reapply later key rotations and project
retention changes as needed.
Startup retention still applies, so an older backup can lose expired events
when it starts. To inspect it without expiration, explicitly disable retention
on the restored copy before starting it. Set the installation default to `0` and
use offline `project retention <project> 0` for every project with an explicit
positive override; the installation default alone does not disable overrides.


## Docker administration and backups

Rotate a key inside the running container:

```sh
docker exec chunkify-analytics chunkify-analytics key rotate-primary ingest
```

Use `key rotate-primary read` in the same position for the read token. Read and distribute
the replacement credential as described below. Named key commands also work
through `docker exec`.

Project creation, renaming and full settings changes require stopping the server first.

To back up the named volume, keep the server stopped until the archive is complete.
Choose a new backup filename and keep its contents private:

```sh
docker stop --time 20 chunkify-analytics
umask 077
docker run --rm \
  --mount type=volume,src=chunkify-analytics-data,dst=/data,readonly \
  --entrypoint tar chunkify-analytics:local -C /data -czf - . \
  > analytics-backup.tar.gz
docker start chunkify-analytics
```

Restore to a new, unused named volume:

```sh
docker volume create chunkify-analytics-restored
docker run --rm -i \
  --mount type=volume,src=chunkify-analytics-restored,dst=/data \
  --entrypoint tar chunkify-analytics:local -C /data -xzpf - \
  < analytics-backup.tar.gz
```

Save the host's `collector.toml` alongside the archive. The read-only host file
mount is separate from the named volume and is not included in this backup.

Start the collector using that restored volume and the same TOML mount, without
running `init`. Follow the same version, retention, and credential rules
in [backup and restore](#backup-and-restore).
