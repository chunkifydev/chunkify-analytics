# Chunkify Analytics

Collect playback events from your video player and query views, watch time,
engagement, startup delays, buffering, and playback errors. You run the service
on your own infrastructure, and its data stays in your storage.

Chunkify Analytics includes:

- Event ingestion through the Chunkify analytics adapter.
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

The build downloads a prebuilt DuckDB library and compiles the collector.
On a small build machine, add
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
| `public_key` | Starts with `ck_public_`. Use it in the player to send events. |
| `secret_key` | Starts with `ck_secret_`. Full project access: read analytics, update or delete the project, and manage keys. Keep it on your backend. |

Save these values privately. You can create separate named keys for each
integration and revoke them independently.

## Install without Docker

Run the executable directly on your computer without a container. You need Git,
Rust 1.99.0, the toolchain used by CI, and your platform's C build tools.
On macOS, install the Xcode Command Line Tools. On Linux, use your distribution's
build tools. No Node.js runtime is required for the collector.

### 1. Build and install the executable

```sh
git clone https://github.com/chunkifydev/chunkify-analytics.git
cd chunkify-analytics
sh scripts/install-duckdb.sh
cargo build --locked --release
mkdir -p "$HOME/.local/bin" "$HOME/.local/lib"
install -m 755 target/release/chunkify-analytics "$HOME/.local/bin/"
install -m 755 target/lib/libduckdb.* "$HOME/.local/lib/"
export PATH="$HOME/.local/bin:$PATH"
```

Keep `$HOME/.local/bin` in your shell's PATH to use the command in future
sessions. The setup script downloads DuckDB 2.0.0 for macOS or Linux and verifies
its checksum. The executable finds the library in the adjacent `lib` directory.

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

Use `public_key` in the player and keep `secret_key` private for API and MCP
access. This secret key must stay out of browser code.

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
      ingestKey: "YOUR_PROJECT_PUBLIC_KEY",
    }),
  ],
});
```

This URL connects to the collector on your computer for testing. Use that
project's public key, and allow the player's origin in `[server].allowed_origins`.
For a deployed player, replace the URL with your collector's public HTTPS address.
The adapter includes browser and device context by default; set
`collectBrowserContext: false` in
its options to disable that collection.

See the [API reference](docs/api.md) for payloads, authentication, filters, and
error responses, or inspect the [OpenAPI specification](openapi.json).

## Read analytics

Use a private secret key from your backend. For dashboards and MCP, create a
read-only private key with `key create PROJECT_ID secret "Dashboard" --read-only`:

```sh
export SECRET_KEY='YOUR_PRIVATE_SECRET_KEY'

curl --fail http://127.0.0.1:4174/v1/summary \
  -H "Authorization: Bearer $SECRET_KEY"
```

| Endpoint | Returns |
| --- | --- |
| `GET /v1/summary` | Views, distinct supplied viewer IDs, and total playing time. |
| `GET /v1/videos` | A paginated list of videos and their totals, sortable by views or watch time. |
| `GET /v1/timeseries` | Hourly or daily metrics for a requested date range. |
| `GET /v1/engagement` | Average watch time and observed reached-end rates. |
| `GET /v1/quality` | Startup timing, buffering, and fatal-error metrics. |
| `GET /v1/events` | Stored events, context, and pagination cursors. |

Secret keys grant access to one project. Keep them out of browser code. Build
customer dashboards with a backend that checks the customer's access before
calling the collector.

For the ten videos with the most views, query
`/v1/videos?sort_by=views&order=desc&limit=10`. Use
`sort_by=total_playing_time_ms` to rank by total watch time.

Use the summary endpoints for totals instead of adding event snapshots together.
Watch time is cumulative, and unique viewer IDs cannot be summed across videos
or dates. The [metric definitions](docs/api.md#metric-definitions) explain
which events contribute to each report.

A [sample dashboard](examples/dashboard/README.md) is included. For a native
setup with synthetic events and a player, see the [local demo](examples/demo/README.md).

## Connect an MCP client

Connect directly to `http://127.0.0.1:4174/mcp` using Streamable HTTP and the
`Authorization: Bearer YOUR_PRIVATE_SECRET_KEY` header. Use your deployed HTTPS
address for remote access and add its hostname to `[mcp].allowed_hosts`.

MCP tools read the authenticated project's analytics. See the
[MCP guide](docs/mcp.md) for client setup and the optional stdio adapter.

## Configure the service

Keep application settings in `collector.toml`. Use `--config` when running from
another directory:

```sh
chunkify-analytics --config /etc/chunkify/collector.toml config check
chunkify-analytics --config /etc/chunkify/collector.toml serve
```

Relative TOML paths resolve beside that file. Restart after changing installation
settings. The [configuration reference](docs/configuration.md) lists all fields
and the small set of environment overrides for deployments and secrets.

## Manage projects and keys

Give each customer or website a separate project and credentials:

```sh
chunkify-analytics project create "Production website"
chunkify-analytics key create PROJECT_ID secret "Dashboard" --read-only
chunkify-analytics project retention PROJECT_ID 604800
```

Read-only private keys can query analytics through HTTP and MCP. Existing and
new private keys without `--read-only` keep full project access.

Use the returned `project_id` in later commands. Save new secrets when returned.
In Docker, run these commands through
`docker exec chunkify-analytics chunkify-analytics …`. Project creation, key
management and retention changes work while serving on Linux and macOS. Stop the collector
to rename projects or replace project settings. For automatic customer onboarding,
use [project creation through the API](docs/administration.md#create-projects-through-the-api)
with `COLLECTOR_ADMIN_KEY`.

## Storage and backups

Local storage is the default. Set `backend = "s3"` under `[storage]` for S3-compatible
storage, including Cloudflare R2. S3 keeps events and credentials when a container
is replaced. Run one collector per bucket prefix.

Expiration defaults to 30 days by receipt time. Projects can override it.
Expired events stop contributing to reports. Local database files can reuse
freed space without shrinking.

See [storage](docs/storage.md) for configuration and
[backup and restore](docs/administration.md#backup-and-restore) before moving data.

## Troubleshooting

| Symptom | Check |
| --- | --- |
| `401` response | Use an public key for writes and a secret key for reads/MCP. Check whether the key was rotated or revoked. |
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
