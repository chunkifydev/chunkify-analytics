# Native collector demo

A temporary local demo for exploring the collector with coworkers. It starts a
native collector, a dashboard, and a player page. No Docker is involved.
The supplied Apple Silicon bundle is for macOS on M-series Macs. Node.js 24 is
required. The shared bundle includes the collector executable and sample-generator
dependencies, so recipients need neither Rust nor `npm install`.

## Start a shared bundle

1. Install [Node.js 24](https://nodejs.org/en/download) if needed.
2. Extract the demo archive into a folder you can write to.
3. On macOS, open **Start demo.command**, or run `npm run demo` from that folder.
4. Keep the terminal open. The launcher opens the dashboard when setup finishes.
5. Press Ctrl+C in the terminal to stop the demo. Its data is kept.

This development bundle is not notarized. macOS may block a downloaded launcher
or executable. After verifying the sender and file, follow your organization's
policy and Apple's normal Privacy & Security approval flow. Do not disable
Gatekeeper. Local execution is verified; a downloaded/notarized distribution
flow is not part of this temporary demo.

The first launch creates separate **Demo data** and **Player test** projects and
loads 268 synthetic views across nine videos. Subsequent launches reuse the
projects, keys and database. Interrupted sample loading resumes using the same
event IDs, so it does not duplicate views.

If port 8787 is already in use, run `npm run demo -- --port 0` to select a free
port. `--no-open` prints the URLs without opening a browser.
The collector uses its own available loopback port.

## Show what the collector can do

1. **Demo data:** explore Overview, Playback quality and Engagement. Filter by
   video, browser or device. Open an event to see resolution, selected rendition,
   time per rendition, browser context and the raw stored JSON.
2. **Try the player:** follow the link in the dashboard. A sample HLS video and
   `createChunkifyAdapter` are already configured. Press **Start demo view**.
3. **Player test analytics:** open this link from the player page and press
   **Refresh**. After five seconds of active playback, a snapshot updates watch
   time. Seek, pause or change quality to produce more events.
4. Press **Finish view**, then refresh to inspect the final summary. Pausing
   alone does not finish a view. **Start demo view** starts another monitored view.
5. Enter your own video URL and video ID, or copy the adapter example into your
   own player page. Local pages on port 4173 or 5173 are allowed by the demo.

Sample dates remain selected for Demo data even when you reopen the demo later.
Player test defaults to the last seven days. **Sample dates** restores the seeded
period. The dashboard uses manual refresh, with no polling.

The player and analytics SDK load from jsDelivr using pinned versions. The sample
video is [Big Buck Bunny on Mux's HLS test-stream host](https://test-streams.mux.dev/).
Internet access is required for these assets. You can use another browser-playable
HTTP(S) video URL; the stream must allow browser access. Chrome with the sample HLS
stream exposes rendition measurements. Native HLS can leave them unavailable.

The event counter on the player shows events emitted by the SDK. Use the dashboard
to confirm receipt. The default viewer ID is fixed for this demo; it does not count
individual people.

## Data and configuration

Everything persists in `demo-data/` beside the bundle's `package.json`:

- `analytics.duckdb` and its associated files contain events.
- `keys.json` contains the collector's project/key registry.
- `demo.json` contains private demo credentials and the sample date/seed marker.
- `collector.log` contains collector diagnostics.

Read tokens stay on the dashboard backend. Only the Player test ingest key is
sent to the player page. Never share your used `demo-data` directory. Share the
clean generated archive; each recipient gets their own keys and database.

The launcher binds to loopback. Anyone who can access its local port can inspect
the demo projects. It is not a hosted or authenticated customer dashboard.
Retention is disabled for these local samples; normal collector defaults are
unchanged. Inherited `COLLECTOR_*` settings are ignored to keep the demo isolated.

To start fresh, stop the demo and move `demo-data/` to a backup location, then
start again. Alternatively use `--data-dir /path/to/a/new/demo-directory`.
The launcher refuses an existing directory without its demo marker.
Do not run two demos against the same data directory.

## Run from the repository

Install Node.js 24, then run:

```sh
npm ci
npm run demo -- --collector /absolute/path/to/chunkify-analytics-collector
```

You can also place a matching executable in `bin/`. Without `--collector`, the
launcher checks `bin/`, `target/release/`, then `target/debug/`. Developers can
build it with `cargo build --locked`; the first build compiles embedded DuckDB and
requires Rust and a C++ compiler. No binary is downloaded automatically.

## Prepare a clean bundle for a coworker

With dependencies installed and a native executable built for the recipient's
OS and architecture:

```sh
npm run demo:pack -- --collector /absolute/path/to/chunkify-analytics-collector
```

The archive appears in `dist/chunkify-analytics-demo-<platform>-<architecture>.tar.gz`.
Build and package on a matching system; the packer labels the archive using the
current machine. It copies only demo source, the SDK dependency and the executable.
It never includes databases, local credentials or the rest of `node_modules`.

The Node launcher is portable, but the Rust executable is platform-specific.
The Apple Silicon macOS flow is verified. Linux has a launcher and lifecycle CI
coverage. Windows packaging and a build matrix for distributed binaries are future
work; this demo does not claim to provide those binaries yet.
