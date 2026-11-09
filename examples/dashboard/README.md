# Local analytics dashboard demo

A local dashboard with a Node backend.
At runtime, the backend reads the HTTP API; the browser never receives a secret key.

For automatic native setup with sample data and a preconfigured player, use the
[local demo launcher](../demo/README.md). The manual setup below connects this
dashboard to an existing analytics server.

## Run

Use Node.js 24 and a matching analytics server. From the repository root, set the
analytics server URL and a private secret key:

```sh
export CHUNKIFY_URL=http://127.0.0.1:4174
export CHUNKIFY_PLAYER_SECRET_KEY=YOUR_SECRET_KEY
node examples/dashboard/server.cjs
```

Open `http://127.0.0.1:3000`. `PORT` can select another local port.
The project selector only lists configured tokens:

| Environment variable | Label |
| --- | --- |
| `CHUNKIFY_DEMO_SECRET_KEY` | Demo data, marked synthetic |
| `CHUNKIFY_PLAYER_SECRET_KEY` | Player test |
| `CHUNKIFY_LIVE_SECRET_KEY` | Live |
| `CHUNKIFY_STAGING_SECRET_KEY` | Staging |

Each token belongs to the project you created in the analytics server. These labels
are dashboard configuration, not analytics server project overrides. Keep tokens out
of the source, Git, screenshots and browser code.

This example binds to loopback and has no dashboard login. Anyone with access
to this local port can read every configured project. Do not expose or tunnel
it as a customer dashboard without adding user authentication and authorization.
Requests from foreign hosts or browser origins are refused.

## Explore analytics

The dashboard shows totals, daily metrics, videos, playback quality, engagement
and the latest ten events. Select a video to filter every page to it. Press
Refresh after changing filters; the dashboard does not poll.

Dates are UTC days. Browser, OS, viewer and video filters match exact values.
Summary filters select view starts; event filters select each event's occurrence
time and context. Totals cover all matches even when the video table has more pages.
Unique viewer counts cannot be added across videos or days.

Null measurements stay unavailable. Watch time includes playback after the
selected start window. Reached-end counts do not prove the whole video was
watched. See [metric definitions](../../docs/api.md#metric-definitions).

## Optional synthetic data

Use a **separate demo project**. Create it with
`project create "Dashboard demo"`, save the returned credentials privately,
and restart the analytics server. See the root README for native and Docker commands.
Set that project's secret key as `CHUNKIFY_DEMO_SECRET_KEY` above.

The optional seed script uses the repository's installed analytics SDK to create
event envelopes. Run `npm ci` if dependencies are not already installed. Then:

```sh
export CHUNKIFY_DEMO_PUBLIC_KEY=YOUR_DEMO_PROJECT_PUBLIC_KEY
node examples/dashboard/seed.mjs
unset CHUNKIFY_DEMO_PUBLIC_KEY
```

The script sends 816 synthetic events for 268 views across nine videos and seven
days. These are fabricated measurements. Use a demo project's public key.

Same-day reruns reuse event IDs, which the analytics server ignores as
duplicates. A new day produces another dataset. Use a fresh project to replace
sample fixtures.

## Verification

```sh
node --test examples/dashboard/*.test.*
```

For MCP comparisons, use the same secret key and filters with `get_metrics`
(`kind=summary`, `quality` or `engagement`) and `get_timeseries`.
See [MCP setup](../../docs/mcp.md).
