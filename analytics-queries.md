# Analytics queries

## First stage: hourly and daily trends

Add an authenticated, project-scoped `GET /v1/timeseries` operation and an MCP
`get_timeseries` tool using the existing stored events. No SDK or database schema
change is needed.

Required inputs are `from`, `to` and `interval=hour|day`. Optional inputs are the
existing video, viewer, browser, OS and device filters. Dates use RFC3339 with a
timezone, normalized to milliseconds. All bucket boundaries use UTC. A request
may intersect at most 1000 buckets, including partial and empty buckets.

Return the complete ascending series, including zeros for empty buckets. Each
bucket has its UTC start, views, distinct supplied viewer IDs and total playing
time. Unknown parameters and oversized/invalid windows return 400. MCP advertises
required inputs from OpenAPI and rejects invalid arguments before querying HTTP.

Reuse the summary's first-start selection, identity matching, final-event
precedence and latest-snapshot fallback. All of a view's cumulative playing time
belongs to its start bucket. A start outside the window contributes nothing,
even if playback continues inside it. A start inside the window contributes its
retained total even if playback continues outside it. This is not a measure of
activity per clock hour. Do not sum cumulative snapshots or unique viewers across
buckets. Missing starts are excluded; missing final events may undercount time.

The existing summary and video endpoints keep their behavior. The new query uses
the existing bounded read worker and a single ingestion watermark. No per-bucket
database queries or new persistent copies of analytics data are introduced.

## Acceptance checks

- Hour/day UTC alignment, input offsets, partial boundaries and exact exclusive
  upper bounds work, including dates before Unix epoch.
- Empty windows return zero-valued buckets covering the requested range. More than 1000
  buckets are rejected; exactly 1000 are accepted.
- Duplicate and reordered events do not inflate totals. Final values beat
  snapshots, latest snapshots are not added, and inconsistent identities do not
  contribute to another video's view.
- Views and playing time sum to the matching summary on unchanged data. Per-bucket
  unique viewers are distinct within each bucket, not globally additive.
- Filters use the selected start's metadata. Read tokens isolate projects,
  including overlapping view/event IDs. Ingest keys cannot authorize the query.
- OpenAPI examples validate. MCP discovery, required arguments and HTTP parity
  pass. Existing HTTP and Rust tests still pass.
- Measure the query on a bounded synthetic dataset and clean up its files.

## Later stages

Playback-quality summaries, grouped comparisons and view-detail lookups can
follow separately. Accurate watched-segment retention requires SDK work. These
features are outside this first stage.
