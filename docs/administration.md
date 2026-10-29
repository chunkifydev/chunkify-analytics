# Administration

Run commands with the analytics server's configuration and data directory:

```sh
chunkify-analytics --config /path/to/config.toml project list
```

In Docker, run them inside the active container:

```sh
docker exec chunkify-analytics chunkify-analytics project list
```

On Linux and macOS, project creation, key management and retention changes work
while the analytics server runs. Stop it before renaming projects or replacing
project settings.
Other platforms require stopping the analytics server for all administration.

## Projects and named keys

Each project has its own events, credentials and limits. Use separate projects
for customers, websites or environments.

```sh
chunkify-analytics project create "Production website"
chunkify-analytics project rename PROJECT_ID "Customer website"
chunkify-analytics key create PROJECT_ID public "Website"
chunkify-analytics key create PROJECT_ID secret "Dashboard" --read-only
chunkify-analytics key rotate PROJECT_ID KEY_ID
chunkify-analytics key revoke PROJECT_ID KEY_ID
```

The name is a display label. Creating a project generates a UUID and returns
`project_id`, `public_key` and `secret_key`. Use `project_id` in later commands. Creating or rotating a
named key returns its `key_id`, `permission` and `key`. Save the secret when returned; named
key secrets cannot be retrieved later. `project list` shows settings and key IDs
without secrets.

Public keys start with `ck_public_` and send events. Private keys are called
`secret` in the CLI and start with `ck_secret_`. Each key belongs to one project;
multiple keys for that project share its limits and events.

Add `--read-only` when creating a private key for a dashboard or MCP client.
It can query all six analytics endpoints and connect through HTTP or stdio MCP.
It cannot submit events, list or rotate keys, update or delete a project, or
create projects. A valid read-only key receives 403 on project and key management
routes. The flag is rejected for public keys.

Private keys created without the flag keep full project access, including
project and key management. Existing keys remain full-access. Key creation,
rotation and listings include a `read_only` boolean, and rotation preserves it.
Create a new key to choose a different access level.
Rotation keeps the key ID and immediately replaces its secret. For an update
without interruption, create a second key, switch clients, then revoke the old key.

Changes take effect before a successful live command returns. Requests already
authenticated may finish. If a command times out, check `project list` before
retrying. If you lose a new key's secret, rotate or revoke it.

All project credentials, including the default project's keys, are stored only
as hashes. The registry contains project IDs, settings and key hashes in
`data_dir/keys.json` for local storage or `<prefix>keys.json` in S3. S3 mode does
not write a local credential cache. Do not edit the registry while the server runs.
`init` returns the default keys once as JSON; save them in your secret manager.

To replace a default primary credential:

```sh
chunkify-analytics key rotate-primary public
chunkify-analytics key rotate-primary secret
```

Each command returns JSON with the replacement `key`, its `key_id`, permission
and `restart_required`. Save that key and update clients. The secret is returned
only by the command that generates it; stored events are unchanged.

## Project limits and origins

Every project allows the shared TOML origins plus its own additional origins.
Retention inherits the installation default. Default rate, in-flight request
and write-buffer allowances divide the installation limits across projects at
startup. A project created while serving gets a share based on the current
project count; existing projects keep their active limits until restart.
To replace a project's overrides, save a JSON file:

```json
{
  "allowed_origins": ["https://player.example.com"],
  "ingest_per_second": 100,
  "queue_bytes": 4194304,
  "retention_seconds": 604800
}
```

Then stop the analytics server and apply it:

```sh
chunkify-analytics project configure PROJECT_ID /path/to/settings.json
```

Omitted or null values inherit defaults. `allowed_origins: []` clears additional
project origins; shared TOML origins stay allowed. `retention_seconds: 0` disables expiration. `queue_bytes` accepts
4096 to 268435456 bytes, capped by the installation limit. It bounds pending writes,
not retained storage. Installation limits still apply across projects.

Excess requests return 429, full write buffers return 503, and a request larger
than its buffer budget returns 413. Origin checks use the public key's project.

## Create projects through the API

Set `CHUNKIFY_ADMIN_KEY` in the analytics server's environment or private `.env` file to enable
`POST /v1/projects`. Use a separate random secret for your provisioning backend.
An unset or empty value disables this endpoint with a 404 response.

```sh
curl -X POST http://127.0.0.1:4174/v1/projects \
  -H "Authorization: Bearer $CHUNKIFY_ADMIN_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"name":"Customer website","allowed_origins":["https://customer.example"],"retention_seconds":2592000}'
```

The response is 201 with `project_id`, `name`, `public_key`, `secret_key`,
`retention_seconds` and `effective_retention_seconds`.
The ID is generated automatically. Save the keys when returned and store the
project ID alongside your customer record. The project is ready immediately.

`name` is required, with 1 to 128 characters. Names can repeat. `allowed_origins`
is optional and adds to the shared TOML origins. Optional `retention_seconds`
sets the project's receipt-time retention in seconds. Omit it or send `null` to
inherit the installation default; `0` disables expiration. The project and its
retention are saved together and take effect before the response. The response's
`effective_retention_seconds` resolves the inherited default. Supplying an ID is rejected.
Project secret keys cannot create projects. The admin key does not grant access
to project analytics or MCP directly. It can list and rotate keys for any active
project through the recovery routes below. The installation supports up to 1024
active projects.

## Live project API

Use a project's full-access private key as `Authorization: Bearer SECRET_KEY`.
It can manage only that project. Read-only private keys cannot use these routes.

| Endpoint | Use |
| --- | --- |
| `PATCH /v1/project` | Update the name, additional origins or retention without restarting. |
| `DELETE /v1/project` | Delete this project and revoke all its keys. |
| `GET /v1/project/keys` | List key IDs, names and types. |
| `POST /v1/project/keys` | Create a named public or secret key. |
| `POST /v1/project/keys/{key_id}/rotate` | Replace a key and return its new value. |

For example, keep `https://chunkify.dev` in TOML and add a customer's website:

```sh
curl --request PATCH http://127.0.0.1:4174/v1/project \
  -H "Authorization: Bearer $SECRET_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"allowed_origins":["https://customer.example"]}'
```

Both origins then work for this project. Other projects keep their own lists.
Supply `name` to rename the project or `retention_seconds` to change retention.
Omitted fields stay unchanged; send `allowed_origins: []` to clear additions.
Empty requests and null names or origins are rejected. A null
`retention_seconds` restores the installation default. Changes are saved together
before they take effect. The response includes both `retention_seconds` and
`effective_retention_seconds`.

### Create a key through the API

Use the project's full-access secret key to create additional keys:

```sh
curl --request POST http://127.0.0.1:4174/v1/project/keys \
  -H "Authorization: Bearer $SECRET_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"name":"Dashboard","permission":"secret","read_only":true}'
```

Set `permission` to `public` for ingestion or `secret` for analytics access.
Secret keys default to full project access. Set `read_only: true` for dashboard
and MCP clients. Public keys cannot be read-only. Names must be nonblank and
at most 128 UTF-8 bytes.

The 201 response contains `project_id`, `key_id`, `key`, `permission`,
`read_only` and `restart_required: false`. The key works immediately. Save its
value now; later listings expose only metadata. Creating a key does not replace
existing keys. Each project supports 64 active keys, including its initial two;
creating another at the limit returns 409. Invalid requests return 400.

The installation admin key and public keys cannot call this project-scoped
endpoint; read-only secret keys receive 403. The project's full-access key
also authorizes retention changes and the other management routes above.

### Rotate a key

Rotation invalidates the old key immediately. Save the returned `key` value;
key listings never reveal it. Rotating the secret key used by the request means
subsequent calls must use its replacement. Only the selected key is rotated;
rotating a public key leaves secret keys unchanged, and vice versa. If the
replacement is lost, use the independent admin recovery route below.

## Recover a lost project key

Your trusted backend can recover project access using `CHUNKIFY_ADMIN_KEY`.
No current project secret is required. The backend must authenticate the user
and verify their ownership of the requested project before making these calls.
Keep the admin key on the server; it can rotate keys for every project.

| Endpoint | Use |
| --- | --- |
| `GET /v1/projects/{project_id}/keys` | List key IDs, names, types and `read_only`, without secrets. |
| `POST /v1/projects/{project_id}/keys/{key_id}/rotate` | Rotate one existing key and return its replacement. |

Find the key ID, then rotate that key:

```sh
curl "http://127.0.0.1:4174/v1/projects/$PROJECT_ID/keys" \
  -H "Authorization: Bearer $CHUNKIFY_ADMIN_KEY"

curl --request POST \
  "http://127.0.0.1:4174/v1/projects/$PROJECT_ID/keys/$KEY_ID/rotate" \
  -H "Authorization: Bearer $CHUNKIFY_ADMIN_KEY"
```

Save the returned `key` value. The old value stops working immediately. The key
ID, public/secret type and `read_only` setting stay the same; other keys and
stored events are unchanged. A lost rotation response can be recovered by
rotating the same key again with the admin key. Changes take effect immediately
and persist across analytics server restarts.

Unset or empty `CHUNKIFY_ADMIN_KEY` disables these routes with 404. When enabled,
missing or invalid admin credentials, including project keys, receive 401.
Unknown or deleted projects and unknown or revoked keys receive 404. Recovery
does not restore revoked keys or deleted projects. Without an admin key, a
server operator can still use the CLI to rotate a lost key.

## Delete a project

```sh
curl --request DELETE http://127.0.0.1:4174/v1/project \
  -H "Authorization: Bearer $SECRET_KEY"
```

All of the project's public and secret keys stop working. Other projects are
unaffected. Requests already authenticated may finish.

Stored events expire under the project's existing retention policy. Unlimited
retention keeps them indefinitely. The project record remains marked `deleted`
in `project list` to preserve that policy. Deleted projects cannot receive new
keys and do not count toward the active project limit.

## Retention

Expiration uses server receipt time, not the client's event timestamp. The
default is 30 days. Cleanup runs at startup and hourly while serving.
S3 reads hide expired rows before cleanup; local reads can include them until
the next sweep. Expired events stop contributing to reports.

Change a project's duration without replacing its other settings:

```sh
# Seven days:
chunkify-analytics project retention PROJECT_ID 604800
# Keep indefinitely:
chunkify-analytics project retention PROJECT_ID 0
# Use the installation default:
chunkify-analytics project retention PROJECT_ID inherit
```

Changes apply to existing and new events. Shortening retention makes older data
eligible for cleanup; extending it cannot recover removed data. Changing the
installation default or cleanup interval in TOML requires a restart.
A failed scheduled cleanup retries at the next interval; a failed startup cleanup
prevents startup. See [storage](storage.md) for S3 file expiration.

Your backend can change retention through the API with the project's full-access
secret key:

```sh
curl --request PATCH http://127.0.0.1:4174/v1/project \
  -H "Authorization: Bearer $SECRET_KEY" \
  -H 'Content-Type: application/json' \
  --data '{"retention_seconds":604800}'
```

Use `0` for unlimited retention or `null` to inherit the installation default.
Omitting `retention_seconds` leaves it unchanged. The response reports the
configured and effective durations. Values must be whole non-negative seconds
no larger than `9223372036854775`. Read-only, public and installation admin keys
cannot change retention through this endpoint.

## Backup and restore

Stop the analytics server before taking or restoring a backup. Save the TOML configuration
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

Use the same analytics server version or one supporting the saved database schema.
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
