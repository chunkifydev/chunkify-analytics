# Five-minute stability attempt

This follows the [separate-reader comparison](read-worker.md). On 2026-10-19,
we attempted one five-minute run at 3,000 ingestion requests/s on the same
2 CPU / 2 GiB collector, with the durable queue and separate reader enabled.
The [raw report](stability-2026-10-19.json) contains the configuration, source
hashes, progress, resource samples, profiles and restoration checks.

**The run did not pass.** The load generator exceeded its 1,024 MiB RSS guard
after 101.189 seconds. Traffic stopped, the queue drained, and verification after
a graceful restart passed for all 344,991 acknowledged events. There were also
generator omissions and dashboard latency failures before the stop.

## Method and preparation

The collector image and source match the previous validated candidate. No
collector code or settings changed for this run. We preloaded 50,000 OTLP events,
then offered enriched native events, one per request. The planned traffic was
900,000 requests over 300 seconds. Summary, video-list and event-list requests
ran concurrently through the existing monitoring loop.

The generator allowed 4,096 in-flight requests. Collector settings remained at
100 events / 1 MiB / 10 ms per write batch, a 16 MiB prepared-write budget,
64 MiB durable pending-payload budget and 64 MiB DuckDB checkpoint trigger.
Admission remained 10,000 requests/s. The container had two CPUs, 2 GiB RAM and
no swap allowance. The 1,800 MiB container-memory guard remained active.

For the planned 950,000-event dataset, the generator event limit was explicitly
raised to 1,000,000 and the data-directory budget to 3 GiB. The 4 GiB free-disk
floor remained active, and preflight required the data budget plus that floor.
The test did not reach either raised limit.

The benchmark now stores compact UUIDs and encoded replay descriptions. During
verification it releases each description after checking the complete record,
and accumulates summaries per view. Nine focused checks passed, including Docker
accounting tests and rejection of missing events, repeated pages, altered
canonical payloads and altered OTLP context. Standards and Spec reviews found
no remaining implementation findings after two benchmark issues were fixed.

An isolated preflight retaining 950,000 descriptions and acknowledgement IDs
used 510.88 MiB RSS after garbage collection. It excluded active HTTP requests,
latency tracking and transient allocations. Live memory usage later exceeded
the guard, so that preflight was insufficient to establish generator headroom.
No explicit garbage collection ran during measured traffic.

The wrapper stopped all 13 development containers and restored them afterward.
The optional host-memory sampler initially rejected an invalid duration. It was
restarted during preload with a valid duration and covered all timed traffic.
The benchmark's own resource guards remained active throughout.

## Results

| Metric | Observed result |
| --- | ---: |
| Planned / elapsed traffic duration | 300 s / 101.189 s |
| Planned requests | 900,000 |
| Sent / successful requests | 294,991 / 294,991 |
| Requests omitted for scheduling lateness | 8,274 |
| Requests omitted for concurrency | 0 |
| Planned requests not reached before the stop | 596,735 |
| HTTP ingestion errors / rejected events | 0 / 0 |
| Successful requests/s, including HTTP drain | 2,915.24 |
| Ingestion p95 / p99 | 62.20 ms / 1,437.06 ms |
| Summary p95 / p99 | 526.18 ms / 1,479.95 ms |
| Video-list p95 / p99 | 413.42 ms / 3,823.76 ms |
| Event-list p95 / p99 | 538.28 ms / 3,813.82 ms |
| Requests per dashboard endpoint / failures | 73 / 0 |
| Scheduling-lag p95 among sent requests | 5.64 ms |
| Largest sampled pending event count | 9,504 |
| Oldest sampled pending request | 3,123 ms |
| Pending events after HTTP completion | 0 |
| Longest profiled DuckDB commit | 3,025.99 ms |
| Mean container CPU cores | 1.29 |
| Peak sampled container memory | 1,673.03 MiB |
| Peak sampled collector process RSS | 1,224.73 MiB |
| Peak sampled generator RSS | 1,057.44 MiB |
| Checkpointed data-directory size | 454.29 MiB |
| Events verified after restart | 344,991 |

The generator memory limit caused the stop. The collector stayed below its
memory guard and did not report a queue failure, overload rejection or OOM kill.
Both overall benchmark pass flags are false. The summary and event-list p95
also exceed the 500 ms target, and omitted requests prevent complete delivery.
The achieved rate above describes only the shortened run; it is not a passing
five-minute capacity result.

### Behavior over time

| Traffic interval | Successful requests | Late omissions | Ingest p95 | Summary p95 | Video-list p95 | Event-list p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| 0–60 seconds | 179,829 | 165 | 15.78 ms | 251.39 ms | 182.38 ms | 293.75 ms |
| 60 seconds until stop | 115,162 | 8,109 | 296.39 ms | 754.93 ms | 478.00 ms | 1,310.41 ms |

The second interval is partial. Scheduling buckets use due times; submission
and latency buckets use submission times, so a boundary request may move between
adjacent buckets. The monitoring loop is closed: it waits for queries and
resource sampling before sleeping, so slower responses reduce query frequency.
There were 49 reads per endpoint in the first interval and 24 in the second.

Host-wide memory samples show substantial compression and some swapping during
the slower portion. These observations do not isolate a collector bottleneck:
the generator, Docker VM and other applications shared the Mac. Collector
memory, write pauses, reader queueing and execution times also increased. The
profiles include preload in this single run, unlike the earlier comparison,
which restarted after preload. Their totals should not be compared directly.

### Persistence and cleanup

Every acknowledged record survived the graceful restart: 50,000 preloaded
events plus 294,991 new events. Verification checked complete payloads, browser
and IP context, original OTLP envelopes, pagination, global summaries and
per-video summaries. No persisted event lacked an acknowledgement.

The queue was already empty at the first check after HTTP completion. The
4.97 ms drain measurement mostly measures the status check; it is not event
visibility latency. Sampled queue and memory peaks can miss shorter spikes.
This run tests graceful restart; separate integration tests cover abrupt kills.

All 13 development containers were restored. Postgres and LocalStack were
healthy, and no disposable benchmark containers or volumes remained. The
temporary host-memory sampler was stopped. The collector image remains for
reproduction.

## Evidence review

The final Standards review found no actionable issues. The independent Spec
review checked the narrative against the raw results, source and image identity,
restoration records and host-memory samples, and found no factual discrepancies.

## Next step

The immediate blocker is live generator memory. Profile and bound its retained
state and transient allocations before repeating this same test. A file-backed
submission/acknowledgement ledger and a separate verification process are options
to evaluate while preserving complete payload verification. Keep the existing
memory guards and service latency targets.

Then repeat one five-minute run at the same rate and resources. If dashboard
latency still grows with a healthy generator and less host memory pressure,
use the reader profiles to choose the next collector change. This attempt does
not justify raising the supported capacity or enabling experimental settings by
default. The earlier short 3,000 requests/s pass remains the strongest complete
delivery result.
