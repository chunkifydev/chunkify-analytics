# HTTP API reference

See the [OpenAPI specification](../openapi.json) for request and response schemas.

| Method and path | Authorization | Response |
| --- | --- | --- |
| `GET /healthz` | None | `{"status":"ok"}` after a database check, or 503 if unavailable. |
| `POST /v1/events` | `X-Chunkify-Ingest-Key` | 201 with `{"accepted":true,"duplicate":false}` after storage completes; local storage returns 200 with `{"accepted":false,"duplicate":true}` for an existing event. |
| `POST /v1/projects` | `Authorization: Bearer CHUNKIFY_ADMIN_KEY` | Generated project ID and initial public/secret keys, with optional retention. |
| `GET /v1/projects/{project_id}/keys` | `Authorization: Bearer CHUNKIFY_ADMIN_KEY` | Key IDs, names and permissions for recovery, without key values. |
| `POST /v1/projects/{project_id}/keys/{key_id}/rotate` | `Authorization: Bearer CHUNKIFY_ADMIN_KEY` | Replacement for one existing key, without requiring a project secret. Other keys are unchanged. |
| `DELETE /v1/project` | Full-access `Authorization: Bearer SECRET_KEY` | Delete the project and revoke all its keys. Events expire under the existing retention policy. |
| `PATCH /v1/project` | Full-access `Authorization: Bearer SECRET_KEY` | Update the project name, additional origins or retention immediately. |
| `GET /v1/project/keys` | Full-access `Authorization: Bearer SECRET_KEY` | Key IDs, names, permissions and `read_only`, without key values. |
| `POST /v1/project/keys` | Full-access `Authorization: Bearer SECRET_KEY` | Create a public or secret key; its value is returned once. |
| `POST /v1/project/keys/{key_id}/rotate` | Full-access `Authorization: Bearer SECRET_KEY` | New key value with its access level; the previous value stops working immediately. |
| `GET /v1/events` | `Authorization: Bearer SECRET_KEY` | `{"events":[{"project_id":"default","ingest_key_id":"primary-public","received_at":"...","event":{...}}],"next_cursor":null}` |
| `GET /v1/summary` | `Authorization: Bearer SECRET_KEY` | `{"views":3,"unique_viewers":1,"total_playing_time_ms":6500}` |
| `GET /v1/retention` | `Authorization: Bearer SECRET_KEY` | Watched sections along one video, excluding seeks. Requires `video_id`. |
| `GET /v1/engagement` | `Authorization: Bearer SECRET_KEY` | Average playing time per attempt and observed reached-end count/rate. |
| `GET /v1/quality` | `Authorization: Bearer SECRET_KEY` | Startup percentiles, buffering and observed fatal errors, with sample counts. |
| `GET /v1/timeseries` | `Authorization: Bearer SECRET_KEY` | Hourly/daily views, unique viewers and playing time; requires `from`, `to`, `interval`. |
| `GET /v1/videos` | `Authorization: Bearer SECRET_KEY` | `{"videos":[{"video_id":"video-123","views":3,"unique_viewers":1,"total_playing_time_ms":6500}],"next_cursor":null}` |

Native ingestion accepts `Content-Type: application/json` and one complete schema
version 1 event. It validates all seven event shapes emitted by the SDK. Malformed
or invalid payloads return 400; unsupported media types return 415; oversized bodies
return 413. Missing or invalid keys return 401. Disallowed origins return 403.

Only the event ingestion endpoint enables CORS, including preflight for the ingest header.
Read endpoints are intended for the customer's backend and do not enable CORS.
Keys are never accepted in a query string. Responses are marked `no-store`.

Public keys start with `ck_public_`; secret keys start with `ck_secret_`.
Both full-access and read-only private keys can read analytics and use MCP.
Full-access private keys can also update the name, origins or retention, manage
keys and delete their own project. Create a read-only private key through
`POST /v1/project/keys` with `permission: "secret"` and `read_only: true`, or with
`key create PROJECT_ID secret "Dashboard" --read-only`. These keys receive 403
on project and key management routes, including key listings. Key metadata
includes `read_only`, which remains unchanged when rotating a key. Existing
private keys retain full access.

Project creation and key recovery require the separate `CHUNKIFY_ADMIN_KEY`
environment secret. Keep it in your trusted backend, which must authenticate the
user and verify project ownership before requesting recovery. These admin routes
return 404 when the admin key is unset or empty; project credentials receive 401
when they are enabled. Unknown or deleted projects and unknown or revoked keys
also receive 404. Recovery preserves the key ID, type and `read_only` setting.
See [project creation](administration.md#create-projects-through-the-api),
[key recovery](administration.md#recover-a-lost-project-key) and
[live project management](administration.md#live-project-api) for requests.

### Ingestion and storage

`POST /v1/events` is the analytics server's only ingestion endpoint. Use
`createChunkifyAdapter` from `@chunkify/analytics/chunkify` to send events.
Each request contains one complete JSON event. Request batching and gzip are
not supported; the analytics server can still group concurrent requests into database
transactions.

Local storage deduplicates event IDs within each project. The first accepted
payload and context win; later duplicates do not enrich or replace them.
S3 stores every delivery as a separate row, including repeated event IDs, and
returns 201 with `{"accepted":true,"duplicate":false}` on success.

The former `POST /v1/logs` and `GET /v1/ingestion` endpoints have been removed.
Applications using the OTLP adapter with this analytics server must switch to the
Chunkify adapter and `/v1/events`. Remove `server.max_otlp_records` from existing
TOML configuration files; that setting is no longer supported.
Existing events remain readable, including
an optional `otlp` object on events stored by earlier analytics server versions. That
object preserves the original `log_record`, `resource_logs`, and `scope_logs`.
The SDK's OTLP adapter can still send to other telemetry systems.

### Flat event context and location

`GET /v1/events` returns a flat `event_context` alongside the original `event` and
optional legacy `otlp` context. For example:

```json
{
  "schema_version": 1,
  "browser_name": "Firefox",
  "os_name": "Linux",
  "device_type": "desktop",
  "language": "fr-FR",
  "page_origin": "https://example.com",
  "page_path": "/watch/episode",
  "continent": "EU",
  "country": "GB",
  "city": "London"
}
```

The analytics server copies supported scalar or null fields from schema version 1
`event.browser_context`. Browser values are client-reported. Only the collector
supplies `continent`, `country`, and `city`, from a local City MMDB lookup. Country is an ISO
3166-1 alpha-2 code; city is an approximate English name. Each field is null
when unavailable, including local/private addresses. Request IP addresses are
used only during lookup and are never added to events, stored context, or logs.
The Chunkify player adapter also sends `page_origin` and `page_path`, without URL
credentials, query parameters, or fragments. These describe the current document
at each event, including iframe documents and single-page navigation. Retries
keep the original values.
This context schema has its own `schema_version: 1`.

Behind a reverse proxy, configure every trusted hop under `[server]`:

```toml
trusted_proxies = ["10.20.1.2", "2001:db8:abcd::/48"]
```

Trusted proxies must supply the actual client through `X-Forwarded-For`.
The collector selects the first untrusted address from the right. Missing or
invalid forwarding information produces unknown continent, country and city; the event is
still accepted. Other forwarding headers are ignored. A gateway that batches
multiple viewers' events will have one request location.

Local storage keeps the first delivery's location. S3 keeps each delivery's
location separately. Expiration removes location with its event. Arbitrary
caller-supplied event properties remain unchanged; do not send IPs in them.
Previously stored IP context is not retroactively deleted by this change.

Pages displaying or using bundled DB-IP location results must include an
[IP Geolocation by DB-IP](https://db-ip.com) link. See [geolocation](geolocation.md).

### Filters and pagination

Analytics collection queries accept `from`, `to`, `video_id`, `viewer_id`, `view_id`,
`browser_name`, `os_name`, `device_type`, `continent`, `country`, `city`, `page_origin`, and `page_path`. View detail accepts only timeline
pagination; export downloads use their saved query. Timestamps use
RFC3339 with a timezone and are compared in UTC, with `from` inclusive and `to`
exclusive. The raw event endpoint also accepts `event_name`.

Browser and OS names use exact, case-sensitive matching and accept 1 to 64 Unicode
characters. Device type accepts `desktop`, `mobile`, `tablet`, or `unknown`.
Continent accepts `AF`, `AN`, `AS`, `EU`, `NA`, `OC`, or `SA`. Country accepts a
two-letter uppercase code such as `GB`. City accepts an exact, case-sensitive
name of 1 to 256 Unicode characters. Use `country` with `city` to distinguish
names shared by different countries. Location filters use each event's location
on `/v1/events` and the first retained start's location on metric, view,
dimension, rendition, and time-series queries.
Multiple filters combine with AND. Missing, null, and nonstring context values
do not match an explicit filter. `device_type=unknown` matches only a stored
`unknown` value; it does not include events with no device information.

`/v1/events` and `/v1/videos` accept `limit` from 1 to 1000, defaulting to 100.
Follow `next_cursor` until it is null. Keep the same project and filters when following a
cursor. Invalid cursors, changed filters or sorting, unknown parameters, and invalid ranges
return 400. Treat cursors as opaque.

Events are ordered by their server ingestion sequence, oldest first by default.
Use `/v1/events?order=desc&limit=10` for the newest ten received events in one
request. `order` accepts `asc` or `desc` on events and videos, defaulting to `asc`.

Videos accept `sort_by=video_id`, `views`, `unique_viewers`,
`total_playing_time_ms` for watch time, or `playback_score`. The default is `video_id`.
Ranking applies to all matching videos before pagination. Metric ties use
`video_id` ascending in either direction. Missing scores sort last. For example, the ten most watched videos:

```text
GET /v1/videos?sort_by=total_playing_time_ms&order=desc&limit=10
```

Keep the same filters, `sort_by` and `order` with every cursor; changing them
returns 400. Video cursors from earlier versions are invalid; start a new
pagination sequence after upgrading. A pagination sequence is limited to events
already committed at its first page, so later arrivals do not shift pages or change video totals
within that sequence. Start without a cursor to refresh. This is not an indefinite
export snapshot; retention can remove records between requests.

For example, run this from your dashboard backend or terminal, using a local
`SECRET_KEY` environment variable containing the private token:

```sh
curl -H "Authorization: Bearer $SECRET_KEY" \
  'http://127.0.0.1:4174/v1/summary?video_id=video-123'
```

### Metric definitions

- A view is a distinct `view_id` with a retained `view_started` event. It represents
  a playback attempt, including attempts that fail before playback.
- Summary filters select views by the occurrence time, identity and context of
  their first retained start event. Browser/device changes on later events do not
  move the view into another filtered group. Each selected view contributes its entire retained cumulative playing
  time, including a final event that arrives after the selected date range. Raw
  queries instead filter each event's own `occurred_at` and context.
- Playing time uses the final `view_ended` value when present, otherwise the latest
  retained `view_snapshot`, otherwise zero. Records are ranked by SDK sequence,
  then occurrence time and ingestion order. Snapshots are never added together.
- A view without a retained start event is excluded from summaries. Its events
  remain available through raw queries. Duplicate starts for one view use the
  first start by SDK sequence, occurrence time, then ingestion order.
- Unique viewers are distinct non-null supplied `viewer_id` values on selected
  starts. Anonymous views still count as views. This does not identify verified
  people, and per-video viewer counts should not be summed for a global count.
- Final events do not imply video completion. Missing events can make metrics
  incomplete. Client occurrence times and metric values are supplied by the SDK;
  an public key does not verify their authenticity.

### Breakdowns by browser, OS, device and location

Add `group_by` with `browser_name`, `os_name`, `device_type`, `continent`,
`country`, `city`, `page_origin`, or `page_path` to
`GET /v1/summary`, `GET /v1/quality` or `GET /v1/engagement`. The MCP tool
`get_metrics` accepts the same argument, with `kind` selecting `summary`,
`quality` or `engagement`. Without `group_by`, each route returns its
usual totals and rejects `limit` and `cursor`.

Grouping returns one page of groups. Each group's `data` uses the same metric
schema, calculations and sample counts as the route's totals response:

```json
{
  "group_by": "browser_name",
  "groups": [
    {"value": "Chrome", "data": {"views": 2, "unique_viewers": 1, "total_playing_time_ms": 5000}}
  ],
  "next_cursor": null
}
```

Group membership uses the first retained view start's context. A later snapshot,
error or end with a different browser, device or location does not move the view. Missing,
null and nonstring context forms a `null` group. The literal device type
`unknown` is a separate group. Empty results contain an empty `groups` array.
Filters apply to the start cohort before grouping. Unique viewers can appear
in several groups; use ungrouped summary for the overall unique-viewer count.

Groups sort by value ascending, with `null` first. HTTP accepts `limit=1..1000`,
defaulting to 100. Follow `next_cursor` with the same route, `group_by` and
filters; changing them returns 400. The first page's ingestion watermark
excludes later arrivals from that pagination sequence. Expiration can still
remove records between pages. `order`, `sort_by` and `event_name` are rejected
for grouped queries. Events, videos and timeseries do not accept `group_by`.

Examples:

```text
GET /v1/summary?group_by=device_type
GET /v1/quality?group_by=browser_name&video_id=video-123
GET /v1/engagement?group_by=os_name&browser_name=Chrome
GET /v1/summary?group_by=page_origin
GET /v1/summary?group_by=page_path&page_origin=https%3A%2F%2Fexample.com
GET /v1/summary?group_by=country
GET /v1/summary?group_by=city&country=GB
GET /v1/quality?group_by=continent
```

Page filters are exact and case-sensitive, with 1 to 4096 Unicode characters.
Page grouping and view reports use the first retained start, even if the player
navigates during playback. Raw-event filters use each event's page context.
Missing page context forms a null group. Combine `page_path` with `page_origin`
to distinguish identical paths on different sites. The player captures origin
and path without URL credentials, query strings or fragments.

### Hourly and daily charts

`GET /v1/timeseries` groups metrics by the time each view started.
Choose `kind=summary`, `kind=quality` or `kind=engagement`; the default is summary.
Each bucket uses the same calculations, sample counts and null semantics as that
metric set's totals route. The response includes its `kind`. Supply `from`, `to`
and `interval=hour` or `interval=day`. The video, viewer, view, browser, OS and
device filters also apply. The project is selected by the private secret key.

```sh
curl --get 'http://127.0.0.1:4174/v1/timeseries' \
  -H "Authorization: Bearer $SECRET_KEY" \
  --data-urlencode 'from=2026-10-19T00:00:00Z' \
  --data-urlencode 'to=2026-10-21T00:00:00Z' \
  --data-urlencode 'interval=day'
```

The response contains normalized `from`/`to` bounds, `interval`, `timezone: "UTC"`
and ascending `buckets`. Each summary bucket has `bucket_start`, `views`, `unique_viewers`
and `total_playing_time_ms`. Quality and engagement buckets contain their corresponding
metric fields plus `bucket_start`. Empty buckets have zero counts and null unavailable
measurements or ratios. Hourly buckets align to
UTC hours, and daily buckets align to midnight UTC, regardless of input offsets.

### Watch time and reaching the end

`GET /v1/engagement` accepts the same optional date, video, viewer, browser, OS
and device filters as summary. The private secret key selects the project.
Totals have no pagination; add `group_by` for paginated breakdowns. The MCP
equivalent is `get_metrics` with `{"kind":"engagement"}`.

```sh
curl --get 'http://127.0.0.1:4174/v1/engagement' \
  -H "Authorization: Bearer $SECRET_KEY" \
  --data-urlencode 'video_id=video-123' \
  --data-urlencode 'from=2026-10-19T00:00:00Z' \
  --data-urlencode 'to=2026-10-21T00:00:00Z'
```

```json
{
  "views": 4,
  "total_playing_time_ms": 120000,
  "average_playing_time_ms": 30000,
  "reached_end_views": 1,
  "reached_end_ratio": 0.25,
  "progress_reached": {
    "sample_views": 3,
    "at_25_percent": {
      "views": 3,
      "ratio": 1.0
    },
    "at_50_percent": {
      "views": 2,
      "ratio": 0.6666666666666666
    },
    "at_75_percent": {
      "views": 1,
      "ratio": 0.3333333333333333
    },
    "at_100_percent": {
      "views": 1,
      "ratio": 0.3333333333333333
    }
  },
  "watch_time_ms": {
    "sample_views": 3,
    "median": 20000,
    "p95": 74000,
    "distribution": {
      "under_10_seconds": 0,
      "from_10_to_30_seconds": 2,
      "from_30_to_60_seconds": 0,
      "from_1_to_5_minutes": 1,
      "at_least_5_minutes": 0
    }
  }
}
```

The average uses all selected attempts, including failed and ongoing views.
Label it **Average watch time per attempt**.

The analytics server uses one final measurement per view, or the latest snapshot if
there is no final event. Missing values contribute zero, as in summary. The
selected final event must have `reason: ended` to count as reaching the end.
Closing a page, destroying a player or changing source does not count. Multiple
final events use the same sequence/time/ingestion ordering as playing time.
An ended reason on a snapshot does not count.

With no views, both the average and ratio are null, while counts and total time
are zero. With views but no measured time or observed end, these values are zero.
All filters select the first start; matching later events can contribute even
after the date window. Retention and missing browser delivery affect
these metrics. Replays with a new view ID count as new attempts. Live videos and
ongoing attempts can lower the reached-end rate. Seeking to the end can count,
so this does not measure whether the whole video was watched.

### Progress reached and watch-time distribution

Engagement adds `progress_reached` and `watch_time_ms` without changing the
existing totals or their denominators. These fields are also present in grouped
engagement, engagement timeseries and CSV exports.

`progress_reached` counts attempts whose selected final event, or latest snapshot
when no final exists, reports a furthest progress of at least 25%, 50%, 75% or
100%. Each milestone has `views` and `ratio`. Ratios divide by `sample_views`,
which includes only attempts with a positive known media duration and a
`furthest_progress_ratio` between zero and one. A measured zero is a sample.
Missing, out-of-range progress and unknown or zero duration are excluded.
No samples means zero counts and null ratios.

Label these values **Furthest progress reached**. Seeking can reach a milestone
without playing earlier sections. These metrics do not measure watched coverage,
audience retention or where a viewer stopped. Reaching 100% also does not require
an observed end event, so it can differ from `reached_end_views`.

`watch_time_ms` reports `sample_views`, median, p95 and five exclusive distribution
counts. It uses only available `playing_time_ms` from the selected final/latest
measurement. Measured zero is included; start-only and historical null
measurements are excluded. The distribution counts sum to `sample_views`:

| Distribution field | Playing time |
| --- | --- |
| `under_10_seconds` | Less than 10 seconds, including zero. |
| `from_10_to_30_seconds` | At least 10 and less than 30 seconds. |
| `from_30_to_60_seconds` | At least 30 and less than 60 seconds. |
| `from_1_to_5_minutes` | At least one and less than five minutes. |
| `at_least_5_minutes` | Five minutes or longer. |

Percentiles use linear interpolation and are null without samples. Their sample
population differs from `average_playing_time_ms`, which still divides by all
selected attempts and treats missing playing time as zero. Repeated snapshots
and duplicate deliveries do not add extra samples. Winning final nulls remain
unknown; an earlier snapshot does not fill them. Ongoing attempts and missing
final delivery can leave the observed measurements incomplete.

### Playback quality

`GET /v1/quality` returns startup time, buffering and observed fatal errors for
one project's view cohort. It accepts the same optional date, video, viewer,
browser, OS and device filters as summary. Pagination and event-name filters are
not supported. Keep the secret key on your backend.

```sh
curl --get 'http://127.0.0.1:4174/v1/quality' \
  -H "Authorization: Bearer $SECRET_KEY" \
  --data-urlencode 'from=2026-10-19T00:00:00Z' \
  --data-urlencode 'to=2026-10-21T00:00:00Z'
```

```json
{
  "views": 4,
  "startup_time_ms": {
    "sample_views": 2,
    "median": 200,
    "p95": 290
  },
  "buffering": {
    "sample_views": 2,
    "total_playing_time_ms": 9000,
    "total_rebuffer_duration_ms": 1000,
    "ratio": 0.1
  },
  "fatal_errors": {
    "affected_views": 1,
    "ratio": 0.25
  },
  "startup_outcomes": {
    "sample_views": 3,
    "started": {"views": 2, "ratio": 0.5},
    "failed_before_start": {"views": 1, "ratio": 0.25},
    "ended_before_start": {"views": 0, "ratio": 0},
    "unknown": {"views": 1, "ratio": 0.25}
  }
}
```

`startup_time_ms` gives the median and p95 of available startup measurements.
P95 describes the slow end of the sample; percentiles use linear interpolation.
`sample_views` shows how many views supplied a measurement. A measured zero is
valid, while no samples returns null percentiles.

`buffering.ratio` divides total rebuffer time by playing plus rebuffer time. It
weights views by their measured durations. Only views with both durations and a
positive combined duration contribute. Its time totals cover those samples;
no samples returns zero totals and a null ratio. The SDK excludes startup wait,
pauses and seeks from rebuffer time. Multiply ratios by 100 to display percentages.

`fatal_errors.affected_views` counts each view once if any matching retained
playback error is fatal. Its ratio uses all selected attempts, including failures
before playback. Zero means no fatal error was observed; lost events can still
hide failures. No views returns a null ratio.

The final event supplies startup and buffering measurements, or the latest
snapshot if no final event exists. Cumulative snapshots are never added together.
A final null value stays unknown. All filters select the first retained start;
matching later measurements/errors contribute even after `to`. Start-only views
have no startup/buffering sample. Views without a retained start are excluded.
The MCP server provides the same result through `get_metrics` with
`{"kind":"quality"}`.

### Startup outcomes

Quality adds `startup_outcomes`, also available in quality breakdowns, timeseries
and CSV. It has `sample_views` and four mutually exclusive outcomes. Each outcome
contains `views` and `ratio`:

| Outcome | Meaning |
| --- | --- |
| `started` | Playback started. |
| `failed_before_start` | The selected final event explicitly reports failure before playback. |
| `ended_before_start` | The selected final event explicitly reports the attempt ended before playback. Label this **Exited before playback**. |
| `unknown` | The retained measurements do not establish a startup outcome. |

An explicit `startup_outcome` on the selected final event takes precedence.
Without it, an available startup time, including zero, or positive playing time
on the selected final/latest measurement establishes `started`. Other attempts,
including start-only attempts, remain unknown. Outcome values on snapshots or
unrelated error/state events do not establish an explicit final outcome.

All four counts sum to `views`. All outcome ratios divide by **all selected
attempts**, including unknown outcomes, and are null with no attempts.
`sample_views` counts known outcomes, excluding unknown. Missing final events do
not prove an exit or failure. These counts describe startup, so an attempt that
started successfully can still have a fatal error later.

### Per-video reports for a dashboard

Use the same `video_id` and date filters on these existing routes:

| Route | Information to show |
| --- | --- |
| `/v1/summary?video_id=VIDEO_ID` | Views, supplied viewer IDs and watch time. |
| `/v1/engagement?video_id=VIDEO_ID` | Average watch time, observed ends, progress reached and watch-time distribution. |
| `/v1/quality?video_id=VIDEO_ID` | Startup outcomes, startup timing, buffering and observed fatal errors. |
| `/v1/quality?video_id=VIDEO_ID&group_by=browser_name` | Playback health by browser; OS and device breakdowns are also supported. |

These reports select the same start cohort and count attempts once. Keep the
private key on the dashboard backend and respect the project's read concurrency
budget. Show sample counts beside measurements and keep unknown values distinct
from zero. No additional route or player SDK update is required. Historical
stored events contribute where their measurements are available.

### Renditions and average bitrate

`GET /v1/renditions` accepts the usual date, video, viewer, view and browser/OS/device
filters. It uses the same start cohort as summary and quality. Only the final
cumulative `rendition_playing_time` array contributes, or the latest snapshot if
there is no final event. A final null stays unknown; snapshots are never added
together. Historical retained events work without a schema migration.

The response contains `views`, `sample_views`, `total_playing_time_ms`,
`bitrate_sample_playing_time_ms`, `average_bitrate_bps`, `total_renditions` and
`renditions`. Renditions are grouped by dimensions, codec, declared bitrate and
frame rate, then ranked by measured playing time descending. Each row includes
`views`, `playing_time_ms` and its `playing_time_ratio` across all measured
rendition time. A view can watch several renditions, so row view counts must not
be added. Display dimensions as width × height, not an inferred resolution label.

Average bitrate is `sum(bitrate_bps * playing_time_ms) / sum(playing_time_ms)`
for entries with a known bitrate. It measures the declared bitrate selected by
the player, not network throughput. Missing bitrate is excluded from that
average but its watch time still contributes to rendition shares. No measured
time with a known bitrate returns a null average. Report the sample time beside
it. Missing rendition measurements can make this report's watch time smaller
than overall watch time.

`limit` defaults to 100 and accepts 1–1000. It limits ranked rows only, not totals
or the average. Compare returned row count with `total_renditions` to identify a
truncated ranking. There is no cursor or grouping parameter. CSV
`resource=renditions` exports all matching rendition rows. MCP supports the same
report through `get_metrics` with `kind=renditions`, bounded to 100 ranked rows.

### Playback attempts and event history

`GET /v1/views` returns `views` and `next_cursor`. Each row describes one attempt
with a retained start: `view_id`, `video_id`, `viewer_id`, `started_at`, browser,
OS and device names, `playing_time_ms`, optional `startup_time_ms` and
`rebuffer_duration_ms`, and the booleans `reached_end`, `has_ended_event` and
`has_fatal_error`. Measurements use the same final/latest selection as totals.
The first retained start supplies context. A missing end event does not prove
an attempt is ongoing, and a missing error does not prove successful playback.

The usual identity, date and context filters apply, including `view_id`.
`has_fatal_error=true|false` filters on observed fatal errors. Pages sort by
`started_at`, newest first by default or oldest first with `order=asc`. Equal
start times use `view_id` in the same direction. Other supported `sort_by` values
are `video_id`, `view_id`, `device_type`, `playing_time_ms`, `startup_time_ms`,
`rebuffer_duration_ms` and `playback_score`. Durations sort numerically in
milliseconds, and missing values sort last in both directions. Sorting covers
the full filtered cohort before pagination. HTTP page sizes
are 1 to 1000, default 100. Cursors bind filters, sort field, order, fatal-error selection,
project and the initial ingestion watermark.

`GET /v1/views/{view_id}` returns `view`, `events` and `next_cursor`. Only `limit`
and `cursor` are accepted. The metrics and event history share one ingestion
watermark across pages. Events include original payloads and context, in
ingestion order, with SDK sequence and occurrence times preserved. Follow the
cursor with the same view ID to retrieve the remaining history. Expiration or
deletion can still remove retained data between pages. A view with no retained
start returns 404; its orphaned events remain queryable with
`GET /v1/events?view_id=...`.

### Error-code groups

`GET /v1/errors` returns `errors` and `next_cursor`. Filters select the first
retained start cohort, just like summary. Matching error events for the selected
view identity contribute even after the date window. Each group contains
`error_code`, `fatal`, `error_events` and `affected_views`. Codes retain their
JSON type, so numeric `2`, text `"2"` and null are separate groups. Missing fatal
status remains null. Event counts use distinct event IDs per group; affected
views use distinct view IDs. A view can appear under several codes, so these
counts must not be added to calculate a global affected-view count.

Groups sort by canonical JSON code and fatal status ascending. HTTP pages use
`limit=1..1000`, default 100. Continue with the same filters and cursor. Cursors
bind the project and filters and exclude later arrivals. The MCP equivalent is
`get_metrics` with `kind=errors`.

### Available filter values

`GET /v1/dimensions/{dimension}` returns `dimension`, `values` and `next_cursor`.
Supported dimensions are `browser_name`, `os_name`, `device_type`, `continent`,
`country`, `city`, `page_origin`, `page_path`, `video_id` and `viewer_id`. Each value has `value` and `views`;
values and counts come from first retained starts after applying the usual date, identity and context filters.
Values sort ascending with null first. Literal `unknown` remains distinct from
missing context. HTTP page sizes are 1 to 1000, default 100. Cursors bind the
dimension, project, filters and ingestion watermark.

### CSV exports

API and MCP use the same export workflow. Request a download link with a project
private key; read-only private keys can export:

```sh
curl --fail 'http://127.0.0.1:4174/v1/exports' \
  -H "Authorization: Bearer $SECRET_KEY" \
  -H 'Content-Type: application/json' \
  -d '{"resource":"videos","query":{"sort_by":"views","order":"desc"}}'
```

The 201 response contains `download_url`, `filename`, `content_type: "text/csv"`
and `expires_at`. Open `download_url` or download it with:

```sh
curl --fail --output videos.csv "$DOWNLOAD_URL"
```

`GET /v1/exports/{token}` needs no Authorization header. The 256-bit random token
authorizes only its saved project and filters; URL query overrides are rejected.
Treat the URL as a credential. Tokens expire after 30 minutes and are invalidated
by issuer-key rotation/revocation, project deletion or analytics server restart. Expiry
is checked before a download starts; it does not interrupt an active download.

CSV generation starts when the link is opened. It uses one read snapshot on a
separate connection and streams rows with backpressure. CSV contents are never
stored as an export file or sent inside MCP responses. All matching rows are
included, without normal query page or byte limits; `limit` and `cursor` are
rejected. By default one CSV generation can run per project and four across the
server. Excess downloads return 429 immediately with `Retry-After`; they do not
wait. Download rates are shared across the project's keys and links, including
repeated downloads of the same link.
Date filters are optional except for `timeseries`; no fixed 30-day cap applies.
The server retains at most 1024 active grant records and returns 503 when full.
Default rates are 10 links and 2 download starts per minute per project, with
continuous refill and burst capacities equal to those rates. Analytics reads
have a separate budget of 120/minute with a burst of 20, at most 2 queued/running
per project and 8 server-wide. HTTP and MCP share these limits. See
[configuration](configuration.md#read-and-export-limits) to override them.

Supported resources are `summary`, `quality`, `renditions`, `engagement`, `timeseries`,
`videos`, `events`, `views`, `errors` and `dimensions`. `query` uses the resource's
normal query arguments. `timeseries` accepts `kind` and includes every requested
UTC bucket, including ranges larger than the normal 1000-bucket query limit.
`dimensions` requires a `dimension` in `query`.

For reports to open in a spreadsheet, export `views`, `videos` or `timeseries`
with `interval=day`. A 30-day daily report has 30 buckets even when the source
contains millions of events. Raw `events` exports have one row per event and can
be much larger.

Files use UTF-8 and CRLF record separators. Fields containing commas, quotes or
line breaks are quoted, and embedded quotes are doubled. Nested metric columns
use paths such as `startup_time_ms/median`; grouped metrics use `data/...` columns.
Event exports have one column for each documented scalar event field, such as
`event_name`, `view_id`, `playing_time_ms`, `rebuffer_duration_ms` and `error_code`.
Flat context columns use a `context.` prefix, such as `context.browser_name` and
`context.continent`, `context.country` and `context.city`. Metadata columns include `received_at`, `project_id` and
`ingest_key_id`. CSV cells never contain serialized JSON objects or arrays.
Custom attributes, nested rendition arrays and historical OTLP payloads are
excluded; use `GET /v1/events` for original payloads. Null values become empty
cells. Strings that could be interpreted as spreadsheet formulas receive an
apostrophe prefix. Empty exports contain headers.

Normal analytics endpoints continue returning JSON. Interrupted or failed downloads
terminate the stream; discard the partial file and retry the download. Local reads
remain consistent during ingestion and retention. With S3 storage, deleting source
objects during a download can abort it. Large queries still consume database CPU,
memory and storage I/O.

Behind an HTTPS ingress or a URL prefix, set `server.public_url` to the externally
reachable analytics server base, for example `https://analytics.example.com/collector/`.
Otherwise the analytics server derives the URL from the request Host and scheme; it only
trusts `X-Forwarded-Proto` from configured trusted proxies. In-memory tokens belong
to one analytics server process; route downloads to the process that issued their link.

## Watched-section retention

Use `GET /v1/retention?video_id=VIDEO_ID` to build engagement curves along media position. See [Video timelines](video-timelines.md) for measurements, sample counts, bucket resolution, CSV and rendition-event semantics.
