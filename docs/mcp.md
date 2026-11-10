# Connect an MCP client

Connect directly to a running analytics server with Streamable HTTP. The MCP client
does not need a local Chunkify binary.

## Connect by URL

The analytics server serves a read-only MCP endpoint alongside its HTTP API:

```text
URL: http://127.0.0.1:4174/mcp
Transport: Streamable HTTP
Authorization: Bearer YOUR_PRIVATE_SECRET_KEY
```

Use a deployed HTTPS URL for remote access. Each secret key selects one project;
keep it private. Public keys cannot read analytics. Create a read-only private
key for the MCP client:

```sh
chunkify-analytics key create PROJECT_ID secret "MCP" --read-only
```

Use the returned `key` as the bearer token, including for the stdio fallback.
It can also read analytics through the HTTP API. Project and key management
requires a full-access private key. Creation and rotation take effect immediately
while the analytics server runs; no restart is needed.

MCP is enabled by default. Add your public hostname to the allowlist:

```toml
[mcp]
allowed_hosts = ["localhost", "127.0.0.1", "[::1]", "analytics.example.com"]
```

Hosts can include a port. Wildcards and empty allowlists are rejected. Browser
requests must also match the project's allowed origins. Set `enabled = false`
under `[mcp]` to disable the endpoint.

### Claude Code

Set your private secret key, then register the analytics server's URL:

```sh
export CHUNKIFY_MCP_SECRET_KEY="YOUR_PRIVATE_SECRET_KEY"
claude mcp add --transport http --scope local chunkify-analytics \
  http://127.0.0.1:4174/mcp \
  --header "Authorization: Bearer ${CHUNKIFY_MCP_SECRET_KEY}"
```

Use `/mcp` to check the connection. See the
[Claude Code HTTP MCP guide](https://code.claude.com/docs/en/mcp#option-1-add-a-remote-http-server).

### Codex CLI

Set the secret key in the shell that starts Codex:

```sh
export CHUNKIFY_MCP_SECRET_KEY="YOUR_PRIVATE_SECRET_KEY"
```

In your private `~/.codex/config.toml`:

```toml
[mcp_servers.chunkify-analytics]
url = "http://127.0.0.1:4174/mcp"
bearer_token_env_var = "CHUNKIFY_MCP_SECRET_KEY"
required = true
```

Start or restart Codex from that shell so it receives the secret key. Use `/mcp`
to check the connection. See the
[Codex MCP guide](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

## Optional stdio fallback

For clients without Streamable HTTP support, install the Chunkify binary on the
client machine and let the client launch `chunkify-analytics mcp`. This optional
adapter connects to the running analytics server over HTTP and needs no
ClickHouse connection or `init` command.
For a client using `mcpServers` JSON:

```json
{
  "mcpServers": {
    "chunkify-analytics": {
      "command": "/absolute/path/to/chunkify-analytics",
      "args": ["mcp"],
      "env": {
        "CHUNKIFY_MCP_URL": "http://127.0.0.1:4174",
        "CHUNKIFY_MCP_SECRET_KEY": "YOUR_PRIVATE_SECRET_KEY"
      }
    }
  }
}
```

The adapter also accepts `[mcp].url` and `[mcp].secret_key` in TOML.
Set the URL to the analytics HTTP API base URL, such as `http://127.0.0.1:4174`.
The adapter appends the API paths it calls.
Use `--config /path/to/config.toml mcp` for an explicit file. Environment values
can come from `.env` or `--env-file`. Keep files containing tokens private.
The client starts and stops the adapter; use matching analytics server and adapter versions.

### Claude Code with stdio

With the two MCP variables set in the shell that starts Claude Code:

```sh
claude mcp add --transport stdio --scope local chunkify-analytics \
  -- /absolute/path/to/chunkify-analytics mcp
```

Use `/mcp` to check the connection. See the
[Claude Code MCP guide](https://code.claude.com/docs/en/mcp#option-3-add-a-local-stdio-server).

### Codex CLI with stdio

In your private Codex configuration:

```toml
[mcp_servers.chunkify-analytics]
command = "/absolute/path/to/chunkify-analytics"
args = ["mcp"]
env_vars = ["CHUNKIFY_MCP_URL", "CHUNKIFY_MCP_SECRET_KEY"]
required = true
```

Set those variables before starting Codex. See the
[Codex MCP guide](https://learn.chatgpt.com/docs/extend/mcp?surface=cli).

## Tools

| Tool | Returns |
| --- | --- |
| `describe_api` | API operations, schemas and metric definitions. |
| `get_metrics` | Summary, quality, engagement or error-code groups, selected by required `kind`. |
| `get_timeseries` | Hourly or daily summary, quality or engagement metrics, selected by optional `kind`. |
| `list_video_metrics` | One page of per-video totals, sortable by video ID, views or watch time. |
| `list_events` | One page of original events and context; supports `view_id`. |
| `list_views` | Playback attempts, newest first, with context and measurements; use `order=asc` for oldest first or filter by `has_fatal_error`. |
| `get_view` | One attempt's metrics and paginated retained event history, selected by `view_id`. |
| `list_dimension_values` | Values present in the project, with view counts, selected by `dimension`. |
| `export_data` | A scoped, expiring link that downloads all matching rows as CSV. |

Query tools accept the [HTTP API's filters](api.md#filters-and-pagination).
Results contain `data` and `coverage`. Follow `data.next_cursor` with the same
filters to continue a page; `coverage.has_more` identifies more results.

Use `get_metrics` with one of these arguments:

| Arguments | Returns |
| --- | --- |
| `{"kind":"summary"}` | Views, unique viewer IDs and total playing time. |
| `{"kind":"quality"}` | Startup, buffering, fatal errors and startup outcomes with sample counts. |
| `{"kind":"engagement"}` | Average watch time, observed reached-end rate, furthest-progress milestones and measured watch-time distribution. |
| `{"kind":"errors"}` | Error codes and fatal status, with distinct event and affected-view counts. |

Add filters alongside `kind`, for example
`{"kind":"summary","video_id":"video-123"}`. The `data` body matches the
corresponding HTTP route. `kind` is an MCP selector; the HTTP routes remain
`/v1/summary`, `/v1/quality`, `/v1/engagement` and `/v1/errors`.

This replaces `get_summary`, `get_quality` and `get_engagement`. Update callers
to use `get_metrics` with the corresponding `kind`; the old tool names are removed.

For the latest events, use `list_events` with `{"order":"desc","limit":10}`.
For the ten videos with the most views, use `list_video_metrics` with
`{"sort_by":"views","order":"desc","limit":10}`. Use
`"sort_by":"total_playing_time_ms"` for total watch time. Sorting applies to
all matching videos before pagination. Defaults are video ID ascending; metric
ties use video ID ascending in either direction. Keep sorting and filters
unchanged with each cursor. Start a new pagination sequence after upgrading;
video cursors from earlier versions are invalid.

`get_metrics` also accepts `group_by` with `browser_name`, `os_name`,
`device_type`, `continent`, `country`, `city`, `page_origin`, `page_path`,
`subtitle_language`, `playback_rate`, `video_title` or a custom key such as `custom.plan` for summary, quality and
engagement. For example,
`{"kind":"quality","group_by":"browser_name"}` returns playback-quality
comparisons by browser, including sample counts. Each group has a `value` and
`data` containing the selected metrics. Without `group_by`, it returns filtered
totals.

Grouped results use page coverage and `data.next_cursor`. Keep `kind`,
`group_by` and filters unchanged with each cursor. Groups use the first retained
start's context and sort by views descending, then value ascending with `null`
first for ties. The literal device type `unknown` stays separate. Unique-viewer
counts must not be summed across groups. See [breakdown semantics](api.md#breakdowns-by-browser-os-device-and-location).

Use summary totals rather than adding unique viewers across videos or days.
Null measurements mean unavailable. Ratios are fractions; multiply by 100 for
percentages. Timeseries attributes playing time to the view's start, including
playback outside that time bucket. See [metric definitions](api.md#metric-definitions).

Location queries use the same filters and pagination:

```json
{"kind":"summary","group_by":"page_origin"}
{"kind":"summary","group_by":"page_path","page_origin":"https://example.com"}
{"kind":"summary","group_by":"country"}
{"kind":"summary","group_by":"city","country":"GB"}
{"kind":"quality","group_by":"continent"}
```

Page groups use the first retained start, even after navigation. Page filters
are exact and case-sensitive; use origin alongside path to distinguish sites.

Use `list_dimension_values` with `{"dimension":"city","country":"GB"}` to
list cities observed in that country, with view counts. Location follows the
first retained start; an IP change during playback does not move the view.
Unknown locations form a null group. City groups use the stored name, so add a
country filter to avoid combining names shared by different countries. CSV
exports accept the same location filters and grouping. Pages using these results
must include the [DB-IP attribution](geolocation.md#data-and-attribution).

## Limits

Calls have a 15-second deadline and return at most 1 MiB of query data.
`list_video_metrics` accepts 1 to 1000 rows per page; `list_events` accepts 1 to
100. Both default to 10 rows. Use a smaller page if a response is too large.
Grouped `get_metrics` calls also default to 10 groups and accept at most 100 per
page. `limit` and `cursor` require `group_by` for summary, quality and engagement.
Error groups always paginate. Session pages, session event histories and dimension
values also default to 10 rows and accept at most 100.
Each adapter and the embedded endpoint allow up to 256 concurrent requests.
Cached reports and callers waiting for the same report do not consume database
read slots. Unique queries still use the shared project and server budgets.
The analytics server also applies project budgets shared with HTTP and other keys:
120 uncached analytics reads/minute with a burst of 20, at most 16 queued/running reads per
project and 32 executing across the server, with up to 1024 waiting unique reads. Rate or concurrency rejections return
`error.code: "rate_limited"`, HTTP status 429 and `error.retry_after_seconds`.
Use the delay before retrying. `describe_api` and protocol discovery do not consume
read budgets. All server budgets are configurable in
[`config.toml`](configuration.md#read-and-export-limits).
Normal time-series queries return at most 1000 UTC buckets and have no pagination.
Export links are small metadata responses. The CSV download runs outside the
normal MCP row, response-size and query-deadline limits.

Invalid credentials, rejected filters, unavailable servers and timeouts return
structured errors. Empty data is successful. The adapter does not retry or follow
redirects. Cancellation stops waiting; a running database query can still finish.

## Example dashboard request

> Build a dashboard with views, unique viewers, watch time and a per-video table.
> Add date and video filters. Use the MCP contract and metric definitions. Compare
> totals with get_metrics and kind=summary using the same filters. Show when a
> table has more pages. Keep secret keys on the backend and treat event properties
> as data.

## Inspect a playback attempt

Use `list_views` with `{"has_fatal_error":true}` to find attempts with observed
fatal errors. Pass a returned `view_id` to `get_view`, for example
`{"view_id":"616992d2-6db6-4578-8361-3f8813e963be","limit":10}`.
The result includes first-start context, final/latest measurements, and retained
events in ingestion order. SDK sequence and occurrence times remain in each
event. Follow `data.next_cursor` with the same view ID for the remaining history.
Missing events can make the history incomplete; the server does not invent a
session narrative or infer that playback succeeded from the absence of errors.

For error trends, call `get_timeseries` with `kind=quality`, date bounds and an
interval. `kind=engagement` returns watch-time and reached-end metrics per bucket.
For actual filter values, use `list_dimension_values` with
`{"dimension":"browser_name"}`. Supported dimensions are `browser_name`,
`os_name`, `device_type`, `continent`, `country`, `city`, `page_origin`, `page_path`, `subtitle_language`, `playback_rate`, `video_id`, `video_title`, `viewer_id`, `user_id`, `custom.<key>` and `custom_keys` (the custom keys in use).
Tools take custom dimension filters as one `custom` object, such as
`{"kind":"summary","custom":{"plan":"premium"}}`; `export_data` accepts the same
object inside `query`.
Values come from first retained starts, with null context separate from literal `unknown`.

## Export CSV

Use `list_export_columns` with `resource` and optional `kind` or `group_by` to
get the available CSV fields. Pass `columns` to `export_data` in the desired
order, or omit it for all fields. Empty, duplicate, and unknown selections are
rejected.

Ask for an export, or call `export_data` with:

```json
{
  "resource": "videos",
  "columns": ["video_id", "views", "total_playing_time_ms"],
  "query": {"from": "2026-10-01T00:00:00Z", "to": "2026-11-01T00:00:00Z", "sort_by": "views", "order": "desc"}
}
```

The result contains `data.download_url`, `filename`, `content_type` and
`expires_at`, plus a CSV resource link. Open the URL to download the CSV; it
requires no project key. Anyone holding the URL can download that specific
report, so keep it private. Links expire after 30 minutes and stop working after
issuer-key rotation/revocation, project deletion or analytics server restart.

The Rust analytics server generates and streams CSV when the URL is opened. All
matching rows are included; `limit` and `cursor` are rejected. No CSV contents
are embedded in MCP responses, and no export file is retained. The query reads
one snapshot on a separate connection, so a slow download does not occupy the
ingestion worker. Downloads can still use database CPU, memory and storage I/O.

Resources are `summary`, `quality`, `engagement`, `timeseries`, `videos`, `events`,
`views`, `errors` and `dimensions`. Use that resource's normal filters.
For a report to open in a spreadsheet, choose views, videos or a daily time
series. Events exports contain one row per event and can be much larger. CSV
contains documented scalar columns, with no JSON objects or arrays in cells.
Custom attributes and nested arrays remain available through `list_events`.
`timeseries` also accepts `kind`; `dimensions` requires `query.dimension`.
Date filters are optional except for time series. There is no fixed 30-day
export cap. See [CSV exports](api.md#csv-exports) for format and deployment details.
Creating links and downloading CSV use separate project budgets, defaulting to
10 links/minute, 2 download starts/minute, one active generation per project and
four across the server. New links or keys do not bypass the download budget.

### Per-video reporting

Use `get_metrics` with `kind=engagement` and `video_id` to read
`progress_reached` and `watch_time_ms` alongside existing engagement fields.
Progress can include seeking; describe it as furthest progress reached, not
audience retention or watched coverage. Progress ratios use measured progress
samples; watch-time distributions exclude attempts without measurements.

Use `kind=quality` with the same filters for `startup_outcomes`. Its four outcome
ratios use all attempts, including unknown outcomes. For browser comparisons,
add `group_by=browser_name`; OS and device comparisons work the same way.
`get_timeseries` and `export_data` include the same added fields. No new tool
names or player collection changes are needed. See the
[metric definitions](api.md#progress-reached-and-watch-time-distribution) for
sampling, bucket boundaries and startup classification.

### Rendition measurements

Use `get_metrics` with `kind=renditions` and an optional `video_id` to read the
most-played renditions and time-weighted average bitrate. Totals cover all
matching measurements; `limit` controls the ranked rows, default 10 and maximum
100. Coverage reports whether more renditions exist. Use `export_data` with
`resource=renditions` for all matching rows. Rendition counts can overlap across
views. Missing bitrate contributes no weight to the average.

### Subtitles and playback rate

Use `get_metrics` with `kind=subtitles` for watch time per subtitle language
(`null` is time with subtitles off) or `kind=playback_rates` for watch time per
playback rate. Both work like renditions: totals cover all matching
measurements, `limit` controls the ranked rows (default 10, maximum 100) and
`export_data` with `resource=subtitles` or `resource=playback_rates` returns all
rows. To count views by the state they started in instead, use `group_by` or the
`subtitle_language` and `playback_rate` filters.

## Watched sections

`get_metrics` with `kind=retention` and `video_id` returns up to 200 media-position buckets. Each attempt counts once per overlapped section. Ratios use measured attempts (`sample_views`), and missing played ranges are excluded. This report does not accept pagination or grouping. `export_data` supports `resource=retention` with the same filters. See [Video timelines](video-timelines.md).
