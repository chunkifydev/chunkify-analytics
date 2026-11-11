# Chunkify Analytics on ClickHouse with Docker Compose

This stack runs:

- `clickhouse`: one ClickHouse server, storing events and projects in a volume.
- `analytics`: the analytics server. Scale it to any number of replicas.
- `gateway`: Caddy, listening on `127.0.0.1:4174` and spreading requests over
  every `analytics` replica.

## Start

You need Docker with Compose.

```sh
cd examples/clickhouse
cp .env.example .env
```

Set `CLICKHOUSE_PASSWORD` in `.env` to a long random value, and set your website
origin in `config.toml`. Then create the database and initial project once:

```sh
docker compose run --rm analytics init
```

Save the `public_key` and `secret_key` it prints; they are shown only once.
Start the stack with three analytics servers:

```sh
docker compose up -d --scale analytics=3
curl --fail http://127.0.0.1:4174/healthz
```

Send events and read reports at `http://127.0.0.1:4174`, as described in the
[main README](../../README.md#send-playback-events). Change the replica count at
any time with `--scale analytics=N`; the gateway picks up new replicas within
five seconds.

Put your HTTPS reverse proxy in front of the gateway when accepting events from
a public website.

## Resources

`clickhouse.xml` keeps ClickHouse small: server logging at warnings, no
trace, text or profiling system logs, and caches of a few hundred MiB. With
it, the capacity benchmark (900k events, 64 concurrent readers and continuous
ingestion) ran within a 6 GiB ClickHouse memory limit on 4 CPUs. Without it, the
default trace logging and cache sizes exhausted that limit after about an hour.

Give ClickHouse at least 4 GiB of memory and 2 CPUs. On a larger dedicated
server, raise the cache sizes or remove the file to use ClickHouse's defaults.

## Manage projects

Run commands in any analytics container:

```sh
docker compose exec analytics chunkify-analytics project create "Customer"
docker compose exec analytics chunkify-analytics project list
```

Every server applies project and key changes within a few seconds.

## Upgrade

Pull the new image, apply its schema migrations once, then restart the servers:

```sh
docker compose pull analytics
docker compose run --rm analytics migrate
docker compose up -d --scale analytics=3
```

Analytics servers do not start on a database that still needs migrations. See
[upgrade the schema](../../docs/storage.md#upgrade-the-schema).

## Use a locally built image

To use a locally built image, build it from the repository root and select it:

```sh
docker build -t chunkify-analytics:local .
echo 'CHUNKIFY_IMAGE=chunkify-analytics:local' >> examples/clickhouse/.env
```

See [storage](../../docs/storage.md) for settings, replication and
backups.
