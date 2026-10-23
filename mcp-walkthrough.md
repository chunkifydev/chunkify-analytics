# MCP dashboard acceptance — 2026-10-20

## Result

Passed the first-release dashboard-building walkthrough with Codex CLI 0.160.0.
The AI client discovered the collector through MCP, queried two isolated
projects and generated a runnable Node.js backend plus vanilla HTML dashboard.
Browser checks then compared displayed values with known fixtures and direct
HTTP responses. This completes the manual MCP dashboard acceptance item in
[issue #381](https://github.com/chunkifydev/chunkify/issues/381).

This is a development acceptance demo, not a built-in dashboard shipped with
the collector. The separate [administrator and Docker acceptance](administrator-walkthrough.md)
passed on 2026-10-21. The [recommended configuration](configuration.md) is documented; capacity validation
remains pending.

## Environment and method

- macOS with Docker Desktop; collector limited to two CPUs and 1 GiB RAM.
- Source baseline: `7b546bb404db1c7ffb2588c4be61efb3d53dcb17`.
- Existing collector image: `sha256:38dff245041d410fe55b0c04b4528b285b40674216b4f8223cdc4b63b62a953b`.
  Production Rust files matched its recorded source manifest. The later lookup
  regression additions were test-only. No rebuild was needed for this walkthrough.
- Direct ingestion, one DuckDB thread, retention disabled for this tiny fixed
  fixture. This configuration is not a capacity recommendation.
- Two independent stdio companions, launched with `docker exec -i`, each using
  one project's private read token. No database mount was given to the AI client.
- Temporary per-run Codex configuration, workspace-write sandbox, ephemeral
  session, and no edits to global MCP configuration. Tokens stayed outside the
  prompt and were supplied to companion processes privately.
- The client used all four tools: `describe_api`, `get_summary`,
  `list_video_metrics`, and `list_events`. All 16 MCP calls succeeded, including
  a continuation page for the live project's video metrics.
- The generated dashboard uses a backend with a fixed project/route allowlist.
  Browser requests go to that backend; it supplies the collector read token.
  There is no hosted dashboard user authentication in this local demo.

## Fixture and expected results

The live project contains 17 events for six views; staging contains six events
for two views. Some event IDs, view IDs and viewer IDs overlap between projects.
The live fixture includes one anonymous, unfinished view whose latest snapshot
contributes playing time, and viewers who watched more than one video.

| Project/filter | Views | Unique supplied viewer IDs | Playing time (ms) |
| --- | ---: | ---: | ---: |
| live, all | 6 | 3 | 525,000 |
| live, `video_id=intro` | 2 | 2 | 180,000 |
| live, October 19 UTC | 2 | 2 | 240,000 |
| staging, all | 2 | 1 | 18,000 |
| staging, `video_id=intro` | 1 | 1 | 7,000 |
| staging, October 19 UTC | 1 | 1 | 7,000 |

The date window is `2026-10-19T00:00:00Z` inclusive through
`2026-10-20T00:00:00Z` exclusive. Global unique viewers come from the summary
endpoint: summing the live per-video unique counts would incorrectly give five.
The dashboard displays playing hours as milliseconds divided by 3,600,000 and
also exposes the source millisecond total for comparison.

## Verification

- Four generated backend tests passed, covering fixed routes/project selection,
  filter forwarding, pagination, sanitized failures and bounded event samples.
  JavaScript syntax checks passed.
- Eleven direct HTTP checks passed: both projects' totals and filters, rejection
  of a cursor from the other project, invalid read credentials, and refusal to
  use an ingest key for reads. These checks passed again after the container was
  replaced using the same volume and credentials.
- Sixteen browser/API checks passed: displayed values and filters, two-row video
  pagination without changing global totals, switching projects, an empty third
  project, unsupported routes/projects, private read-token absence from captured
  browser traffic, and allowed/disallowed browser-ingestion origins.
- No browser JavaScript errors were observed. The rendered dashboard was
  inspected at a 1440-pixel viewport.
- Source review confirmed data is inserted with `textContent`, collector URLs
  and read routes are fixed by the backend, redirects are refused, error bodies
  do not expose upstream details, and private files are not served.

The recent-event panel scans at most 20 pages of 100 records, then retains five
sample rows. It explicitly reports incomplete scans; it is not a general
newest-events API. Totals never depend on that sample or on video-page sums.

## Reproduce

1. Start a collector and use its offline administrator CLI to create `live` and
   `staging` projects. Keep their ingest and read credentials separate.
2. Ingest starts, cumulative snapshots and ends with known totals, including
   shared IDs across projects, one anonymous view and one unfinished view.
3. Connect two companion entries to a real AI coding client, each configured
   with the same collector URL and its own project read token. Follow
   [MCP setup](mcp.md) and use the dashboard-building prompt there.
4. Ask the client to inspect the API and metrics, query both projects, follow a
   video cursor, and generate a backend plus dashboard without embedding read
   tokens in frontend files. The walkthrough used Node built-ins and vanilla JS.
5. Run the dashboard locally with read tokens in backend environment variables.
   Compare all-project, exact-video and date-window totals with the collector.
   Exercise pagination and verify that global totals do not become page totals.
6. Inspect browser requests, responses and static files for private tokens.
   Reject cross-project cursors and confirm ingest keys cannot read analytics.

The local run kept its generated demo, fixture, client transcript, screenshot
and assertions outside the repository under a temporary walkthrough directory.
No real customer events or credentials were used. A separate, initially empty
`player` project was added for hands-on SDK testing after fixture verification;
its browser ingest key is separate from backend read tokens. That user testing
is not claimed as completed by this record.

## Limits

The walkthrough validates one installed AI client and the current collector
image. It does not certify Claude Code, arbitrary MCP clients, remote MCP/OAuth,
a production dashboard authentication model, or high-load performance. The
container replacement check provided persistence evidence. The later
[administrator walkthrough](administrator-walkthrough.md) completed the separate
schema-upgrade, credential-lifecycle and backup/restore checks. CSV export and S3
archiving remain deferred.
