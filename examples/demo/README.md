# Local collector and dashboard demo

For coworkers working in this repository, build the collector locally and follow
the launch commands below. It starts the same dashboard, sample data and player as the shared
archive. The source workflow below is the recommended way to try the demo together.
Docker is not required. If you already received a prebuilt archive, see
[Start a shared bundle](#start-a-shared-bundle).

## Run from the repository

### 1. Install the development tools on macOS

These tools are installed once. Skip any that are already installed.

- **Node.js 24** runs the launcher and dashboard. Install version 24 from
  [Node.js downloads](https://nodejs.org/en/download). npm is included. If you
  already use nvm, `nvm install 24` and `nvm use 24` select the required version.
- **Apple Command Line Tools** provide the C++ compiler used to build embedded
  DuckDB. The command below opens Apple's installation dialog. Full Xcode is not
  needed. See [Apple's instructions](https://developer.apple.com/documentation/xcode/installing-the-command-line-tools).

```sh
xcode-select --install
```

- **Rust and Cargo** compile the collector and manage its dependencies. Use the
  [official Rust installer](https://rust-lang.org/tools/install/), then follow
  its prompts:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Reopen your terminal after installing Rust so it can find `cargo`. Check the tools:

```sh
node --version
npm --version
cargo --version
rustc --version
clang++ --version
```

Node must report `v24.x`. The repository's CI uses Rust 1.99.0. If your Rust
installation is older, `rustup update stable` updates the stable toolchain.

### 2. Get the demo source

If you do not already have a local checkout, clone the repository using your
usual GitHub authentication:

```sh
git clone https://github.com/chunkifydev/chunkify-analytics.git
cd chunkify-analytics
```

If you already have a checkout, open a terminal in that directory instead.
Get the latest version of `main`:

```sh
git fetch origin
git switch main
git pull --ff-only
```

### 3. Install dependencies and build the collector

Run these commands from the repository root:

```sh
npm ci
cargo build --locked
```

The first Rust build compiles embedded DuckDB and can take several minutes.
Cargo writes the native collector to `target/debug/chunkify-analytics-collector`.
Build output and dependencies use more disk space than the small demo dataset.
There is no separate DuckDB installation or database server to configure.

### 4. Start the complete demo

```sh
npm run demo -- --collector target/debug/chunkify-analytics-collector
```

Keep the terminal open. The launcher automatically:

1. Starts the collector you just built in `target/debug`.
2. Creates separate **Demo data** and **Player test** projects and their keys.
3. Generates **816 synthetic events for 268 views across nine videos** on first launch.
4. Starts the dashboard and opens it in your browser, normally at
   `http://127.0.0.1:8787`.

The player page is already configured to send to your local collector through
`createChunkifyAdapter`. You do not need to enter tokens or run a seed command.
The data is generated on your computer; no database download is needed.

If port 8787 is already in use, select a free port:

```sh
npm run demo -- --collector target/debug/chunkify-analytics-collector --port 0
```

`--no-open` prints the URLs without opening a browser. The collector uses its own
available loopback port. Internet access is needed for the player libraries.
Paste your own HTTP(S) video URL in the player page to test real playback.

### 5. Stop, restart or update

Press **Ctrl+C** in the terminal to stop the dashboard and collector. To start
again, run:

```sh
npm run demo -- --collector target/debug/chunkify-analytics-collector
```

Subsequent launches reuse your projects, keys and database without adding the
samples again. Interrupted sample loading resumes using the same event IDs.

After pulling source updates, refresh dependencies and rebuild:

```sh
git pull --ff-only
npm ci
cargo build --locked
npm run demo -- --collector target/debug/chunkify-analytics-collector
```

If you intentionally use an existing collector executable, pass
`npm run demo -- --collector /absolute/path/to/chunkify-analytics-collector`.
The source commands above select `target/debug` explicitly. Without `--collector`,
the launcher checks `bin/`, `target/release/`, then
`target/debug/`; keep any executable in those earlier locations up to date too.

## Show what the collector can do

1. **Demo data:** explore Overview, Playback quality and Engagement. Filter by
   video, browser or device. Open an event to see resolution, selected rendition,
   time per rendition, browser context and the raw stored JSON.
2. **Try the player:** follow the link in the dashboard. The video URL field starts
   empty with the placeholder `put your video link here.mp4`. Paste a full HTTP(S)
   URL for your MP4 or HLS video, choose a video ID, and press **Start demo view**.
   `createChunkifyAdapter` is already configured for your local collector.
3. **Player test analytics:** open this link from the player page and press
   **Refresh**. After five seconds of active playback, a snapshot updates watch
   time. Seek, pause or change quality to produce more events.
4. Press **Finish view**, then refresh to inspect the final summary. Pausing
   alone does not finish a view. **Start demo view** starts another monitored view.
5. Change the video URL and video ID to test another video, or copy the adapter
   example into your own player page. Local pages on port 4173 or 5173 are allowed by the demo.

Sample dates remain selected for Demo data even when you reopen the demo later.
Player test defaults to the last seven days. **Sample dates** restores the seeded
period. The dashboard uses manual refresh, with no polling.

The player and analytics SDK load from jsDelivr using pinned versions and require
internet access. Supply a browser-playable HTTP(S) video URL that allows browser
access. Rendition measurements require a supported playback engine and an adaptive
stream; they can be unavailable with MP4 or native HLS playback.

The event counter on the player shows events emitted by the SDK. Use the dashboard
to confirm receipt. The default viewer ID is fixed for this demo; it does not count
individual people.

## Data and configuration

Everything persists in `demo-data/` at the repository root, or beside
`package.json` when using a shared bundle:

- `analytics.duckdb` and its associated files contain events.
- `keys.json` contains the collector's project/key registry.
- `demo.json` contains private demo credentials and the sample date/seed marker.
- `collector.log` contains collector diagnostics.

Read tokens stay on the dashboard backend. Only the Player test ingest key is
sent to the player page. Each local checkout gets its own keys and database.
`demo-data/` is ignored by Git; keep it private when sharing the demo.

The launcher binds to loopback. Anyone who can access its local port can inspect
the demo projects. It is not a hosted or authenticated customer dashboard.
Retention is disabled for these local samples; normal collector defaults are
unchanged. Inherited `COLLECTOR_*` settings are ignored to keep the demo isolated.

To start fresh, stop the demo and move `demo-data/` to a backup location, then
start again. Alternatively use `--data-dir /path/to/a/new/demo-directory`.
The launcher refuses an existing directory without its demo marker.
Do not run two demos against the same data directory.

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
