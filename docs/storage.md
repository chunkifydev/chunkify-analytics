# Storage

Chunkify Analytics stores events, projects and credential hashes in a
[ClickHouse](https://clickhouse.com/) database. ClickHouse is open source
(Apache 2.0) and runs in its own container or on its own host; no hosted service
is required. ClickHouse 26.10 is tested.

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
| `clickhouse.async_insert` | — | `true` | Let ClickHouse combine inserts from all servers into fewer parts, buffering each batch for 10 to 50 ms. Each request still waits until its events are stored. `false` writes each batch directly, which suits a single server. |
| `clickhouse.registry_refresh_seconds` | — | `5` | How often each server checks the shared registry for project and key changes. 1 to 3600. |
| `clickhouse.deduplication_window_seconds` | — | `604800` (7 days) | Repeated event IDs received within this window are reported as duplicates and not stored again. `0` stores every delivery. |
| `clickhouse.node_id` | `CHUNKIFY_CLICKHOUSE_NODE_ID` | Leased | 0 to 63. Part of each event's internal order, unique per running server. When unset, each server leases a free value from ClickHouse for as long as it runs; startup fails if all 64 are in use. Set it to assign values yourself. |
| `clickhouse.rollup_interval_seconds` | — | `45` | How often servers refresh [report rollups](#report-rollups) when `rollup_max_lag_seconds` is above zero. 0 to 3600. `0` turns rollups off. |
| `clickhouse.rollup_max_lag_seconds` | — | `0` | Let reports read [report rollups](#report-rollups) while the last refresh is at most this old. 0 to 3600. `0` keeps reports exact and turns refreshes off. Otherwise at least twice `rollup_interval_seconds`; 120 suits the default interval. |

The user needs permission to create the database and tables, and to read,
insert and delete rows in them. Run `init` once per database, not once per
server.

Each server applies its own `server.ingest_per_second`, `[batch]` and
`[rate_limits]` settings, so installation-wide limits grow with the number of
servers.

Reports and inserts share ClickHouse's CPU. Each server runs up to
`rate_limits.read_concurrent` report queries at once, so the total across
servers should stay near the number of ClickHouse cores. On a 4-core host, a
limit of 8 instead of the default 32 kept ingestion three times higher while
dashboards were saturated, with similar report throughput. See the
[capacity benchmark](development.md#ingestion-and-read-capacity) to measure
your own host.

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
  server check the registry first. Those checks run one at a time, at most once
  per second per server, so anyone sending unknown keys cannot load ClickHouse;
  a request with a new key waits up to about a second for the next one.
  Rotated or revoked keys stop working on other servers at their next refresh.
- **Report rollups.** When reports may lag, one server at a time refreshes the
  hourly rollups every `rollup_interval_seconds`; the others skip that interval. See
  [report rollups](#report-rollups).
- **Retention.** Reports hide expired events immediately. For each sweep
  interval, one server claims and runs the expiration sweep, which deletes them. When a project's retention changes,
  events that had already expired stay expired, even if retention grows.

`project` and `key` commands work while servers run. Run them anywhere with the
same `[clickhouse]` settings, for example in an analytics container:

```sh
docker compose exec analytics chunkify-analytics project create "Customer"
```

They return `"restart_required": false`, except `project configure`, whose
ingestion limits apply when a server starts.

## Report rollups

Reports count each view once, from its first start and its final or latest
measurement. By default every report works that out from events, so its
results include every event stored before it ran, and its ClickHouse CPU grows
with the views in its date range.

Set `rollup_max_lag_seconds` above zero to let reports lag stored events
instead. Reports then read every hour as of the last completed rollup refresh:
whole hours from per-hour aggregates, partial hours at the ends of a range from
per-view results. They do no per-view work at all, and lag stored events by the
refresh interval plus about ten seconds. Hours with expired events are still
read from events, so expired events stay hidden at once. Unique viewers and
percentiles are exact; totals can differ from an events-only result in the
last digits of floating-point sums, because the additions happen in another
order.

These reports read rollups when their filters are at most a date range, a
video and one context value (for a breakdown, the context value must be of its
`group_by` dimension):

- summary, engagement and quality totals and breakdowns;
- time series;
- videos sorted by `video_id`, `views` or `unique_viewers`.

Other filters (viewer, view, event name, or two context values), other video
sort orders, and every other report read events.

Reports lag stored events by the refresh interval plus about ten seconds, and
by the refresh's own duration. If no refresh completed within
`rollup_max_lag_seconds`, for example because refreshes fail, reports read
events until one does. Keep that limit well above the interval: under heavy
read load a refresh takes longer, and reports that fall back to events add the
very load that slows it. With the default 45-second interval, 120 works well. The next pages of a
breakdown or video list read the same refresh as their first page, so a refresh
between pages does not move items across pages; a cursor older than about 20
refreshes reads the latest one instead.

### How refreshes work

A materialized view keeps each view's aggregation state (`view_states`) as
events are inserted. Every `rollup_interval_seconds`, one server refreshes the
hours whose views received events since the previous refresh: it merges the
states of the views that started in those hours, which costs in proportion to
those views rather than their events, then rebuilds those hours' aggregates.
A refresh claims its run in the `rollup_runs` table, so one server works at a
time; if a server stops in the middle of a run, another takes over after four
intervals (at least one minute). With `rollup_max_lag_seconds = 0`, servers do
not refresh at all, but `view_states` is still kept at insert.

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
before running `init`. `init` and `serve` detect the Replicated engine and
create every table as `ReplicatedMergeTree`, so all replicas share events, the
registry, rollups and claims. Plain `MergeTree` tables in a Replicated database
would keep their data on one node. The database must exist before `init`:

```sql
CREATE DATABASE chunkify_analytics ON CLUSTER my_cluster
ENGINE = Replicated('/clickhouse/databases/chunkify_analytics', '{shard}', '{replica}');
```

Replicas apply writes asynchronously. For read-your-writes behavior across
replicas, route each analytics server to one replica, or set `insert_quorum`
in the ClickHouse user's profile. Registry changes, rollup runs and claims
stay single-winner across replicas, because ClickHouse Keeper deduplicates
their inserts. This setup was tested with two replicas and ClickHouse Keeper.

## Schema

| Table | Contents |
| --- | --- |
| `events` | One row per stored event, ordered by project and receipt order and partitioned by receipt month. Location, page and the numeric fields reports measure are extracted at insert time, and ClickHouse records when it stored each row. |
| `registry` | Versioned project registry documents. The highest version is active. |
| `view_states` | Each view identity's aggregation states, kept by a materialized view as events are inserted. Fully expired views are deleted by the expiration sweep. |
| `rollup`, `rollup_hours`, `rollup_views` | [Report rollups](#report-rollups): partial aggregates per hour, each hour's receipt range, and per-view results. Derived from `events`; superseded rows are deleted after 20 refreshes and expired hours by the expiration sweep. |
| `rollup_runs` | Rollup refresh claims and completions. |
| `claims` | Short leases between servers: node IDs and expiration sweeps. Rows expire after a day. |

`init` and `serve` create missing tables. Do not change these tables by hand.
See [backup and restore](administration.md#backup-and-restore).
