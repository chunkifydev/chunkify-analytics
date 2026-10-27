# Local collector and dashboard demo

Run a local collector, dashboard and video player with sample analytics.
Docker is not required.

## Build and start

You need Node.js 24, Rust 1.99.0 and a C++17 compiler. See the
[installation guide](../../README.md#install-without-docker) for build requirements.
From the repository root:

```sh
npm ci
cargo build --locked
npm run demo -- --collector target/debug/chunkify-analytics
```

The first build compiles DuckDB and can take several minutes. The launcher creates
separate Demo data and Player test projects, loads 816 synthetic events for 268
views, and opens the dashboard at `http://127.0.0.1:8787`.

Use `--port 0` to choose an available port or `--no-open` to print the URL without
opening a browser:

```sh
npm run demo -- --collector target/debug/chunkify-analytics --port 0 --no-open
```

Keep the terminal open. Press Ctrl+C to stop. Run the same command to restart;
projects, credentials and events are reused, and interrupted sample loading resumes.
After updating source, rerun `npm ci` and `cargo build --locked` before launching.

## Try playback

1. Open **Demo data** to explore views, playback quality and engagement.
2. Open **Try the player**, paste a browser-playable MP4 or HLS URL and start a view.
3. Open **Player test analytics** and press **Refresh**. Snapshots update watch
   time after five seconds of playback.
4. Press **Finish view**, then refresh to inspect the final result.

The player already has its public key. Secret keys stay on the dashboard backend.
Player libraries load from jsDelivr and need internet access. The video must allow
browser access. Rendition measurements can be unavailable with MP4 or native HLS.

The player event counter shows emitted events; the dashboard confirms receipt.
Its fixed demo viewer ID does not count individual people. Charts use manual
refresh. **Sample dates** restores the seeded date range.

## Saved data

The launcher stores its database, credentials, configuration and logs in
`demo-data/`. Keep this directory private. It is ignored by Git. Retention is
disabled for samples, and the launcher uses its own configuration.

To start fresh, stop the demo and move `demo-data/` elsewhere, or select a new
location with `--data-dir /path/to/new-directory`. The launcher refuses existing
directories without a demo marker. Run one demo per directory.

The dashboard listens on loopback and has no login. Anyone with access to that
local port can read the demo projects.

## Shared bundles

To use a prepared bundle, install Node.js 24, extract the archive, and run
`npm run demo`. On macOS, **Start demo.command** launches it too. Data persists
beside `package.json`.

To prepare a bundle with an executable built for the recipient's OS and architecture:

```sh
npm run demo:pack -- --collector /absolute/path/to/chunkify-analytics
```

The archive appears in `dist/chunkify-analytics-demo-<platform>-<architecture>.tar.gz`.
It includes demo source, the SDK dependency and the executable. It excludes
local databases and credentials. Build and package on the matching platform.
Bundles are not signed or notarized; follow your operating system's normal
approval process for downloaded executables.
