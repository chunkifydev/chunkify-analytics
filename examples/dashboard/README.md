# Local analytics dashboard demo

A small vanilla HTML/JavaScript dashboard with a Node backend. It demonstrates
how an AI can use the collector's MCP contract and data to build a dashboard.
At runtime, the backend reads the HTTP API; the browser never receives a read token.

## Run

Use Node 24 and a collector version with the summary, quality, engagement and
UTC timeseries routes. From the repository root, set the collector URL and one
or more private read tokens in your shell or a private environment file:

```sh
export COLLECTOR_URL=http://127.0.0.1:4174
export CHUNKIFY_PLAYER_READ_TOKEN=YOUR_PROJECT_READ_TOKEN
node examples/dashboard/server.cjs
```

Open `http://127.0.0.1:3000`. `PORT` can select another local port.
The project selector only lists configured tokens:

| Environment variable | Label |
| --- | --- |
| `CHUNKIFY_DEMO_READ_TOKEN` | Demo data, marked synthetic |
| `CHUNKIFY_PLAYER_READ_TOKEN` | Player test |
| `CHUNKIFY_LIVE_READ_TOKEN` | Live |
| `CHUNKIFY_STAGING_READ_TOKEN` | Staging |

Each token belongs to the project you created in the collector. These labels
are dashboard configuration, not collector project overrides. Keep tokens out
of the source, Git, screenshots and browser code.

This example binds to loopback and has no dashboard login. Anyone with access
to this local port can read every configured project. Do not expose or tunnel
it as a customer dashboard without adding user authentication and authorization.
Requests from foreign hosts or browser origins are refused.

## Demo flow

1. **Overview:** views, unique supplied viewer IDs, total and average watch time,
   a daily chart and a paginated video table. Select Explore on a video to filter
   every page to that video.
2. **Playback quality:** startup median/p95, buffering ratio, fatal-error rate,
   sample coverage and underlying counts/durations.
3. **Engagement:** average watch time, reached-end count/rate and a watch-time chart.
4. **Events:** the last 10 received matching events. Open a row for measurements,
   browser/device context and the full stored JSON.

Filters apply across pages after pressing Refresh. Changing pages does not fetch
again. There is no polling. Dates are whole UTC days; Through includes the selected
day and is sent to the API as the next day's exclusive midnight. Browser, OS,
viewer and video IDs are exact case-sensitive matches. Advanced filters can be
combined with the date and video selection. Date ranges are capped at 1,000 days.

Chart data is available as a table on Overview. Unavailable measurements stay
unavailable; they are not converted to zero. Summary totals cover all matching
views, regardless of the current video page. Unique IDs cannot be added across
videos or days. Aggregate filters select view starts; Events filters select the
events' own occurrence time and context. The latest-event scan stops at 2,000
records and asks for narrower filters if it cannot determine the latest ten.

Reached-end means the final event reports `reason=ended`, not that the whole
video was watched. Startup is attempt-to-first-playing, not first-frame timing.
Watch time is cumulative playing time attributed to the view's start, including
playback outside the selected date window. Missing delivery can undercount it.
Separate API calls may observe different ingestion watermarks during live traffic.

## Optional synthetic data

Use a **separate demo project**. Stop the collector to create it with
`project-create demo "Dashboard demo"`, save the returned credentials privately,
and restart the collector. See the root README for native and Docker commands.
Set that project's read token as `CHUNKIFY_DEMO_READ_TOKEN` above.

The optional seed script uses the repository's installed analytics SDK to create
event envelopes. Run `npm ci` if dependencies are not already installed. Then:

```sh
export CHUNKIFY_DEMO_INGEST_KEY=YOUR_DEMO_PROJECT_INGEST_KEY
node examples/dashboard/seed.mjs
unset CHUNKIFY_DEMO_INGEST_KEY
```

It sends 816 synthetic events for 268 views across nine videos, seven days,
91 supplied viewer IDs and mixed browser/device contexts. It takes about 90
seconds at ten events per second. Same-day reruns use the same event IDs and
are deduplicated. Running it on another day creates another dataset; do not
use a production project's ingest key. No real viewer data is sent elsewhere.

## Verification

```sh
node --test examples/dashboard/server.test.cjs
```

Backend tests cover project isolation, exact filter forwarding for every metric,
pagination, bounded latest-event scans, error redaction and local-origin checks.
For MCP parity, call `get_summary`, `get_quality`, `get_engagement` and
`get_timeseries` with the same read token and filters. Compare their `data` with
the dashboard's `/api/…` JSON; only display units and rounding differ in the UI.
The MCP connection instructions are in [mcp.md](../../mcp.md).
