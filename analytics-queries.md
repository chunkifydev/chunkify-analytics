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

## Verification, 2026-10-23

The first stage was implemented on `codex/analytics-timeseries`, based on
`41655af`. The feature commit is `b4ce252`; the subsequent MCP cleanup only shares
OpenAPI parameter reference resolution between its two callers.

- Rust: 23 unit tests and four official-client MCP integration tests passed.
  Formatting and Clippy across all targets passed.
- HTTP and contract checks: 19 OpenAPI/context-filter tests passed, including the
  new time-series tests with both database reader modes. Another 57 collector
  regression tests passed. One crash-recovery check was rerun after adding its
  Python prerequisite to the disposable test image; permission checks ran as a
  non-root container user.
- The unchanged Chromium adapter test was not run in this pass. Two optional
  previous-executable upgrade tests were skipped because no previous binary was
  supplied. No schema migration or SDK changes are part of this feature.
- Standards and Spec reviews found no remaining findings after the shared MCP
  parameter-resolution cleanup.

A disposable query probe seeded 300,000 rows representing 100,000 views across
30 days, with start/snapshot/end events, 100 videos and 10,000 supplied viewer IDs.
It exercised the actual Store methods with one DuckDB thread on the shared
Docker Desktop VM. Views, playing time and summary totals were asserted.

| Query | Returned buckets | Median of five queries | Observed range |
| --- | --- | --- | --- |
| Hourly | 720 | 109.4 ms | 95.7–119.2 ms |
| Daily | 30 | 96.7 ms | 94.7–103.4 ms |
| Existing summary | Not applicable | 92.7 ms | 92.3–98.7 ms |

These were sequential queries immediately after seeding, using a synthetic
fixture with only the columns needed for the metrics. They do not measure HTTP
throughput, concurrent ingestion, cold caches or production sizing. The probe
asserted that its temporary database was removed after the run. The measurements
support proceeding with this first stage without a new storage structure.
