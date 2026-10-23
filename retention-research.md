# Collector retention research

Checked on 2026-10-19 against public vendor documentation. These are published
product capabilities, not a description of every internal storage layer or a
guarantee for a particular customer contract.

## Vendor findings

| Provider | Standard retention | Longer retention | Engagement versus technical metrics |
| --- | --- | --- | --- |
| Mux Data | 100 days on all current plans. Its privacy guide says pseudonymized view data is deleted after up to 100 days. | Optional 13-month long-term metrics on Custom Media, with basic dimensions. Advanced dimensions remain limited to 100 days. | The standard allowance explicitly covers both engagement and QoE. Long-term metrics explicitly retain QoE, including technical performance metrics. This is a distinction in detail and dimensions, not a policy of discarding all technical metrics early. |
| Bitmovin Analytics / Observability | Current public pricing includes 30 days and offers longer durations by agreement. | The current public pricing page does not give a universal extended duration. Its April 2026 incident report confirms customers on both 30-day and 90-day retention tiers. | No public category-specific retention policy was found in the reviewed sources. Do not assume that audience metrics survive longer than buffering or startup metrics. |

Sources for Mux: [current pricing](https://data.mux.com/pricing),
[view-data privacy guidance](https://www.mux.com/docs/guides/ensure-data-privacy-compliance),
[standard engagement and QoE retention announcement](https://data.mux.com/blog/the-newest-way-to-get-started-with-mux-data-self-service-media-plan),
and [long-term metrics availability and dimensions](https://data.mux.com/blog/13-months-of-video-insights-long-term-metrics-is-now-ga).

Sources for Bitmovin: [current pricing FAQ](https://bitmovin.com/pricing/)
and [April 2026 recovery report](https://status.bitmovin.com/history/3).

Mux assigns engagement and QoE values to individual completed views, then
aggregates those values for metrics reports. Its historical metrics API describes
aggregation for long-term reporting. Therefore, retaining a buffering rate does
not imply retaining every buffering event. The 2024 launch explicitly described
13 months for both engagement and QoE; the 2025 general-availability announcement
explicitly guarantees QoE with selected dimensions. Confirm the exact supported
engagement queries for a contracted long-term plan.
[Metric definitions](https://www.mux.com/docs/guides/understand-metric-definitions),
[historical metrics API](https://www.mux.com/docs/api-reference/data/metrics),
[2024 announcement](https://data.mux.com/blog/mux-data-now-with-long-er-term-metrics).

Bitmovin's official May 2022 support answer says the then-default retention was
90 days. That answer explains older search results but should not override the
current pricing page's 30-day allowance. Its April 2026 report says hourly
historical dashboard data remained available while minute-level data and some API
queries were affected. This provides evidence of different data representations;
it does not establish their retention periods or an engagement/QoE split.
[Historical support answer](https://community.bitmovin.com/t/how-long-does-bitmovin-analytics-retain-data-queryable-via-api/697),
[incident report](https://status.bitmovin.com/history/3).

## Raw exports and event detail

- Mux's daily CSV exports contain individual views and their dimensions. Files
  remain downloadable for seven days after generation. This download window is
  separate from its 100-day data retention. Streaming exports send completed
  views, metadata, metrics and each view's event timeline to customer-owned Amazon
  Kinesis or Google Pub/Sub. Customers can store these exports for longer.
  [Mux export documentation](https://www.mux.com/docs/guides/export-raw-video-view-data)
- Bitmovin's official June 2022 export explanation says its exports contain the
  collected high-detail analytics samples, subject to the customer's retention
  window. It excluded error-detail/network-explorer information at that time.
  Current product documentation advertises full exports and integrations with
  S3, Google Cloud Storage and Azure Storage. Current pricing includes up to ten
  full exports per month. Treat the older detail exclusions as historical until
  confirmed for the current export schema.
  [Export support explanation](https://community.bitmovin.com/t/export-api-bitmovin-analytics/908),
  [current product capabilities](https://bitmovin.com/video-observability-analytics),
  [current pricing](https://bitmovin.com/pricing/)

The reviewed public sources do not establish a separate retention period for
every internal raw ingestion buffer, network trace or backend event table.
Mux's exported view records should not be equated with the original ingestion
messages. Vendor policy supports keeping detailed records for a bounded period
and retaining useful metrics longer, but it does not prove that either vendor
saves storage by expiring every technical metric before engagement data.

## DuckDB and S3

DuckDB can read and write Parquet files directly in S3 through its httpfs
extension, including partitioned exports. It can read only the relevant Parquet
columns and ranges rather than downloading the whole dataset first.
[S3 support](https://duckdb.org/docs/current/core_extensions/httpfs/s3api)
[Partial reading](https://duckdb.org/docs/current/core_extensions/httpfs/https)

This is different from putting the active analytics.duckdb file in S3.
DuckDB can attach a native database there only for reading; it cannot update
that file in place through the S3 API.
[Remote database limitation](https://duckdb.org/docs/current/guides/network_cloud_storage/duckdb_over_https_or_s3)

S3 general-purpose buckets have no maximum aggregate size or object count.
Storage, requests and applicable transfer charges still accumulate.
DuckDB still needs compute, memory and temporary disk for some queries.
[S3 capacity](https://docs.aws.amazon.com/AmazonS3/latest/userguide/BucketRestrictions.html)
[S3 pricing](https://aws.amazon.com/s3/pricing/)
[DuckDB query resources](https://duckdb.org/docs/current/guides/performance/how_to_tune_workloads)

Partitioning by commonly filtered dates and selecting only required columns
reduces remote reads. Export batches rather than one object per event.
DuckDB recommends at least 100 MB per partition as a general guideline,
not a minimum object size.
[Remote query tuning](https://duckdb.org/docs/current/guides/performance/how_to_tune_workloads)
[Partitioned writes](https://duckdb.org/docs/current/data/partitioning/partitioned_writes)

DuckLake is another option if transactional tables on object storage become
necessary. It adds a SQL metadata catalog alongside Parquet files. Its tables
do not support primary-key or UNIQUE constraints, so adopting it would require
reworking our event deduplication, not just changing a storage path.
[DuckLake introduction](https://ducklake.select/docs/stable/duckdb/introduction)

## Current collector

The installation defaults to 30-day retention based on receipt time. Projects
can override that duration, disable expiry or inherit the default. Administrators
can change an override live on Linux/macOS through `project-retention`. The
collector deletes events and their original OTLP context together. It does not retain
a separate historical aggregate after expiration.
Sources: [configuration](src/config.rs), [storage and summary queries](src/store.rs),
[retention documentation](README.md#retention).

The events table stores the complete event JSON plus extracted columns.
Browser context also appears in a separate context column, and OTLP ingestion
retains its original representation in event_otlp. These are candidates for
measuring storage savings; the duplication does not imply a known compression
ratio. The current summary reads retained start and latest snapshot/end events,
so shortening raw retention alone also shortens dashboard history.
Sources: [schema and summaries](src/store.rs), [database layout](README.md#database-layout).

The SDK already includes cumulative watch time, buffering count and duration,
startup time and other metrics in view snapshots/end events. A compact per-view
record can preserve both engagement and QoE totals after detailed transitions
expire. Missing end events require snapshot fallback, and late arrivals require
a correction policy. Do not sum cumulative snapshots.
Source: [SDK view metrics and emission](../../packages/analytics/src/core.ts).

Deletion does not necessarily shrink the native DuckDB file immediately.
Checkpoints can reclaim space; VACUUM does not reclaim deleted storage.
Copying into a fresh database is a documented compaction option.
Measure steady-state file size with real expiration before setting disk budgets.
[Space reclamation](https://duckdb.org/docs/current/operations_manual/footprint_of_duckdb/reclaiming_space)

## Suggested retention design

These are starting points for our product, not vendor policies or measured
capacity guarantees.

| Representation | Suggested retention and location | Preserved capability |
| --- | --- | --- |
| Recent raw events | A bounded local window measured in hours, plus recovery headroom | Live ingestion, recent debugging |
| Archived raw events | 7 to 30 days in compressed, date-partitioned Parquet on S3 | Detailed investigations and reprocessing |
| Compact per-view records | 90 days initially, with S3 for older partitions as volume requires | Watch time, completion, startup, buffering totals, errors and selected dimensions |
| Hourly/daily aggregates | 13 months or longer, with selected dimensions | Engagement and QoE trends |

The main reduction comes from fewer retained records and less repeated context.
Deleting every technical metric is unnecessary. For example, a view can retain
its total buffering duration and count without retaining every buffering
transition. Per-view summaries cannot preserve every diagnostic detail or
reconstruct a complete timeline.

Build summaries before expiring raw events. Publish and verify archive batches
before removing local copies. Track which batches are published so queries and
retrying exports do not count both local and archived copies. Bound the local
queue and archive backlog for S3 outages. Preserve the existing viewer-deletion
behavior across any new archive and summary stores.

For aggregate correctness, retain sums and denominators for weighted rates.
Daily distinct-viewer counts cannot simply be added into a monthly unique count;
choose retained identities or a suitable mergeable approximation. Percentiles
also need retained distributions or suitable sketches.

At a continuous 3,000 events/s, ingestion produces 259.2 million events per day.
Using the existing benchmark's mean 1,666-byte request body gives about 431.8 GB
per day before storage compression or database overhead. This arithmetic is
not an on-disk size estimate. A local window must follow a measured byte budget.
Source for the benchmark inputs: [sizing research](sizing-research.md).

Recommended first implementation direction: keep bounded local ingestion,
materialize compact per-view records and aggregates, and export raw history in
Parquet batches to S3. Validate archive throughput, query latency, crash recovery
and physical disk reclamation before claiming that it solves production sizing.
