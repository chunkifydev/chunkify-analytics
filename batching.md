# Bounded write batching

Measured on 2026-10-17. This follows the prepared-statement experiment in
[performance.md](performance.md). Measurements use a shared Apple M2 development
machine and local Docker volumes. They are not production capacity guarantees.

## Implementation

The database worker combines concurrent requests into a transaction bounded by
100 records, 1 MiB of prepared request buffers, and a 10 ms collection window.
The queue retains at most 256 waiting operations. Prepared buffers in the queue
and active batch share a 16 MiB budget. These limits are configurable.

The collection window excludes earlier queue wait and subsequent SQL execution.
A queued read or maintenance operation ends collection. Whole OTLP requests stay
together. Requests too large for a batch receive 413; admission pressure receives
503. A transaction failure fails all its requests. Success follows commit.

The default `bulk` path appends first-occurrence rows to a temporary table, runs
one event INSERT with conflict handling, and appends OTLP envelopes only for newly
inserted events. It clears staging inside the transaction. The persistent schema
stays at version 3. `row` mode keeps cached per-event INSERTs with the same
cross-request transactions, so the benchmark can separate the two improvements.

Receipt time is captured before preparation and queue admission. Duplicate event
IDs keep the first admitted payload, context, receipt time and optional envelope.
Each native request receives its own new/duplicate result after the batch commits.

## Verification

All 35 native integration tests pass, including upgrade from the earlier binary.
Three Docker tests pass, covering non-root lifecycle, crash recovery and benchmark
accounting. Rust formatting, Clippy with warnings denied, and diff checks pass.
After the queue-size configuration was added, all six affected native checks
passed again. The measured Docker runs exercise the final image through restart.

New checks cover bounded shared transactions, the idle collection timer, byte
budget exhaustion and recovery, whole-request rejection, duplicates within and
across requests, and persistence after graceful and forced restarts. Making the
temporary data directory unwritable exercises a real WAL creation failure; all
affected requests fail and retain no data. Reads flush pending batches, and
graceful shutdown drains an incomplete batch. Empty OTLP exports remain valid.

The tests caught an empty-export bulk regression and a signal-handler readiness
race. Both are fixed. The standards and spec reviews also caught a missing deadline
check while the queue stayed busy. The worker now checks the collection deadline
before accepting another request. Both reviews have no remaining findings.

### Standards

No remaining findings. The review covered the collector and benchmark changes
after `cc04fec`, excluding the user's demo page. The collection-timing concern
was fixed and checked again. No additional correctness or code-structure issues
were reported.

### Spec

No remaining findings. The missing collection-deadline check was fixed. The review
confirmed per-request results after commit, whole-request atomicity, preservation
of first-occurrence data, bounded buffers, and FIFO handling of reads and maintenance.

## Measurement method

Each case uses a disposable container limited to two CPUs, 2 GiB RAM and no extra
swap. No build or integration suite runs during a probe. Unrelated containers
remain running and can affect measurements. Dashboard reads and retention scans
run concurrently. Profiling is enabled on both comparison variants.

The native endpoint receives one event per request. OTLP cases explicitly state
their record count. The admission cap is raised to 10,000 requests/s for these
measurements; the server default remains 100. The initial generator allows 128
concurrent requests and reports omitted traffic separately from HTTP errors.
Follow-up runs explicitly record a higher concurrency limit where used.

Every completed case restarts its collector and checks all acknowledged records,
original event and OTLP payloads, pagination, summaries and per-video totals.
Browser-enriched runs also check flat context and request IP. Chromium captures
context through the actual Chunkify adapter once and closes before timed traffic.
The benchmark then replays that context with fresh event and view identities.
The first enriched native runs include null browser fields. The initial mixed
fixture then exposed an expectation mismatch: the existing OTLP adapter omits
null custom fields such as `device_family`, while native JSON retains them.
That failed fixture run is preserved separately and contributes no performance
result. The corrected mixed fixture omits null browser fields in both replay
formats and records `browser_context_nulls_omitted: true`. SDK and collector
semantics are unchanged. Browser context in OTLP uses an explicit benchmark
mapper; the OTLP adapter itself still does not collect browser information.

Version 2 profiling counts queue wait per request and worker execution per batch.
`prepare_request` runs before admission, outside worker execution. Commit timing
covers the DuckDB call, without isolating storage synchronization. Aggregate wall
times include host scheduling delays. Rejected requests have no worker timing.

## Results

The [raw reports](batching-2026-10-17.json) preserve all 15 completed probes,
configuration, image identities, source hashes, resource samples and verification
results. They also preserve the rejected mixed fixture run separately. Across
completed probes, 435,546 acknowledged unique records, including preload, survived
restart with their expected payloads and summaries. Peak sampled container memory
was 501.98 MiB. All disposable benchmark containers were removed afterward.

The first comparisons used a 64-operation queue. All rows below ran for 15 seconds
with profiling and concurrent dashboard queries. A successful request can contain
multiple events, as stated in the workload column.

| Write path | Workload, offered rate | Successful requests/s | New events/s | Successful p95, ms | HTTP 503 | Generator omissions | Probe thresholds |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- |
| Previous cached INSERT, transaction per request | Native, 1,000/s | 530.96 | 530.96 | 165.59 | 6,967 | 0 | Failed |
| Shared transaction, row INSERT | Native, 1,000/s | 818.26 | 818.26 | 219.25 | 744 | 1,913 | Failed |
| Shared transaction, bulk INSERT | Native, 1,000/s | 997.23 | 997.23 | 23.97 | 19 | 0 | Passed |
| Shared transaction, bulk INSERT | Native, 2,000/s | 1,964.59 | 1,964.59 | 27.07 | 450 | 0 | Failed |
| Shared transaction, bulk INSERT | Native, 4,000/s | 3,248.75 | 3,248.75 | 41.55 | 1,963 | 9,196 | Failed |
| Shared transaction, row INSERT | OTLP, 100/s with 10 records | 95.62 | 956.24 | 702.91 | 2 | 0 | Failed |
| Previous cached INSERT, transaction per request | OTLP, 100/s with 10 records | 79.91 | 799.06 | 905.64 | 122 | 3 | Failed |
| Shared transaction, bulk INSERT | OTLP, 100/s with 10 records | 99.92 | 999.25 | 35.88 | 0 | 0 | Passed |

Sharing transactions alone reduced commit frequency, but the native row path spent
11.69 of its 12.76 seconds of worker execution in event INSERTs. Bulk insertion
reduced that repeated work. At 2,000 offered native requests/s, the bulk worker
spent 4.08 seconds executing 1,019 transactions over a 15-second load window.
Commit calls occasionally took 151.50 ms, enough to fill the 64-operation queue.
The prepared-buffer footprint at that traffic level was far below 16 MiB.

This motivated a configurable queue with a default of 256 waiting operations.
The transaction bounds and 16 MiB write-buffer budget remain the same. On repeat
15-second probes, the larger queue returned no HTTP errors at either offered rate.
It achieved 1,913.15/s at 2,000 offered and 3,567.68/s at 4,000 offered. However,
the generator omitted 1,238 and 6,358 requests, respectively, mostly because it
reached its 128-request concurrency bound. Neither probe passes the delivery
threshold. The shared host also varies, so the higher queue alone does not prove
a particular throughput gain.

The passing threshold allows up to 1% HTTP errors and up to 1% omitted scheduled
requests, with ingest p95 at most 250 ms and query p95 at most 500 ms. It also
requires no query failures and schedule-lag p95 at most 20 ms. Passing a short
probe does not establish sustained capacity or imply zero errors.

### Longer test with browser context

At 2,000 offered native requests/s for 60 seconds, using the 256-operation queue
and 256 generator concurrency, 98,532 of 120,000 requests were submitted.
The collector acknowledged 98,458 and returned 74 HTTP 503s. Achieved throughput
was 1,639.05/s, with successful p95 of 145.01 ms and p99 of 1,028.88 ms.
The 21,468 omitted requests and query p95 values of 679.99 to 788.25 ms fail the
probe thresholds. All acknowledged events and their browser/IP context survived
restart and matched summaries.

Commit calls consumed 16.61 of 28.35 seconds of worker execution, with a maximum
of 1.30 seconds. Event INSERTs consumed 8.14 seconds and also had a long outlier.
Mean container CPU was 0.73 of its two allocated cores. Peak sampled container
memory was 501.98 MiB, including filesystem cache. These measurements identify
commit and database pauses as the next area to investigate. They do not isolate
disk synchronization, automatic checkpointing, or host scheduling from one another.

The 1,000/s browser-enriched follow-up also ran for 60 seconds. It submitted
57,170 of 60,000 requests, acknowledged 57,167 and returned three HTTP 503s.
Throughput was 952.10/s, successful p95 was 74.37 ms, and query p95 stayed below
79 ms with no query errors. Commit calls peaked at 746.80 ms. The 2,830 omitted
requests still fail the delivery threshold. All acknowledged records survived
restart. Neither longer run establishes sustained operating capacity.

### Mixed traffic and recovery

These 15-second cases use the corrected browser-context fixture and final
256-operation queue. The preloaded case starts with 20,000 events; the lower
preload keeps the run within the available disk headroom. This is still a small
retained dataset.

| Workload | Successful requests/s | New events/s | Successful p95, ms | HTTP errors | Generator omissions | Probe thresholds |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Mixed native/OTLP, gzip, 16 KiB mapper fields, duplicates | 49.97 | 219.87 | 33.23 | 0 | 0 | Passed |
| Mixed native/OTLP and duplicates after 20,000-event preload | 99.88 | 439.48 | 122.40 | 0 | 0 | Passed |
| Native burst and recovery | 675.37 | 675.37 | 51.94 | 0 | 361 | Failed |

All query p95 values stayed below 500 ms with no query errors in these cases.
The burst case moved from 100 to 1,600 and back to 100 offered requests/s.
Its overall delivery threshold failed, but the recovery phase sent and
acknowledged all 600 requests with p95 of 28.30 ms and no omissions. Each phase
drains its in-flight requests before the next phase; this measures recovery
after that drain, not immediate recovery under a continuous changing schedule.

## Reproduce

Build the current source, then run cases sequentially after builds and tests finish:

```sh
docker build -t chunkify-analytics-collector:batched-queue services/analytics-collector
COLLECTOR_IMAGE=chunkify-analytics-collector:batched-queue \
COLLECTOR_LOAD_PROFILE=1 \
COLLECTOR_LOAD_CASES=native-1000,native-2000 \
COLLECTOR_LOAD_SECONDS=15 \
COLLECTOR_LOAD_REPORT=/tmp/collector-batching.json \
node tests/collector/docker/load.mjs
```

Use `COLLECTOR_LOAD_INSERT_MODE=row` or `bulk` and
`COLLECTOR_LOAD_QUEUE_MAX_REQUESTS=64` to repeat the first comparison. Set
`COLLECTOR_LOAD_BROWSER_CONTEXT=1`, `COLLECTOR_LOAD_CONCURRENCY=256` and
`COLLECTOR_LOAD_SECONDS=60` for the longer enriched tests. Chromium must be
installed for that fixture. Other benchmark controls are in the README.

The prior one-transaction-per-request image is preserved locally as
`chunkify-analytics-collector:profile-cached`. Building the current tree under that
tag would replace the comparison executable. Image IDs in each raw report identify
the tested builds; local source hashes describe the checkout, not a separately
supplied old image. Builds used uncommitted source after `cc04fec`.

## Next performance work

The follow-up [checkpoint investigation](checkpointing.md) measures 180-second
browser-enriched runs and compares checkpoint thresholds. Consult it alongside
these earlier short probes when selecting an operating rate.

Keep bulk insertion and bounded admission. Profile the long database commit
pauses on an isolated deployment with sufficient disk headroom before raising
CPU or RAM. The measured CPU and memory use do not establish either allocation
as the bottleneck. The current worker serializes commits and reads, so a long
write also delays dashboard responses.

Separate checkpoint work, storage synchronization and host scheduling in that
investigation. Compare any change with the same payloads, retained dataset,
query load and offered rate. Then run a longer test against a representative
retention volume. These small datasets do not establish 30-day capacity, and the
fourteen unrelated containers and limited free disk make this Mac unsuitable
for certifying a production throughput target.
