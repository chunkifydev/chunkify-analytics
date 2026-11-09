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

## Manage projects

Run commands in any analytics container:

```sh
docker compose exec analytics chunkify-analytics project create "Customer"
docker compose exec analytics chunkify-analytics project list
```

Every server applies project and key changes within a few seconds.

## Use a locally built image

To use a locally built image, build it from the repository root and select it:

```sh
docker build -t chunkify-analytics:local .
echo 'CHUNKIFY_IMAGE=chunkify-analytics:local' >> examples/clickhouse/.env
```

See [storage](../../docs/storage.md) for settings, replication and
backups.
