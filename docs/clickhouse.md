# ClickHouse storage

Use ClickHouse storage to run several analytics servers at once. With local or S3
storage, one server owns the data. With ClickHouse, servers keep no data of their
own: events, projects and key hashes live in one ClickHouse database that every
server shares. Add servers behind a load balancer to handle more ingestion and
reads, and replace them without downtime.

The HTTP API, MCP endpoint, CSV exports and dashboard work as with local storage.
The reports use the same cohort rules, ranking, cursors and response fields.

ClickHouse is open source (Apache 2.0) and runs in its own container, so you do
not need a hosted service.

## Run with Docker Compose

[examples/clickhouse](../examples/clickhouse/README.md) starts ClickHouse, the
analytics server and a small gateway that spreads requests over every analytics
replica:

```sh
cd examples/clickhouse
cp .env.example .env            # then set CLICKHOUSE_PASSWORD
docker compose run --rm analytics init
docker compose up -d --scale analytics=3
curl --fail http://127.0.0.1:4174/healthz
```

`init` creates the database and prints the initial `public_key` and
`secret_key` once. Run it once per ClickHouse database, not once per server.

## Configure

Select the backend and point every analytics server at the same database:

```toml
[storage]
backend = "clickhouse"

[clickhouse]
url = "http://clickhouse:8123"
database = "chunkify_analytics"
user = "chunkify"
# password = "..."   # Prefer CHUNKIFY_CLICKHOUSE_PASSWORD.
```

| TOML field | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `clickhouse.url` | `CHUNKIFY_CLICKHOUSE_URL` | `http://127.0.0.1:8123` | ClickHouse HTTP interface. HTTP or HTTPS, without credentials, query or fragment. |
| `clickhouse.database` | `CHUNKIFY_CLICKHOUSE_DATABASE` | `chunkify_analytics` | Database for events and the project registry. Created by `init` if missing. Letters, digits and underscores. |
| `clickhouse.user` | `CHUNKIFY_CLICKHOUSE_USER` | ClickHouse default user | ClickHouse user. |
| `clickhouse.password` | `CHUNKIFY_CLICKHOUSE_PASSWORD` | Empty | ClickHouse password. |
| `clickhouse.async_insert` | — | `true` | Let ClickHouse combine inserts from all servers into fewer parts. Each request still waits until its events are stored. |
| `clickhouse.registry_refresh_seconds` | — | `5` | How often each server checks the shared registry for project and key changes. 1 to 3600. |
| `clickhouse.deduplication_window_seconds` | — | `604800` (7 days) | Repeated event IDs received within this window are reported as duplicates and not stored again. `0` stores every delivery. |
| `clickhouse.node_id` | `CHUNKIFY_CLICKHOUSE_NODE_ID` | Random | 0 to 63. Part of each event's internal order. Set a different value on each server for strict uniqueness. |

The user needs permission to create the database and tables, and to read, insert
and delete rows in them. Each server still needs a writable `data_dir` for its
lock and local administration socket; it does not need a persistent volume.

Other settings, such as rate limits, retention and origins, work as for other
backends. Each server applies its own `server.ingest_per_second` and `[rate_limits]`,
so the total for the installation grows with the number of servers.

## How servers share work

- **Ingestion.** Each server batches the requests it receives and inserts them
  in one request. A response is sent after ClickHouse has stored the events, so
  acknowledged events survive a server crash.
- **Reads.** Each report is one or two independent ClickHouse queries. Per-project
  and per-server read limits apply on each server.
- **Projects and keys.** The registry is a versioned document in the `registry`
  table, containing key hashes only. Every change is a compare-and-set, so two
  servers cannot overwrite each other's changes; a conflicting change fails and
  can be retried. Servers adopt changes within `registry_refresh_seconds`. A key
  created through one server works on the others immediately: an unknown key
  makes a server check for registry changes, at most once per second. Revoked
  or rotated keys stop working on other servers at their next refresh.
- **Retention.** Reports hide expired events immediately. Every server runs the
  expiration sweep, which deletes expired rows. As with S3 storage, extending
  retention before the next sweep can make not-yet-deleted events visible again.

`project` and `key` commands work while servers run. Run them in a server's
container, or in any container configured with the same database:

```sh
docker compose exec analytics chunkify-analytics project create "Customer"
```

They return `"restart_required": false` because servers apply changes live, except
`project configure`, whose ingestion limits apply when a server starts.

## Differences from local storage

- **Duplicates.** Like local storage, a repeated event ID keeps its first payload
  and returns `200` with `"duplicate": true`. ClickHouse storage checks IDs within
  `deduplication_window_seconds`. If two servers receive the same new event at
  the same moment, both copies can be stored; reports count each view once, but
  `GET /v1/events` lists both.
- **Read-after-write.** A read never waits for writes that have not been
  acknowledged yet.
- **Floating-point sums.** Totals such as `total_playing_time_ms` can differ from
  local storage in the last digits because ClickHouse adds values in a different
  order. Video rankings remain stable across pages.

## Scale ClickHouse itself

One ClickHouse server is enough for most installations. To replicate it, create
the database yourself with the [Replicated database engine](https://clickhouse.com/docs/engines/database-engines/replicated)
before running `init`. Tables created by `init` are then replicated automatically:

```sql
CREATE DATABASE chunkify_analytics ON CLUSTER my_cluster
ENGINE = Replicated('/clickhouse/databases/chunkify_analytics', '{shard}', '{replica}');
```

Point analytics servers at replicas through a load balancer. Replicas apply
writes asynchronously; for read-your-writes behavior across replicas, route each
analytics server to one replica, or configure `insert_quorum` in the ClickHouse
user profile.

## Backups and moving data

Back up the ClickHouse database with
[`BACKUP DATABASE`](https://clickhouse.com/docs/operations/backup) or your
ClickHouse provider's tools. Include the `registry` table: it holds project
settings and key hashes. Changing the backend does not copy existing events.

## Schema

| Table | Contents |
| --- | --- |
| `events` | One row per stored event, ordered by project and receipt order and partitioned by receipt month. Location and page columns are extracted at insert time. |
| `registry` | Versioned project registry documents. The highest version is active. |

`init` and `serve` create missing tables. Do not change these tables by hand.
