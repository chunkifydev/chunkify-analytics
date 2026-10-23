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

Grouped comparisons and view-detail lookups can follow separately. Accurate watched-segment retention requires SDK work. These
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

## Second stage: playback quality summary

Expose `GET /v1/quality` and MCP `get_quality` using the existing project read
token and optional `from`, `to`, `video_id`, `viewer_id`, `browser_name`, `os_name`
and `device_type` filters. Reject pagination, event-name and unknown parameters.
Dates and identity/context filters select the first retained start exactly as in
summary. One view contributes at most once, under one ingestion watermark.

Return `views` and these groups:

- `startup_time_ms`: `sample_views`, `median`, `p95`. Use non-null numeric startup
  measurements from the winning final event, or latest snapshot when no final
  event exists. Zero is a valid measurement. Compute continuous percentiles with
  linear interpolation. No samples means null percentiles, not zero.
- `buffering`: `sample_views`, `total_playing_time_ms`,
  `total_rebuffer_duration_ms`, `ratio`. Use playing and rebuffer durations from
  the same winning event. Include only views with both measurements available
  and a positive combined duration. Ratio is total buffering divided by total
  playing plus buffering across those views, a fraction from 0 to 1. Do not
  average per-view ratios or add cumulative snapshots. No eligible views means
  zero totals and a null ratio. Startup wait, pauses and seeking are not counted
  as rebuffering by the SDK.
- `fatal_errors`: `affected_views`, `ratio`. Count a view once if any retained
  matching `playback_error` has `error_fatal=true`, including failures before
  playback. Divide by all selected playback attempts (`views`). No views means
  null; views with no observed fatal errors mean zero. This is an observed error
  rate, not proof that all error events were delivered.

All measurements and errors must match the first start's video/viewer identity.
Later events outside the start-date window still contribute. Starts without a
measurement count as views, with no startup/buffering sample. Orphan events do
not contribute. A final null startup value remains unknown even if an older
snapshot had one. Read typed numeric columns and the lossless JSON fallback so
integral JSON floats and large numeric values are not accidentally lost.
Calculate ratios without overflowing the sum of two finite durations. If an
aggregate itself cannot fit a finite response number, return 500 rather than
serializing it as a misleading null or zero.

No SDK or storage-schema changes, persisted aggregates, arbitrary SQL, grouped
quality comparisons or quality time series are included. Existing summary,
video and timeseries responses keep their semantics. Use the bounded read worker.

### Second-stage acceptance checks

- Exact startup sample counts, median and p95 for odd/even samples, valid zero,
  null final values, and empty cohorts.
- Weighted buffering, zero-duration exclusion, same-event selection, final
  precedence, latest snapshot fallback and no cumulative double counting.
- Duplicate/reordered deliveries, multiple fatal errors in one view, nonfatal
  and unknown errors, orphan events, inconsistent identity and pre-start errors.
- Start-based date/context filtering and project isolation for overlapping IDs.
- Native and OTLP delivery produce the same quality result; lossless numeric
  storage fallback is covered, including JSON integral floats.
- OpenAPI examples, live HTTP responses in both reader modes, MCP discovery,
  parity and error handling pass. Existing metric tests remain green.

### Second-stage verification, 2026-10-23

Implemented in `b09f18b`, with the reviewed numeric fix in `6ceb54e`.

- 24 Rust unit tests and four official-client MCP integration tests passed.
  Formatting and Clippy across all targets passed.
- 21 HTTP, OpenAPI and context-filter checks passed. Quality and existing time
  series were exercised with both database reader modes. Native and OTLP
  quality results matched. MCP checks included overlapping IDs across projects.
- Regression cases cover missing historical durations, final null measurements,
  repeated snapshots/errors, inconsistent identity, integral JSON floats and
  large numeric values preserved in JSON.
- Standards review found no actionable issues. Spec review found an overflow
  when adding two finite durations. The old binary reproduced a ratio of zero
  for equal durations of `1e308`; the fix returns 0.5. Totals that themselves
  exceed finite output range now return the documented 500 error. Both review
  axes confirmed no remaining findings after the fix.
- Tests used temporary databases and disposable containers. No capacity benchmark,
  browser download, previous-binary upgrade test or SDK change was needed for
  this stage. Existing deployed collectors were not changed.

This completes the planned first playback-quality summary. Grouped comparisons,
quality trends and more engagement metrics can wait for actual dashboard needs.
The next useful validation is a real player session displayed in a dashboard,
using the new endpoint and its sample counts.

## Third stage: average watch time and reached-end rate

Add `GET /v1/engagement` and MCP `get_engagement`, authorized by the existing
project read token. Accept the same optional start-cohort date, video, viewer,
browser, OS and device filters as summary. Reject pagination, event-name and
unknown parameters. Share the first-start selection, matching video/viewer
identity and final-event/latest-snapshot precedence with the existing metrics.
Use one ingestion watermark and the bounded read worker.

Return five fields:

- `views`: selected playback attempts, including failed, ongoing and start-only
  attempts. Replays with another view ID count separately.
- `total_playing_time_ms`: the same total as summary for unchanged data and filters.
  Each view contributes its selected cumulative measurement once; missing values
  contribute zero. Pauses, buffering and seeking do not count as playing time.
- `average_playing_time_ms`: total playing time divided by all selected views.
  This is average watch time per attempt, not per unique viewer or completed view.
  Return null without views and zero when views exist but have no measured time.
- `reached_end_views`: count selected views whose winning `view_ended` has
  `reason: ended`. Snapshots, errors, missing/other reasons and other identities
  cannot mark a view as reaching the end. If multiple final events exist, use the
  same winner as playing time, ordered by sequence, occurrence time, ingestion
  order. Count each view at most once.
- `reached_end_ratio`: reached-end views divided by all selected attempts, a
  fraction from 0 to 1. Return null without views; otherwise zero when none reached
  the end. This is observed end-of-media, not watched coverage or a retention curve.

All filters select the first retained start. Events after the date window can
contribute to that start. Orphans are excluded. Missing final events or snapshots
can undercount watch time and end detection; live/ongoing attempts can lower the
reached-end rate. Retention/deletion remove contributions. Do not average rates or
averages across groups without weighting their denominators. Reject non-finite
aggregate totals with 500, consistently with quality summaries.

No SDK, schema migration, grouped endpoint or retention-curve work is needed.
Existing HTTP responses and MCP tools retain their behavior. Document the new
operation in OpenAPI, README and MCP, and explain the complete dashboard workflow.

### Third-stage acceptance checks

- Exact totals, average and reached-end ratio for mixed completed, abandoned,
  ongoing, failed and start-only attempts; empty and zero-valued cohorts.
- Duplicates, reordered snapshots and conflicting final events do not inflate
  metrics. Sequence/time/ingestion ties follow existing summary rules.
- An ended reason on a snapshot cannot mark completion. Different identities,
  orphan events and starts outside the filters do not contribute.
- Every supported filter selects the first start, including context changes,
  exclusive date bounds and later final events.
- Native and OTLP ingestion match; overflow returns 500. Both database reader
  modes, OpenAPI examples/responses and MCP discovery/parity/isolation pass.
- Keep read tokens private and describe the distinction between the collector,
  a customer's dashboard, and the local MCP companion.

### Third-stage verification, 2026-10-23

Implemented in `2ddad6d` on `codex/analytics-timeseries`.

- 25 Rust unit tests and four official-client MCP integration tests passed.
  Formatting and Clippy across all targets passed.
- 23 HTTP/OpenAPI/context-filter checks passed, including engagement in both
  reader modes and regression checks for quality and time-series calculations.
  Native and OTLP engagement results matched. MCP verified isolated projects
  with overlapping event/view IDs and different watch-time/end totals.
- Tests cover empty and zero cohorts, historical null measurements, all filters,
  missing final delivery, failed/ongoing attempts, duplicate deliveries,
  conflicting final events, identity mismatches and overflowing totals.
- Standards and Spec reviews found no actionable findings.
- Tests used small temporary databases and disposable containers. No SDK change,
  schema migration, browser download or capacity benchmark was needed. Existing
  deployed collectors were not changed.

The README's dashboard workflow maps each HTTP operation to a customer-facing
card, chart, table or event inspector and explains how the local MCP companion
helps an AI coding tool build that dashboard.


## Dashboard acceptance, 2026-10-23

The reusable example is in `examples/dashboard`. It has Overview, Playback
quality, Engagement and Events pages, common cohort filters, manual refresh,
a paginated video table and a drawer for the latest ten stored events. Node
keeps each project's private read token on the backend. The existing Player test
project remains separate from an explicitly labelled synthetic demo project.

The actual stdio MCP companion was used for `tools/list`, `describe_api` for the
three new operations, and `get_summary`, `get_quality`, `get_engagement` and
`get_timeseries`. Its structured `data` matched the dashboard proxy responses
with identical date/project scope. This validates an MCP-assisted dashboard
workflow; the generated dashboard itself uses the HTTP API at runtime.

The demo contains 816 events for 268 attempts across nine videos and seven days.
Independent fixture calculations and real collector responses agree on 91
supplied viewer IDs, 65,068,260 ms playing time, 170 reached-end views and 12
fatal-error views. Startup has 236 samples, median 1,021 ms and p95 1,909 ms.
Buffering has 256 samples and 467,546 ms rebuffer time, a ratio of approximately
0.0071342. These are synthetic presentation data, not capacity measurements.

The live walkthrough exposed a packaging defect missed by the earlier tests:
quality SQL needed DuckDB's JSON extension, which the test environment could
automatically install. The non-root runtime container had no writable home and
returned 500 even for an empty quality query. Enabling the pinned DuckDB crate's
`json` feature bundles the extension into the executable, with no runtime
download or privilege change. A packaged regression failed before the fix and
passed afterwards. The crate's feature definition is documented at
https://docs.rs/crate/duckdb/1.10506.0/features.

A second packaged regression confirmed that two accepted `1e308` playing-time
values could produce a 200 timeline response with a null total. The timeline now
rejects that unrepresentable aggregate with 500, as quality and engagement do.
The regression failed before the guard and passed with it.

Validation:

- Ten dashboard backend tests passed. They cover project/route isolation, exact
  filters, bounded event scans, large records, credential redaction and local
  host/origin restrictions.
- Both packaged regressions passed in the actual non-root, read-only runtime.
- Browser checks passed for all four pages, nine-video pagination, video
  drilldown, UTC dates, combined browser/device filters, explicit Refresh,
  empty/null metrics, event JSON and Escape dismissal. October 23 alone returned
  47 attempts; adding mobile and Safari returned 16. Player test retained its
  three views and 14,281.60000038147 ms of playing time.
- No browser console errors were observed. Source and browser responses were
  checked for leaked runtime credentials; none were found.
- Standards review: no remaining blockers. The event scan now has a shared
  20-second deadline, and event-read failures preserve successful aggregates.
- Spec review: no remaining blockers. Startup and buffering previews now include
  sample counts; event pages use ten records per request while keeping the
  2,000-record scan limit.

The local demo stays running for manual testing. The branch's GitHub checks are
still required before merge; the local checks above do not replace CI.
