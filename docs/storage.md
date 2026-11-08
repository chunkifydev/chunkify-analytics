# Storage

Chunkify Analytics stores events, projects and credential hashes in a
[ClickHouse](https://clickhouse.com/) database. ClickHouse is open source
(Apache 2.0) and runs in its own container or on its own host; no hosted service
is required. ClickHouse 25.8 LTS is tested.

Analytics servers keep no data of their own. Any number of servers can use the
same database: add servers behind a load balancer to handle more ingestion and
reads, and replace them at any time.

## Configure

Point every analytics server at the same database:

```toml
[clickhouse]
url = "http://clickhouse:8123"
database = "chunkify_analytics"
user = "chunkify"
# password = "..."   # Prefer CHUNKIFY_CLICKHOUSE_PASSWORD.
```

| TOML field | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `clickhouse.url` | `CHUNKIFY_CLICKHOUSE_URL` | `http://127.0.0.1:8123` | ClickHouse HTTP interface. HTTP or HTTPS, without credentials, query or fragment. |
| `clickhouse.database` | `CHUNKIFY_CLICKHOUSE_DATABASE` | `chunkify_analytics` | Database for events and the project registry. `init` creates it if missing. Letters, digits and underscores. |
| `clickhouse.user` | `CHUNKIFY_CLICKHOUSE_USER` | ClickHouse default user | ClickHouse user. |
| `clickhouse.password` | `CHUNKIFY_CLICKHOUSE_PASSWORD` | Empty | ClickHouse password. |
| `clickhouse.async_insert` | — | `true` | Let ClickHouse combine inserts from all servers into fewer parts. Each request still waits until its events are stored. |
| `clickhouse.registry_refresh_seconds` | — | `5` | How often each server checks the shared registry for project and key changes. 1 to 3600. |
| `clickhouse.deduplication_window_seconds` | — | `604800` (7 days) | Repeated event IDs received within this window are reported as duplicates and not stored again. `0` stores every delivery. |
| `clickhouse.node_id` | `CHUNKIFY_CLICKHOUSE_NODE_ID` | Random | 0 to 63. Part of each event's internal order. Give each server a different value for strict uniqueness. |

The user needs permission to create the database and tables, and to read,
insert and delete rows in them. Run `init` once per database, not once per
server.

Each server applies its own `server.ingest_per_second`, `[batch]` and
`[rate_limits]` settings, so installation-wide limits grow with the number of
servers.

## How servers share work

- **Ingestion.** Each server batches the requests it receives into one insert;
  requests that arrive while a batch is saved form the next batch. A response is
  sent after ClickHouse has stored the events, so acknowledged events survive a
  server crash. Graceful shutdown finishes every accepted request first.
- **Reads.** Each report is one or two independent ClickHouse queries.
  Per-project and per-server read limits apply on each server. When a client
  disconnects, ClickHouse cancels its query.
- **Projects and keys.** The registry is a versioned document in the `registry`
  table and contains key hashes only. Every change is a compare-and-set: when
  two servers or commands change projects at the same time, the later change is
  applied again to the newest registry, so neither change is lost. Servers adopt
  changes within `registry_refresh_seconds`. A key created through one server
  or the CLI works on every server at once, because an unknown key makes a
  server check the registry first. Rotated or revoked keys stop working on other
  servers at their next refresh.
- **Retention.** Reports hide expired events immediately and every server runs
  the expiration sweep, which deletes them. When a project's retention changes,
  events that had already expired stay expired, even if retention grows.

`project` and `key` commands work while servers run. Run them anywhere with the
same `[clickhouse]` settings, for example in an analytics container:

```sh
docker compose exec analytics chunkify-analytics project create "Customer"
```

They return `"restart_required": false`, except `project configure`, whose
ingestion limits apply when a server starts.

## Duplicate deliveries

A repeated event ID keeps its first payload and returns `200` with
`{"accepted":false,"duplicate":true}`. Servers check IDs received within
`deduplication_window_seconds`. If two servers receive the same new event at
the same moment, both copies can be stored; reports count each view once, but
`GET /v1/events` lists both.

## Expiration

Expiration uses server receipt time. It defaults to 30 days, and projects can
override it. Set `seconds = 0` under `[retention]` to keep events indefinitely.
There is no manual event deletion command.

## Scale ClickHouse

One ClickHouse server is enough for most installations. To replicate it, create
the database with the
[Replicated database engine](https://clickhouse.com/docs/engines/database-engines/replicated)
before running `init`. Tables created by `init` are then replicated
automatically:

```sql
CREATE DATABASE chunkify_analytics ON CLUSTER my_cluster
ENGINE = Replicated('/clickhouse/databases/chunkify_analytics', '{shard}', '{replica}');
```

Replicas apply writes asynchronously. For read-your-writes behavior across
replicas, route each analytics server to one replica, or set `insert_quorum`
in the ClickHouse user's profile.

## Schema

| Table | Contents |
| --- | --- |
| `events` | One row per stored event, ordered by project and receipt order and partitioned by receipt month. Location and page columns are extracted at insert time. |
| `registry` | Versioned project registry documents. The highest version is active. |

`init` and `serve` create missing tables. Do not change these tables by hand.
See [backup and restore](administration.md#backup-and-restore).
