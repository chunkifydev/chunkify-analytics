# Higher-rate ingestion probes

Measured on 2026-10-19, following the two passing three-minute
[1,000 requests/s runs](postcall.md). The [raw reports](throughput-2026-10-19.json)
preserve successful and failed runs, host memory samples, a generator diagnostic
and a crash-recovery check.

## Setup

The collector image and Rust source are unchanged throughout this comparison.
The container has two CPUs and 2 GiB of RAM. Tests run on the same M2 Mac through
Docker Desktop and loopback HTTP. The 13 development services are stopped, and
Docker is restarted once before the comparison.

Each timed native request contains one event with browser context. IP collection,
dashboard queries, retention scans and profiling remain enabled. Each run uses a
disposable database. The populated case inserts 50,000 events through OTLP before
starting timed traffic. Retention scans do not exercise deletion of expired data.

The generator permits 4,096 requests in flight. Its default remains 128. The
1 GiB generator RSS guard, 1,800 MiB container-memory guard, 512 MiB data guard,
200,000 unique-event limit and 50 ms scheduling deadline remain unchanged.
The strict pass condition requires every scheduled request to be submitted and
successful, ingestion p95 at most 250 ms, dashboard p95 at most 500 ms, no dashboard
errors and scheduling-lag p95 at most 20 ms.

All runs use a 16 MiB prepared-write budget, a maximum transaction batch of
100 events or 1 MiB, and an explicit ingestion rate limit of 10,000 requests/s.
The table identifies changes to queue length, batch delay and checkpoint threshold.
These are benchmark overrides, not changes to deployment defaults.

## Empty-database results

Each run schedules 180,000 requests. The 2,000/s runs last 90 seconds and the
3,000/s runs last 60 seconds.

| Offered requests/s | Queue | Batch delay, ms | Checkpoint, MiB | Successful / scheduled | HTTP 503 | Generator misses | Ingest p95, ms | Full pass |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 2,000 | 1,024 | 10 | 16 | 177,836 / 180,000 | 2,164 | 0 | 297.21 | No |
| 3,000 | 1,024 | 10 | 16 | 170,761 / 180,000 | 9,083 | 156 | 438.06 | No |
| 2,000 | 4,096 | 10 | 16 | 180,000 / 180,000 | 0 | 0 | 302.03 | No |
| 2,000 | 4,096 | 50 | 16 | 180,000 / 180,000 | 0 | 0 | 323.78 | No |
| 2,000 | 4,096 | 10 | 64 | 180,000 / 180,000 | 0 | 0 | 65.16 | Yes |
| 3,000 | 4,096 | 10 | 64 | 180,000 / 180,000 | 0 | 0 | 201.91 | Yes |
| 3,000, repeat | 4,096 | 10 | 64 | 179,872 / 180,000 | 0 | 128 | 201.45 | No |
| 3,000, GC diagnostic | 4,096 | 10 | 64 | 179,986 / 180,000 | 0 | 14 | 244.82 | No |

The initial 3,000/s run also has nine dashboard HTTP 503 responses. The 50 ms
batch-delay run has event-query p95 of 503.76 ms. The other completed empty-database
runs have no dashboard errors and stay below the dashboard p95 limit.

Increasing the queue eliminates the observed ingestion overload responses at
2,000/s but leaves p95 above its limit. Increasing the batch delay reduces the
transaction count from 5,545 to 1,866, but increases waiting and does not pass.

Raising the checkpoint threshold from 16 to 64 MiB, with the 10 ms delay restored,
reduces total commit time in the matched 2,000/s runs from 18.02 to 11.46 seconds.
Ingestion p95 falls from 302.03 to 65.16 ms. This supports less frequent checkpoint
work as a useful configuration change for this workload. It does not eliminate
long commits. The longest commit in the passing 2,000/s run is 1,005.78 ms.

The passing 3,000/s run completes all 180,000 requests. Its reported average is
2,995.68 successful requests/s because the denominator includes draining requests
after the 60-second traffic window. Its ingestion p99 is 666.96 ms, and its highest
dashboard p95 is 442.51 ms. The repeat and diagnostic also pass the latency gates,
but their unsent requests prevent a full-delivery pass.

## Populated-database result

After preloading 50,000 events, the 3,000/s case is scheduled for 40 seconds.
It stops early at 36.06 seconds because generator RSS reaches 1,028.64 MiB,
exceeding the 1,024 MiB safety guard. This is a failed, incomplete capacity probe.
The memory guard remains unchanged.

The collector accepts all 107,913 submitted requests with no HTTP or dashboard
errors. No requests are skipped for concurrency or scheduling lag; 12,087 planned
requests remain unsent when the guard stops traffic. Ingestion p95 is 537.70 ms
and p99 is 910.36 ms. Video-list and event-query p95 also exceed their 500 ms limit,
at 513.19 and 603.68 ms. These are observations from the shortened run, not a
passing 40-second result.

The runner verifies all 157,913 acknowledged events after restart, including the
50,000 preloaded events. Peak collector container memory is 1,101.45 MiB, below its
guard. The collector does not trigger the memory stop.

The aggregate profile includes preload work. Its longest commit is 913.48 ms.
Event-query execution reaches 105.75 ms, and queued operations can wait almost a
second. These observations identify write stalls and read contention for further
investigation. The failed run does not isolate how much each contributes.

## Generator diagnosis

The repeat misses 128 scheduling deadlines; the diagnostic misses 14. Neither
hits the generator concurrency cap, and every submitted request succeeds and is
verified after restart. These missed requests never reach the collector.

The diagnostic records 358 GC pauses across the generator process, including
setup and verification. The longest is 27.5 ms, below the 50 ms scheduling deadline.
This does not identify GC alone as the cause. Combined callback work, GC and host
scheduling can still cause a longer scheduling delay. The cause remains unresolved.
The diagnostic itself adds logging overhead and is not an uninstrumented repeat.

Two earlier diagnostic attempts fail before traffic because Docker cannot inspect
the image tag even though its image listing still includes it. Inspection and
subsequent runs succeed using the previously recorded immutable image ID. No image
is rebuilt or deleted. These startup failures are recorded separately from capacity
results.

## Candidate configuration

The improved runs use these explicit overrides:

```dotenv
COLLECTOR_QUEUE_MAX_REQUESTS=4096
COLLECTOR_QUEUE_MAX_BYTES=16777216
COLLECTOR_CHECKPOINT_THRESHOLD_MIB=64
COLLECTOR_BATCH_MAX_EVENTS=100
COLLECTOR_BATCH_MAX_BYTES=1048576
COLLECTOR_BATCH_DELAY_MS=10
COLLECTOR_INSERT_MODE=bulk
COLLECTOR_INGEST_PER_SECOND=10000
```

The queue absorbs temporary write stalls. It cannot compensate for a sustained
arrival rate above storage capacity. The byte budget can reject large requests
before the request-count limit is reached. A higher checkpoint threshold permits a
larger write-ahead log and can change restart or checkpoint duration. Successful
HTTP responses still follow transaction commit.

This profile is a candidate for further testing. The default server queue remains
256, the default checkpoint threshold remains 16 MiB and the default ingestion
rate limit remains 100 requests/s. A 3,000/s production capacity claim requires a
repeatable generator and longer runs with a substantially larger retained database.

## Crash recovery and verification

A separate check uses the tuned 4,096-request queue and 64 MiB checkpoint threshold
with mixed native/OTLP traffic, gzip, duplicate events, large payloads and browser/IP
context. All 100 requests succeed, covering 300 record acknowledgements. There are
50 new events and 250 duplicate acknowledgements, in addition to a 20-event preload.

The runner sends SIGKILL after those requests finish, restarts the container and
verifies all 70 distinct acknowledged events, complete payloads, OTLP context,
playtime totals and per-video summaries. This checks durable acknowledgements with
the tuned configuration. It does not simulate a crash while requests are in flight.

Every capacity run also verifies acknowledged events after graceful restart,
including the run stopped by the memory guard. Together they verify 1,586,368
acknowledged events, including preload events. The full raw reports retain these
checks and each failed verdict.

All 13 development services were restored. Postgres and LocalStack report healthy.
Disposable benchmark containers and volumes were removed.

## What remains

The tuned profile passes one short empty-database run at 2,000/s and one at
3,000/s. It has not established reliable 3,000/s capacity. The repeat misses
scheduled traffic, and the populated case fails both latency and generator-memory
checks. No acknowledged data loss is observed in any of these probes.

The next work should make the generator fit its memory budget with an existing
dataset, then repeat the populated workload while measuring write stalls and
query contention separately. Longer-duration testing follows once that workload
passes. Larger queues alone will not resolve the latency failure. Storage-size
optimization remains deferred.

## Review

### Standards

No findings. The CLI additions follow the existing case and validation patterns,
and the README documents the new cases and concurrency range.

### Spec

No findings. All nine capacity verdicts agree with the raw delivery, latency, query
and safety results. The crash check is limited to completed acknowledgements, and
the report makes no sustained-capacity claim.

Review totals are zero Standards findings and zero Spec findings.
