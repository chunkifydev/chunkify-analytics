# Write-path profiling and first optimization

Measured on 2026-10-17. These are diagnostic probes on a shared development
machine. They do not establish a production service level or sustained capacity.
This report preserves the first optimization, which reused prepared INSERT
statements. Its version 1 profiles and transaction behavior describe that earlier
snapshot. The subsequent batching experiment is documented in [batching.md](batching.md).

## Setup

- Apple M2, eight logical CPUs, Docker Desktop 27.5.1, local Docker named volumes.
- Each collector container has two CPU cores, 2 GiB RAM, and no extra swap.
- Fourteen unrelated containers remained running. Tests and builds completed
  before each load sequence. Shared-host load still varies between measurements.
- Native requests contain one SDK event. OTLP requests in `otlp-batch10` contain
  ten SDK events. IDs are fresh and payloads come from the existing seven-event
  playback fixture. These cases contain no duplicate traffic or preload.
- IP context storage is enabled. These fixtures use the HTTP/OTLP adapters and
  do not include the Chunkify adapter's browser context. Browser-enriched traffic
  and larger retained datasets still need separate measurements.
- The rate cap is raised to 10,000 requests/s, the generator allows at most 128
  requests in flight, and retention scans run every five seconds. Dashboard reads
  run concurrently except in the explicitly marked ingestion-only comparison.
- The database schema, indexes, queue, and transaction boundaries are the same in
  both variants. Success responses still follow commit.

The baseline image includes profiling but still calls `Connection::execute` for
each INSERT. The optimized image calls `prepare_cached(...).execute(...)` for
the event and original OTLP payload INSERTs. The pinned Rust driver prepares a
statement on every `Connection::execute`; caching reuses its prepared statement
handle. Parameters are still bound separately for each event.

## What the profile measures

`COLLECTOR_PROFILE=true` emits one JSON record to stderr when the database worker
drains on graceful shutdown. It contains fixed-name counters and aggregate times,
without payloads, identities, IP addresses, or credentials. Profiling is disabled
by default. The benchmark enables it in both compared variants.

Queue wait is measured from submission to worker execution. Execution time is
measured inside the worker. INSERT timing includes preparation or cache lookup,
binding, and execution. Commit timing measures the entire DuckDB commit call;
it does not identify disk synchronization separately. Record preparation measures
context/payload serialization and occurrence-time parsing inside the worker.

The counters cover the process lifetime, including health checks and retention.
They exclude HTTP parsing, schema initialization, and the final checkpoint. Queue
depth and internal timing percentiles are unavailable. Only admitted jobs have
queue/worker timings, so rejected dashboard requests do not contribute execution
time. HTTP rejections remain visible in each benchmark result.
Durations are wall-clock measurements and include time when the host deschedules
the worker. They are not CPU-time measurements.

## Initial diagnosis

In the first profiled native run at 800 offered requests/s, the database worker
spent a mean 1.886 ms per ingestion request. Event INSERTs accounted for 54.2% of
that time and commits for 43.4%. Record preparation accounted for 0.43%.
Mean queue wait was 119.3 ms. This explains why HTTP latency grows under load even
though each admitted database operation only takes a few milliseconds.

The same baseline with dashboard reads disabled achieved 498.36 successful
requests/s, versus 524.81 with reads enabled. This does not show that disabling
queries makes ingestion slower; shared-host variation is large enough to matter.
It provides no evidence that these dashboard queries are the main bottleneck in
this small-dataset test. Admitted read queries consumed under 1% of the worker's
measured execution time in the first run. Some reads were rejected under overload.
This conclusion does not extend to a large retained dataset or heavier queries.

## Results

The [raw reports](performance-2026-10-17.json) preserve all twelve cases, their
image IDs, configuration, generator omissions, resource samples, and verification
results. The comparable profiled runs below start with empty databases and run
dashboard reads concurrently. Throughput includes the request drain period.

| Variant and case | Duration | Offered requests/s | Successful requests/s | New events/s | Successful p95, ms | Ingest 503s |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Baseline, native | 15 s | 800 | 524.81 | 524.81 | 161.74 | 4,027 |
| Cached, native | 15 s | 800 | 584.66 | 584.66 | 150.38 | 3,166 |
| Baseline repeat, native | 15 s | 800 | 377.49 | 377.49 | 336.44 | 6,147 |
| Cached repeat, native | 15 s | 800 | 524.80 | 524.80 | 240.36 | 3,989 |
| Baseline, OTLP batch of 10 | 15 s | 100 | 75.08 | 750.83 | 1,154.12 | 315 |
| Cached, OTLP batch of 10 | 15 s | 100 | 83.04 | 830.44 | 960.01 | 153 |
| Cached, native | 15 s | 400 | 399.86 | 399.86 | 26.42 | 0 |
| Cached follow-up, native | 60 s | 400 | 377.02 | 377.02 | 228.07 | 1,281 |
| Cached, native | 15 s | 1,000 | 489.57 | 489.57 | 191.79 | 6,756 |

Caching improves achieved throughput in both native comparisons, but run-to-run
variation prevents a precise percentage claim. In the first comparison, mean
event INSERT time falls from 1.022 to 0.850 ms and commit time stays near 0.8 ms.
The optimized native worker still spends roughly half its time in INSERT and
half in commit. OTLP still performs one event INSERT and one envelope INSERT
per new record, so it has not become a bulk insertion path.

The 15-second cached run at 400 requests/s passes all probe thresholds. The
60-second follow-up fails, with 1,281 ingestion rejections and 15 query failures.
Therefore 400 requests/s is not established as reliable operating capacity. The
1,000-request/s case also fails; the generator omits 827 of 15,000 scheduled
requests, including 735 late requests, so it is not a clean saturation estimate.
It cannot support a capacity promise. No sustained operating rate is established
by this investigation.

All 108,240 acknowledged records across the twelve cases survived container
replacement with their full payloads and original OTLP envelopes, and matched
global/per-video summaries. Maximum sampled container memory was 245.3 MiB.
These diagnostic cases use graceful shutdown; the separate Docker accounting
test verifies abrupt-stop durability and duplicate accounting.

## Reproducing the comparison

The local image tags are `chunkify-analytics-collector:profile-baseline` and
`chunkify-analytics-collector:profile-cached`. Their immutable IDs are in each
raw report. These builds were made with uncommitted source after `cc04fec`;
the report also records optimized source-file hashes and the baseline store hash.
The [small variant patch](performance-prepared-statements.patch) contains the
entire difference between those two instrumented snapshots. It is historical and
does not apply to the current source, which now includes batching. Rebuilding this
checkout produces the batched version. To repeat the earlier measurements locally,
use the preserved image IDs in the report and verify that each tag still resolves
to its recorded image. The images have not been published.

Run the same command once per image, sequentially, after builds/tests finish:

```sh
COLLECTOR_IMAGE=chunkify-analytics-collector:profile-cached \
COLLECTOR_LOAD_PROFILE=1 \
COLLECTOR_LOAD_CASES=native-800,otlp-batch10 \
COLLECTOR_LOAD_SECONDS=15 \
COLLECTOR_LOAD_REPORT=/tmp/collector-profile-cached.json \
node tests/collector/docker/load.mjs
```

Use `COLLECTOR_LOAD_QUERIES=0` with `native-800` for the ingestion-only comparison.
Use `COLLECTOR_LOAD_CASES=native-400` and `COLLECTOR_LOAD_SECONDS=60` for the longer
follow-up. Normal Docker connection configuration applies. The shared-host runs
here used the existing local Docker socket. The runner creates and removes its
own containers and volumes and keeps its existing memory/disk safety limits.

## Verification and review

The 26 collector integration tests and two Docker benchmark-accounting tests pass.
They cover deduplication, OTLP atomic validation, context preservation, migration,
retention, summaries, and restart durability. Rust formatting and Clippy with
warnings denied pass. The profiling test verifies counters, secret-free output,
disabled profiling, and data persistence through the public HTTP/CLI interfaces.

### Standards

One test synchronization finding was fixed: the process helper now waits for
`close`, including for an already exited process, so stderr is drained before
the test checks the shutdown report. No other actionable findings were reported.

### Spec

No actionable findings in the profiling and prepared-statement slice. This work
does not claim to complete cross-request batching or capacity certification.

## Experiments identified at this stage

1. Group several admitted requests into a bounded transaction, with limits for
   records, bytes, collection delay, and total queued memory. Preserve each
   request's result and whole-request OTLP atomicity. Acknowledge only after commit.
2. Compare an efficient bulk insert with the cached per-row loop. Preserve the
   first accepted occurrence of each event, flat context, original OTLP data, and
   pagination.
3. Recheck low-volume latency, overload recovery, query/retention fairness, shutdown,
   and restart durability. Measure with browser context and pre-populated datasets.
4. Once the software path is improved, compare CPU/RAM allocations using identical
   traffic and datasets. More cores cannot remove serialized work by themselves;
   measure the benefit before selecting a larger machine.

Issue #381 already describes bounded batching, bulk-write comparisons, durability,
and capacity measurement. There is no reason to replace those requirements with a
throughput promise yet. 1,000 single-event requests/s is an exploratory benchmark
point. A deployment target also needs an error budget, latency requirement, event
size, query load, retention volume, and hardware. Sustaining 1,000 events/s means
86.4 million events per day, which needs a separate storage and query-capacity plan.

DuckDB's [insertion guidance](https://duckdb.org/docs/lts/data/insert) recommends
avoiding row-by-row INSERT loops for bulk ingestion. Its
[Rust import documentation](https://duckdb.org/docs/current/clients/rust/data_import)
describes the Appender option. Those support testing a bulk path; they do not
predict the throughput of this collector or resolve its duplicate-handling rules.
