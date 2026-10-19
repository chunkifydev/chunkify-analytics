# Separate dashboard read worker

This follows the [durable-queue experiment](durable-queue.md). The earlier run
kept durable ingestion responsive, while summary and event-list p95 remained
above 500 ms. This comparison changes only where dashboard reads execute.

## Implementation

`COLLECTOR_EXPERIMENTAL_READ_WORKER=true` enables one dedicated Rust thread and
one cloned connection to the same DuckDB database instance. The process still
owns one data directory and uses one writer for ingestion, retention and viewer
deletion. Shared-worker execution remains the default. The durable ingestion
queue is enabled in both measured conditions.

The reader processes `/v1/events`, `/v1/summary` and `/v1/videos`. It accepts one
executing query and at most 64 waiting queries; overflow returns HTTP 503. Calls
canceled while waiting are skipped. A query already executing runs to completion.

Each response executes its watermark lookup and result query in one transaction.
This preserves a consistent snapshot while ingestion or retention commits on the
writer connection. This design uses DuckDB's documented
[concurrency within one process](https://duckdb.org/docs/current/connect/concurrency)
and [snapshot isolation](https://duckdb.org/docs/current/sql/statements/transactions).
The deployed pinned DuckDB version was also tested directly.

A read no longer forces pending writes to flush. In direct mode, a read started
after the ingestion response sees that commit. With the durable queue, a response
still acknowledges SQLite receipt and the event becomes visible after DuckDB
replay. Pagination excludes later insertions using its existing watermark;
retention can remove records between pages.

Shutdown joins the reader, drains the durable queue, releases the last writer
handle, then waits for the writer's final checkpoint. Profiling labels the reader
and writer separately, exposing queue wait and execution times for each.

## Correctness and review

- The regression test first failed on the shared worker, then passed with the
  reader enabled: a busy ingestion worker does not block a dashboard read.
- A real DuckDB test holds a read snapshot while another connection deletes and
  inserts records. The writer commits before the reader is released; both reads
  in that transaction see the old snapshot and the next query sees the commit.
- Read failures close their transaction; subsequent reads succeed. Queue overflow
  and cancellation checks pass.
- Public HTTP tests cover real SDK native/OTLP events, direct and queued modes,
  pagination during ingestion, summaries, original metadata and restart.
- The integration suite was exercised with separate reads enabled. Its only
  failure was an old shared-worker test that expected reads to force a flush.
  That test now explicitly selects shared mode and passes; a new separate-mode
  test verifies committed-only reads without forcing a flush and passes too.
  Other executed cases passed, including retention and durable-queue crash tests.
  The optional previous-executable migration case was not configured.
- Four Rust tests, focused integration reruns, formatting, Clippy with warnings
  denied and both Docker benchmark-accounting checks passed.

### Standards review

No remaining actionable findings in the change from the start-of-turn snapshot.

### Spec review

No remaining actionable implementation findings. The final evidence review also
found no inaccuracies in the measurements, verification totals or stated limits.

## Method

The matched runs use one image on the same shared M2 Mac and Docker Desktop.
Each container is limited to two CPUs and 2 GiB RAM. Both enable the durable queue,
use 50,000 identical preloaded OTLP events, and offer 3,000 enriched native requests
per second for 30 seconds, one event per request. They also execute concurrent
summary, video-list and event-list requests in the existing monitoring loop.

The runner preloads once, waits for replay, stops the collector and snapshots the
checkpointed volume. Both measured processes start from copies of that state.
Timed event generation resets in both conditions. Profiles exclude preload.
Shared mode also uses the per-response snapshot wrapper, so only read-worker
placement changes between the two measured conditions.

Both use a 4,096-request bound, 16 MiB prepared-write budget, batches of up to
100 events or 1 MiB with a 10 ms collection window, a 64 MiB DuckDB checkpoint
trigger, 10,000 requests/s admission and 30-day retention checked every five
seconds. The 64 MiB SQLite pending-payload limit is unchanged. Query latency
includes response transfer and parsing in the load generator.

The 13 development containers are stopped for measurement and restored in a
finally block. Explicit Node garbage collection runs between conditions, outside
measured traffic. No SQL or query plans were optimized for this comparison.
All delivery, latency, resource, queue-drain and restart-verification checks remain
in force. This is one short pair, not a sustained production-capacity measurement.

## Results

Measured on 2026-10-19. The [raw report](read-worker-2026-10-19.json) records
configuration, source hashes, both worker profiles, queue samples, host memory
samples, restart verification and service restoration.

| Metric | Shared worker | Separate reader |
| --- | ---: | ---: |
| Scheduled / sent / successful ingestion requests | 90,000 / 90,000 / 90,000 | 90,000 / 90,000 / 90,000 |
| Generator omissions | 0 | 0 |
| HTTP errors | 0 | 0 |
| Successful requests/s, including HTTP drain | 2,996.32 | 2,995.81 |
| Ingestion p95 | 10.65 ms | 9.98 ms |
| Ingestion p99 | 28.59 ms | 23.36 ms |
| Summary p95 | 253.06 ms | 147.58 ms |
| Video-list p95 | 273.66 ms | 164.42 ms |
| Event-list p95 | 236.79 ms | 161.63 ms |
| Summary p99 | 435.34 ms | 171.17 ms |
| Video-list p99 | 455.58 ms | 218.88 ms |
| Event-list p99 | 580.61 ms | 215.23 ms |
| Requests per dashboard endpoint | 25 | 26 |
| Longest DuckDB write commit | 863.44 ms | 896.85 ms |
| Largest sampled pending event count | 2,927 | 1,239 |
| Largest sampled pending payload bytes | 5,395,403 | 2,284,228 |
| Oldest sampled pending request | 980 ms | 428 ms |
| Pending events after HTTP completion | 0 | 0 |
| Mean container CPU cores | 0.93 | 0.90 |
| Peak sampled container memory | 794.24 MiB | 853.40 MiB |
| Peak sampled generator memory | 713.91 MiB | 719.05 MiB |
| Full-delivery benchmark pass | Yes | Yes |

Both conditions meet the ingestion p95 target of 250 ms, all dashboard p95
limits of 500 ms, scheduling-lag p95 of 20 ms and complete request delivery.
There are no query failures, queue failures, overload rejections or safety stops.
The rates just below 3,000 include the small HTTP drain after the 30-second offered
window. All 90,000 scheduled requests were sent and acknowledged in each run.

Dashboard p95 improves by 41.7% for summaries, 39.9% for video lists and 31.7% for
event lists in this pair. The monitoring loop waits after queries and resource
sampling, so faster reads produce 78 dashboard requests in separate mode versus
75 in shared mode. This is the same query policy, not an identical read count.
Only 25 or 26 samples per endpoint contribute to these percentiles.

The fresh shared-worker baseline also passes. The earlier durable-queue experiment
had read p95 failures and generator omissions, which this pair does not reproduce.
Host variation, query arrival order and the new shared per-response transaction
wrapper may all affect cross-run results. The measured effect of separation is
the comparison within this pair, not the difference from that earlier run.

### What the profiles show

Maximum ingestion-worker queue wait falls from 275.39 ms to 14.05 ms. Dashboard
jobs no longer appear on the writer profile. The dedicated reader still serializes
reads with each other: maximum waiting time is 147.97 ms for summaries, 203.03 ms
for videos and 38.37 ms for events. Read execution remains nontrivial; event-list
execution averages about 87 ms and reaches 185.53 ms.

The approximately 0.9-second DuckDB commit pauses remain. The durable queue keeps
HTTP acknowledgement responsive during them. Reader separation removes waiting
behind writer jobs, but all workers still share storage and DuckDB resources.
These observations support the scheduling change; they do not establish that all
latency variation is caused by one mechanism.

### Backlog, persistence and restoration

The largest sampled pending backlog is lower with the reader, and both runs
catch up. Both queues are already empty at the first sample after HTTP completion.
The recorded drain measurements of 3.40 ms and 3.37 ms mostly measure the status
requests; they are not per-event visibility latency. Sampled peaks can miss spikes.

Graceful restart verification checks 140,000 events in each condition: the same
50,000-event preload plus that run's 90,000 newly acknowledged events. Every full
payload, browser/IP context, original OTLP envelope, global/per-video summary and
paginated record set passes. No persisted events lack an acknowledgement. These
totals must not be added as globally distinct events because the preload is shared.
Separate integration tests cover abrupt process termination and durable replay.

All 13 development containers are restored, with Postgres and LocalStack healthy.
No disposable test containers or volumes remain. The image remains available for
reproduction. The user's example page and manually collected data were not changed.

## Conclusion

This implementation and comparison are complete. Keep the separate reader and
durable queue available as opt-in settings. We now have a fully passing short
3,000-ingest-requests/s comparison on 2 CPU / 2 GiB, with concurrent dashboard
reads and complete acknowledged-event verification. Separate reads improve all
three dashboard p95 measurements in this pair while ingestion stays near 10 ms.

The next validation should measure longer operation and a larger retained dataset,
with emphasis on backlog recovery, memory and storage growth. One short pair on a
shared Mac does not establish sustained production capacity or a precise speedup.
