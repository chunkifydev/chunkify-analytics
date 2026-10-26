# HTTP API reference

See the [OpenAPI specification](../openapi.json) for request and response schemas.

| Method and path | Authorization | Response |
| --- | --- | --- |
| `GET /healthz` | None | `{"status":"ok"}` after a database check, or 503 if unavailable. |
| `POST /v1/events` | `X-Chunkify-Ingest-Key` | 201 with `{"accepted":true,"duplicate":false}` after commit; 200 with `{"accepted":false,"duplicate":true}` for an existing event. |
| `POST /v1/logs` | `X-Chunkify-Ingest-Key` | OTLP JSON logs. 200 with `{}` after the whole batch commits, including empty or duplicate-only batches. |
| `GET /v1/events` | `Authorization: Bearer READ_TOKEN` | `{"events":[{"project_id":"default","ingest_key_id":"primary-ingest","received_at":"...","event":{...}}],"next_cursor":null}` |
| `GET /v1/summary` | `Authorization: Bearer READ_TOKEN` | `{"views":3,"unique_viewers":1,"total_playing_time_ms":6500}` |
| `GET /v1/engagement` | `Authorization: Bearer READ_TOKEN` | Average playing time per attempt and observed reached-end count/rate. |
| `GET /v1/quality` | `Authorization: Bearer READ_TOKEN` | Startup percentiles, buffering and observed fatal errors, with sample counts. |
| `GET /v1/timeseries` | `Authorization: Bearer READ_TOKEN` | Hourly/daily views, unique viewers and playing time; requires `from`, `to`, `interval`. |
| `GET /v1/videos` | `Authorization: Bearer READ_TOKEN` | `{"videos":[{"video_id":"video-123","views":3,"unique_viewers":1,"total_playing_time_ms":6500}],"next_cursor":null}` |

Native ingestion accepts `Content-Type: application/json` and one complete schema
version 1 event. It validates all seven event shapes emitted by the SDK. Malformed
or invalid payloads return 400; unsupported media types return 415; oversized bodies
return 413. Missing or invalid keys return 401. Disallowed origins return 403.

Only the ingest endpoints enable CORS, including preflight for the ingest header.
Read endpoints are intended for the customer's backend and do not enable CORS.
Keys are never accepted in a query string. Responses are marked `no-store`.

### OTLP format and storage

`POST /v1/logs` accepts `application/json`, with no content encoding or with
`Content-Encoding: gzip`. It supports multiple `resourceLogs`, `scopeLogs`, and
`logRecords` using the lower camel case field names in the
[OTLP JSON specification](https://opentelemetry.io/docs/specs/otlp/#json-protobuf-encoding).
Unknown OTLP message fields are ignored during normalization and preserved in
the stored log and context. This endpoint accepts Chunkify analytics logs only.
Binary Protobuf, gRPC, metrics, traces, and arbitrary log events are outside its scope.

The canonical event comes from each log's `attributes`. The decoder supports the
SDK's string, boolean, integer, finite number, array, and key-value list values.
Integer values may be decimal strings or JSON integers. Unsupported value types,
duplicate attribute keys, and invalid Chunkify fields reject the export. Custom
`mapAttributes` functions must retain all required schema fields. The collector
restores known nullable fields omitted by the SDK, including nullable rendition
fields. It cannot reconstruct additional null fields that a custom mapper omitted.

Every log is validated before storage. An invalid record rejects the entire
batch with HTTP 400. Wire size, decompressed size, or record count violations return
413. Unsupported content types or encodings return 415. OTLP errors use the JSON
Status shape `{"message":"..."}`, including authentication and rate-limit errors.
No partial export is accepted, and success is sent only after the transaction commits.

The raw-event API returns the canonical `event` and, for an event first received
through OTLP, an additional `otlp` object:

```json
{
  "log_record": { "attributes": [], "eventName": "..." },
  "resource_logs": { "resource": {}, "schemaUrl": "..." },
  "scope_logs": { "scope": {}, "schemaUrl": "..." }
}
```

`log_record` retains the original log object. `resource_logs` and `scope_logs`
retain their original group fields, excluding the nested record/group arrays.
Objects preserve JSON values, not original whitespace or key order. Native events
have no `otlp` field. Deduplication is shared across formats: the first accepted
payload and context win, and later duplicates do not enrich or replace them.
Both formats feed the same filters, pagination, and summary calculations.

### Flat event context and request IP

`GET /v1/events` returns a flat `event_context` alongside the original `event` and
optional `otlp`. For example:

```json
{
  "schema_version": 1,
  "browser_name": "Firefox",
  "os_name": "Linux",
  "device_type": "desktop",
  "language": "fr-FR",
  "peer_ip": "10.20.1.2",
  "client_ip": "198.51.100.8",
  "client_ip_source": "x_forwarded_for"
}
```

The collector copies the supported scalar or null fields from schema version 1
`event.browser_context` into this object. Unsupported fields and nested values
remain only in the original payload. Browser values are client-reported, not
verified identity. Only the collector supplies `peer_ip`, `client_ip`, and
`client_ip_source`. When IP collection is disabled, these fields are absent and
browser context is still available. Without either source, `event_context` is
omitted. This context schema has its own `schema_version: 1`.

`peer_ip` comes from the TCP connection. Without a trusted proxy, `client_ip`
equals `peer_ip` and `client_ip_source` is `connection`. Browser-provided fields
cannot replace this collector-derived context. IPv4-mapped IPv6 addresses are
stored as IPv4. The collector makes no outbound IP lookup and does not infer
viewer identity or location from the address.

For a reverse proxy, configure only addresses or CIDRs you control. For example,
`COLLECTOR_TRUSTED_PROXIES=10.20.1.2,2001:db8:abcd::/48`. Ensure each trusted proxy
overwrites incoming `X-Forwarded-For` with the actual source IP, or correctly
appends that source to the existing chain. Restrict direct access to the collector
where possible. Broad trust ranges can allow clients to forge their address.

The collector reads `X-Forwarded-For` only when the direct peer is trusted. It
combines all header lines in order, walks from right to left, skips configured
trusted hops, and selects the first untrusted address. This follows the
[trusted-proxy selection described by MDN](https://developer.mozilla.org/en-US/docs/Web/HTTP/Reference/Headers/X-Forwarded-For#selecting_an_ip_address).
Only literal IPv4 or bare IPv6 entries are accepted, with at most 32 addresses
and 4096 bytes across the header values. Missing, malformed, excessive, or entirely
trusted chains leave `client_ip` and `client_ip_source` null while retaining
`peer_ip`. The event is still accepted. `Forwarded`, `X-Real-IP`, and
`CF-Connecting-IP` are ignored; configure the proxy to supply `X-Forwarded-For`.

The IP describes the HTTP sender. When a gateway batches multiple viewers' events,
the request context describes that gateway for every event in the batch. It cannot
recover those viewers' individual IPs. NAT, Docker networking and VPNs can also
change the visible address. Supplied `viewer_id` remains the basis for unique-viewer
counts. IP data is available only through the private raw-event API, with no IP
filter or GeoIP enrichment in this stage.

Set `COLLECTOR_STORE_IP=false` and restart to omit IP fields for new events.
Raw forwarding headers are neither stored nor logged, and normal request handling
does not log IP values. First accepted delivery wins, including whether context
was collected; retries never add or replace it. Retention and viewer deletion
remove the event's context with its row.

### Filters and pagination

All read endpoints accept `from`, `to`, `video_id`, `viewer_id`, `browser_name`,
`os_name`, and `device_type`. Timestamps use
RFC3339 with a timezone and are compared in UTC, with `from` inclusive and `to`
exclusive. The raw event endpoint also accepts `event_name`.

Browser and OS names use exact, case-sensitive matching and accept 1–64 Unicode
characters. Device type accepts `desktop`, `mobile`, `tablet`, or `unknown`.
Multiple filters combine with AND. Missing, null, and nonstring context values
do not match an explicit filter. `device_type=unknown` matches only a stored
`unknown` value; it does not include events with no device information.

`/v1/events` and `/v1/videos` accept `limit` from 1 to 1000, defaulting to 100.
Follow `next_cursor` until it is null. Keep the same project and filters when following a
cursor. Invalid cursors, changed filters, unknown parameters, and invalid ranges
return 400. Treat cursors as opaque.

Events are ordered by their server ingestion sequence, oldest first by default.
Use `/v1/events?order=desc&limit=10` for the newest ten received events in one
request. `order` accepts `asc` or `desc` only on this route. Keep the same order
with each cursor; changing it returns 400. Videos are ordered by `video_id`. A pagination sequence is limited to events already committed
at its first page, so later arrivals do not shift pages or change video totals
within that sequence. Start without a cursor to refresh. This is not an indefinite
export snapshot; retention and deletion can remove records between requests.

For example, run this from your dashboard backend or terminal, using a local
`READ_TOKEN` environment variable containing the private token:

```sh
curl -H "Authorization: Bearer $READ_TOKEN" \
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
  an ingest key does not verify their authenticity.

### Hourly and daily charts

`GET /v1/timeseries` returns the same metrics grouped by the time each view
started. Supply `from`, `to` and `interval=hour` or `interval=day`. The existing
video, viewer, browser, OS and device filters also apply. The project is selected
by the private read token.

```sh
curl --get 'http://127.0.0.1:4174/v1/timeseries' \
  -H "Authorization: Bearer $READ_TOKEN" \
  --data-urlencode 'from=2026-10-19T00:00:00Z' \
  --data-urlencode 'to=2026-10-21T00:00:00Z' \
  --data-urlencode 'interval=day'
```

The response contains normalized `from`/`to` bounds, `interval`, `timezone: "UTC"`
and ascending `buckets`. Each bucket has `bucket_start`, `views`, `unique_viewers`
and `total_playing_time_ms`. Empty buckets contain zeros. Hourly buckets align to
UTC hours, and daily buckets align to midnight UTC, regardless of input offsets.

### Watch time and reaching the end

`GET /v1/engagement` accepts the same optional date, video, viewer, browser, OS
and device filters as summary. The private read token selects the project.
There is no pagination. The MCP tool is `get_engagement`.

```sh
curl --get 'http://127.0.0.1:4174/v1/engagement' \
  -H "Authorization: Bearer $READ_TOKEN" \
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
  "reached_end_ratio": 0.25
}
```

This example represents four playback attempts, 120 seconds of total playing
time, 30 seconds per attempt on average, and one attempt reaching the end: 25%.
The average uses all selected attempts, including failed and ongoing views.
It is not the average per unique viewer. Label it **Average watch time per attempt**.

The collector uses one final measurement per view, or the latest snapshot if
there is no final event. Missing values contribute zero, as in summary. The
selected final event must have `reason: ended` to count as reaching the end.
Closing a page, destroying a player or changing source does not count. Multiple
final events use the same sequence/time/ingestion ordering as playing time.
An ended reason on a snapshot does not count.

With no views, both the average and ratio are null, while counts and total time
are zero. With views but no measured time or observed end, these values are zero.
All filters select the first start; matching later events can contribute even
after the date window. Retention/deletion and missing browser delivery affect
these metrics. Replays with a new view ID count as new attempts. Live videos and
ongoing attempts can lower the reached-end rate. Seeking to the end can count,
so this does not measure whether the whole video was watched.

### Playback quality

`GET /v1/quality` returns startup time, buffering and observed fatal errors for
one project's view cohort. It accepts the same optional date, video, viewer,
browser, OS and device filters as summary. Pagination and event-name filters are
not supported. Keep the read token on your backend.

```sh
curl --get 'http://127.0.0.1:4174/v1/quality' \
  -H "Authorization: Bearer $READ_TOKEN" \
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
The MCP server provides the same result through `get_quality`.

The server stores receipt time separately for retention and operations.
Server operational messages use stdout/stderr and contain no keys. CLI commands
that create or rotate project credentials return new secrets once on stdout.
