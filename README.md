# Chunkify Analytics

Collect playback events from your video player and query views, watch time,
engagement, startup delays, buffering, and playback errors. You run the service
on your own infrastructure, and its data stays in your storage.

Chunkify Analytics includes:

- Event ingestion through the Chunkify analytics SDK or OTLP/HTTP JSON logs.
- Separate projects and credentials for different websites or customers.
- An HTTP API for building dashboards.
- A read-only MCP endpoint for connecting AI tools.
- Local or S3-compatible storage with configurable expiration.

One `chunkify-analytics serve` process runs ingestion, queries, and MCP. You do
not need to install or run a separate database server.

Choose [Docker](#install-with-docker) for a packaged deployment, or
[install the executable directly](#install-without-docker) on your computer.

## Install with Docker

Docker is the recommended way to run the service. These instructions build the
image from this repository; prebuilt releases are not available yet.

### 1. Get the application

You need Git and Docker with BuildKit support.

```sh
git clone https://github.com/chunkifydev/chunkify-analytics.git
cd chunkify-analytics
docker build -t chunkify-analytics:local .
```

The first build compiles DuckDB and can take tens of minutes. Later builds can
reuse cached dependencies. On a small build machine, add
`--build-arg BUILD_JOBS=1` to reduce simultaneous compilation work.

### 2. Configure your website

```sh
cp collector.toml.example collector.toml
```

Edit `collector.toml` and set the website origin that will send events:

```toml
[server]
allowed_origins = ["http://localhost:3000"]
store_ip = true

[retention]
seconds = 2592000
```

Use the exact origin, including the scheme and any nonstandard port, without a
path or trailing slash. For local development, that might be
`http://localhost:3000`. Add multiple origins as separate array entries.

Retention defaults to 30 days. Set `store_ip = false` under `[server]` to stop
the collector from adding request IP addresses to new events.

### 3. Initialize persistent storage

```sh
docker volume create chunkify-analytics-data

docker run --rm \
  --mount type=volume,src=chunkify-analytics-data,dst=/data \
  --mount type=bind,src="$(pwd)/collector.toml",dst=/data/collector.toml,readonly \
  chunkify-analytics:local init
```

Run `init` once for a new installation. It generates your initial credentials
and refuses to overwrite existing credentials.

### 4. Start the service

```sh
docker run -d --name chunkify-analytics --restart unless-stopped \
  --mount type=volume,src=chunkify-analytics-data,dst=/data \
  --mount type=bind,src="$(pwd)/collector.toml",dst=/data/collector.toml,readonly \
  -p 127.0.0.1:4174:4174 \
  chunkify-analytics:local
```

Check that it is running:

```sh
curl --fail http://127.0.0.1:4174/healthz
docker logs chunkify-analytics
```

The health endpoint returns `{"status":"ok"}`. The example exposes the port on
localhost. Put your HTTPS reverse proxy in front of it when accepting events
from a public website. A proxy running in another container can use a shared
Docker network instead.

Keep the named volume when replacing containers. It holds the database and
credentials. Run one collector process per data volume.

The container loads `/data/collector.toml` automatically. Edit the host file and
recreate the container with the same mounts to apply changes. The image uses
`/data` for storage and listens on `0.0.0.0:4174` inside the container; these two
image settings override `data_dir` and `[server].bind` in TOML. The port mapping
above restricts access from your computer to localhost.

### 5. Get your credentials

```sh
docker exec chunkify-analytics cat /data/keys.json
```

The initial `default` project has two credentials:

| Credential | Use |
| --- | --- |
| `ingest_key` | Send events from the player. This key can be included in browser code. |
| `read_token` | Read analytics through the HTTP API or MCP. Keep it on a backend or in private AI-client settings. |

Save these values privately. You can create separate named keys for each
integration and revoke them independently.

## Install without Docker

Run the executable directly on your computer without a container. You need Git,
Rust 1.99.0, the toolchain used by CI, and a C++17 compiler for the initial build.
On macOS, the Xcode Command Line Tools provide the compiler. On Linux, use your
distribution's C++ build tools. No Node.js runtime is required for the collector.

### 1. Build and install the executable

```sh
git clone https://github.com/chunkifydev/chunkify-analytics.git
cd chunkify-analytics
cargo build --locked --release
mkdir -p "$HOME/.local/bin"
install -m 755 target/release/chunkify-analytics "$HOME/.local/bin/"
export PATH="$HOME/.local/bin:$PATH"
```

Keep `$HOME/.local/bin` in your shell's PATH to use the command in future
sessions. The first build compiles DuckDB; you do not install DuckDB separately.

### 2. Configure and initialize

From the repository directory:

```sh
cp collector.toml.example collector.toml
# Edit collector.toml before continuing.
chunkify-analytics config check
chunkify-analytics init
```

Edit the website origins and data path before initialization. The example stores
its data in `./data`, beside `collector.toml`. Read the initial credentials with:

```sh
cat data/keys.json
```

Use `ingest_key` in the player and keep `read_token` private for API and MCP
access. This read token must stay out of browser code.

### 3. Run and check the service

```sh
chunkify-analytics serve
```

Leave this process running. In another terminal:

```sh
curl --fail http://127.0.0.1:4174/healthz
```

The HTTP API and `/mcp` are both available on `127.0.0.1:4174`. Press Ctrl+C in
the server terminal to stop gracefully. Run `serve` again to reuse the same data;
you do not run `init` again.

When starting from another directory, select the configuration file explicitly:

```sh
chunkify-analytics --config /path/to/collector.toml serve
```

For a service manager, use an explicit `--config` path and make the data directory
writable by the service account. Keep the complete data directory when moving or
updating the executable.

## Send playback events

Install the analytics SDK in your player application:

```sh
npm install @chunkify/analytics
```

Connect it to a video element:

```js
import { monitor } from "@chunkify/analytics";
import { createChunkifyAdapter } from "@chunkify/analytics/chunkify";

const session = monitor(document.querySelector("video"), {
  videoId: "video-123",
  destinations: [
    createChunkifyAdapter("http://localhost:4174/v1/events", {
      ingestKey: "YOUR_PROJECT_INGEST_KEY",
    }),
  ],
});
```

This URL connects to the collector on your computer for testing. Use that
project's ingest key, and allow the player's origin in `[server].allowed_origins`.
For a deployed player, replace the URL with your collector's public HTTPS address.
The adapter includes browser and device context by default; set
`collectBrowserContext: false` in
its options to disable that collection.

For OTLP/HTTP JSON logs, use `createOtlpAdapter` from
`@chunkify/analytics/otlp`, point it at `/v1/logs`, and supply the ingest key in
`X-Chunkify-Ingest-Key`. This endpoint accepts Chunkify playback events carried
in OTLP logs; it is not a general metrics or traces backend.

See the [API reference](docs/api.md) for payloads, authentication, filters, and
error responses, or inspect the [OpenAPI specification](openapi.json).

## Read analytics

Use a private project read token from your backend:

```sh
export READ_TOKEN='YOUR_PRIVATE_PROJECT_READ_TOKEN'

curl --fail http://127.0.0.1:4174/v1/summary \
  -H "Authorization: Bearer $READ_TOKEN"
```

| Endpoint | Returns |
| --- | --- |
| `GET /v1/summary` | Views, distinct supplied viewer IDs, and total playing time. |
| `GET /v1/videos` | A paginated list of videos and their totals. |
| `GET /v1/timeseries` | Hourly or daily metrics for a requested date range. |
| `GET /v1/engagement` | Average watch time and observed reached-end rates. |
| `GET /v1/quality` | Startup timing, buffering, and fatal-error metrics. |
| `GET /v1/events` | Stored events, context, and pagination cursors. |
| `GET /v1/ingestion` | Ingestion status. |

Read tokens grant access to one project. Keep them out of browser code. Build
customer dashboards with a backend that checks the customer's access before
calling the collector.

Use the summary endpoints for totals instead of adding event snapshots together.
Watch time is cumulative, and unique viewer IDs cannot be summed across videos
or dates. The [metric definitions](docs/api.md#metric-definitions) explain
which events contribute to each report.

A [sample dashboard](examples/dashboard/README.md) is included. For a native
setup with synthetic events and a player, see the [local demo](examples/demo/README.md).

## Connect Claude, Codex, or Cursor with MCP

MCP runs inside the collector process by default. Configure an MCP client that
supports Streamable HTTP and a bearer-token header with:

```text
URL: http://127.0.0.1:4174/mcp
Authorization: Bearer YOUR_PRIVATE_PROJECT_READ_TOKEN
```

Use the deployed HTTPS URL for a remote installation. When exposing a hostname
such as `analytics.example.com`, add it to the MCP hostname allowlist in TOML:

```toml
[mcp]
allowed_hosts = ["localhost", "127.0.0.1", "[::1]", "analytics.example.com"]
```

The MCP tools provide read-only access to the authenticated project's analytics.
They cannot ingest events or administer projects. Set `enabled = false` under
`[mcp]` to disable the endpoint.

No second collector process is required. Clients that only support standard
input/output can use the optional `chunkify-analytics mcp` adapter. See the
[MCP setup guide](docs/mcp.md) for both connection methods and available tools.

## Configure the service

Use `collector.toml` as your main configuration file, as shown in both installation
guides. Environment variables and `.env` are optional overrides, useful for
credentials and deployment-specific settings. Explicit CLI flags have the highest
priority:

```text
CLI flags > process environment > .env > TOML > built-in defaults
```

A minimal `collector.toml` is shown below. The complete
[example file](collector.toml.example) includes every setting.

```toml
data_dir = "./data"

[server]
bind = "127.0.0.1:4174"
allowed_origins = ["http://localhost:3000"]
store_ip = true

[retention]
seconds = 2592000

[duckdb]
threads = 1
```

The executable loads `collector.toml` and `.env` from its working directory when
present. Select other files with `--config` and `--env-file`:

```sh
chunkify-analytics --config /etc/chunkify/collector.toml config check
chunkify-analytics --config /etc/chunkify/collector.toml serve
```

Relative data paths in TOML resolve beside that file. Relative CLI and
environment paths resolve from the working directory. Configuration changes
apply when the process starts. `config check` validates server settings without
creating or opening a database.

The Docker guide mounts the file as `/data/collector.toml`, where the executable
finds it automatically. For optional environment overrides, use `-e NAME=value`
or Docker's `--env-file .env`. The image sets `COLLECTOR_DATA_DIR=/data` and
`COLLECTOR_BIND=0.0.0.0:4174`; those environment values take priority over TOML.

See the [configuration reference](docs/configuration.md) for every setting,
accepted range, and advanced option.

## Manage projects and keys

Run administration commands against the same data directory as the collector.
With Docker, use the executable inside the container:

```sh
docker exec chunkify-analytics chunkify-analytics project list
docker exec chunkify-analytics chunkify-analytics key create default read "Dashboard"
docker exec chunkify-analytics chunkify-analytics project retention default 604800
```

The last command keeps the default project's events for seven days. Use `0` to
keep them indefinitely or `inherit` to restore the installation default.

On Linux and macOS, listing projects, changing retention, and managing keys work
while the service is running. Stop it before creating or renaming a project,
or replacing a project's full settings.

For native installations, the same commands run directly:

```sh
chunkify-analytics project create production --name "Production website"
chunkify-analytics key create production ingest "Website"
chunkify-analytics key rotate production KEY_ID
chunkify-analytics key revoke production KEY_ID
```

Creating or rotating a named key prints its new secret. Save it when returned;
you cannot retrieve it later. Rotation replaces the old secret immediately. To
switch clients without an interruption, create another key, update the clients,
then revoke the old one.

Use `chunkify-analytics --help` or a command such as
`chunkify-analytics project create --help` for arguments. The
[administration guide](docs/administration.md) includes project settings,
Docker commands and backup/restore procedures.

## Storage, retention, and backups

Choose storage in one flat `[storage]` section. Local storage is the default:

```toml
[storage]
backend = "local"
# Optional database file, relative to this TOML file:
path = "./data/analytics.duckdb"
```

For S3 or Cloudflare R2, set `backend = "s3"` and supply the bucket URL and
connection settings. Events and installation credentials persist in object
storage, so a replacement ECS Fargate task can start with an empty local data
directory. Run one collector task per bucket prefix. See the
[storage guide](docs/storage.md) for a complete R2 example and deployment limits.

Expiration defaults to 30 days by server receipt time, with cleanup at startup
and hourly afterward. Projects can have different durations. Expiration also
removes those events' contributions to reports. It limits age, not storage size.
Local files can reuse space after cleanup; their disk size need not shrink.

For local storage, back up the data directory and any custom database path while
the collector is stopped. For S3, back up the configured prefix, including its
`keys.json`. See [backup and restore](docs/administration.md#backup-and-restore).

## Troubleshooting

| Symptom | Check |
| --- | --- |
| `401` response | Use an ingest key for writes and a read token for reads/MCP. Check whether the key was rotated or revoked. |
| Browser requests receive `403` | Match the website's exact origin in the project's allowed origins. |
| MCP requests receive `403` | Add the client-facing HTTP hostname to `[mcp].allowed_hosts`; browser Origin headers must also be allowed. |
| Ingestion receives `429` | Requests exceed an installation or project rate limit. See the admission settings. |
| Requests receive `503` | The write/read queue or another configured resource budget is full. Retry later and check logs. |
| Data directory is in use | Another collector or an offline administration command owns it. Run one writer per directory. |
| Container configuration did not change | Recreate the container with the updated TOML file and the same data volume. Environment changes also require recreation. |

## Further documentation

- [Storage backends and R2 setup](docs/storage.md)
- [Configuration reference](docs/configuration.md)
- [HTTP API and metric definitions](docs/api.md)
- [Projects, credentials, retention, and backups](docs/administration.md)
- [MCP setup](docs/mcp.md)
- [Development and testing](docs/development.md)

Released under the [MIT license](LICENSE).
