# Durable ingestion queue prototype

This experiment tests the user-approved proposal to acknowledge durable receipt
independently of DuckDB replay. It remains opt-in; it is not a production default.

## Implementation

The same Rust process embeds SQLite as a bounded queue before the DuckDB writer.
A producer groups incoming requests for up to one millisecond and acknowledges
only after a SQLite transaction commits with `synchronous=FULL`. A consumer reads
bounded batches, releases the SQLite lock, waits for DuckDB to commit, and then
removes the queue entries. It never holds a SQLite transaction during DuckDB work.
With `COLLECTOR_EXPERIMENTAL_REPLAY_PIPELINE=true`, the consumer prepares at most
one successor while the current write runs. The default is sequential replay. Both
prepared batches share the existing writer byte budget; smaller budgets fall
back to sequential replay. The successor is submitted only after the preceding
write and queue deletion succeed. `COLLECTOR_DUCKDB_THREADS` optionally limits
DuckDB parallelism, shared by the writer and reader. It defaults to DuckDB's
automatic choice. Measure the pipeline together with this setting and the existing
request/byte limits; more overlap can increase CPU contention.

A native response is HTTP 202, meaning durable receipt for later processing.
OTLP keeps the standard successful HTTP 200 export response. Events can become
visible to reads after acknowledgement. Replay preserves the original receipt
time, complete event, original OTLP envelope, browser fields and captured IP.
Event-ID deduplication in DuckDB handles replay after a crash between the DuckDB
commit and queue deletion. It preserves the first stored payload and context.

The queue defaults to 64 MiB of pending serialized payloads. SQLite file pages and
WAL add disk overhead; the database page limit and automatic WAL checkpointing
remain enabled. The configured prepared-write limit also constrains admission and
replay grouping. A full queue rejects new requests with HTTP 503. Worker failure
stops admission and makes health fail while preserving pending events. Resolving
the problem and restarting is required; this prototype has no worker retry loop.

The authenticated `/v1/ingestion` endpoint exposes pending requests, events,
serialized bytes, oldest age and failure state. Startup drains recovered entries
before serving traffic or performing retention/viewer deletion. Disabling queue
mode with unprocessed entries fails startup.

See [configuration and response details](README.md#experimental-durable-queue).

## Correctness checks

Five new integration tests exercise the running Rust server with real SDK events:

- Acknowledgement while DuckDB is still waiting for its batch, then SIGKILL and
  recovery, duplicate handling, original receipt time and metadata preservation.
- Queue saturation, explicit rejection, draining and subsequent acceptance.
- OTLP recovery and offline viewer deletion after pending events are recovered.
- Admission and replay with a small prepared-write budget, including rejection
  before acknowledgement when JSON fits but the prepared row does not.
- The post-DuckDB-commit/pre-dequeue crash window, recreated using a nonempty
  online SQLite backup. Repeated replay keeps one event and the first context.

A Rust test injects a deferred foreign-key violation into the test queue so the
INSERT succeeds and COMMIT fails. It exercises the producer's response path and
checks that no success or committed queue row remains. This is a commit failure
check, not a simulation of power loss.

The existing integration suite passed its executed direct-ingestion cases. The
optional previous-executable migration case was not enabled. Docker benchmark
accounting checks also passed on the final image. Formatting and Clippy with
warnings denied passed.

## Comparison method

Both conditions use the same final image, two CPUs, 2 GiB RAM, one enriched native
event per request, and the existing three dashboard endpoints. The target is
3,000 ingestion requests/s for 30 seconds, with 50,000 preloaded OTLP events.
Both use a 4,096-request bound, 16 MiB prepared-write budget, batches of up to 100
events or 1 MiB with a 10 ms collection window, a 64 MiB DuckDB checkpoint trigger,
and 10,000 requests/s admission limit. The queue's producer adds its own one
millisecond group-commit window when enabled.

The runner preloads the direct condition once, stops the collector, and copies
its checkpointed data volume into a disposable fixture. The direct collector
restarts from that state, and the queue condition starts from a copy of it. Both
timed generators restart their event sequence and duplicate policy identically;
fresh identities and timestamps still differ. The second condition skips preload
HTTP requests. Profiles for both traffic runs exclude preload because the
collectors start after fixture creation.

Queue metrics are sampled in addition to the three dashboard reads. The runner
records pending work after all HTTP requests complete, then measures observed
drain time from that point. It records resource samples during draining too.
A queue that fails to drain within 20 seconds fails the run. Safety stops remain
failures and still proceed to acknowledged-event verification where possible.
If verification cannot complete, its queued-data volume is retained rather than
deleted. Graceful restart verification checks every acknowledged event and the
complete payloads, pagination, metadata and summary calculations.

The shared M2 Mac uses Docker Desktop. The 13 development containers are stopped
for the comparison and restored afterward. Docker is not restarted. Explicit
Node garbage collection runs between the two conditions, outside measured traffic.
All existing memory, disk, request-delivery and latency gates remain in place.

## Results

Measured on 2026-10-19. The [raw report](durable-queue-2026-10-19.json) includes
request accounting, queue samples, resource measurements and service restoration.

| Metric | Direct DuckDB acknowledgement | Durable queue acknowledgement |
| --- | ---: | ---: |
| Scheduled requests | 90,000 | 90,000 |
| Sent and successful | 89,598 | 89,904 |
| Generator omissions, late | 402 | 96 |
| HTTP errors | 0 | 0 |
| Ingestion p50 | 19.10 ms | 4.40 ms |
| Ingestion p95 | 883.87 ms | 14.89 ms |
| Ingestion p99 | 1,284.26 ms | 116.76 ms |
| Summary p95 | 642.86 ms | 637.18 ms |
| Video-list p95 | 708.05 ms | 233.91 ms |
| Event-list p95 | 677.34 ms | 626.25 ms |
| Longest DuckDB commit | 1,237.49 ms | 1,128.08 ms |
| Mean container CPU cores | 0.85 | 1.02 |
| Peak container memory | 924.42 MiB | 922.43 MiB |
| Peak generator memory | 742.83 MiB | 723.64 MiB |
| Full benchmark pass | No | No |

Both runs execute 24 requests to each dashboard endpoint, 72 dashboard reads in
total. Queue mode additionally samples its ingestion-status endpoint. All HTTP
requests actually sent succeed; omissions never reach the collector. Neither
condition omits requests due to the generator concurrency limit. There are no
safety stops or failed dashboard HTTP responses.

Queue-mode ingestion meets the 250 ms p95 target. Its summary and event-list p95
still exceed the 500 ms read target, and its 96 omitted scheduled requests prevent
a full-delivery pass. The direct run fails ingestion latency, all three read
latency targets and full scheduled delivery. A completed run is not a passing run.

### Processing backlog

The largest sampled backlog is 3,621 events, 6,673,270 serialized bytes, about
6.36 MiB. The oldest pending request reaches 1,206 ms. Spikes fall back to hundreds
or tens of pending events during continued traffic. After all HTTP requests
complete, 18 events remain, with oldest age 34 ms. The queue is observed empty
165.84 ms after drain measurement starts. This includes status polling and
resource-sampling overhead; it is not a precise per-event visibility measurement.
Sampled peaks can miss shorter spikes.

DuckDB commits still take more than a second. The queue substantially reduces the
HTTP wait while those pauses remain. The backlog catches up in this short run;
it does not show sustained, unbounded growth here. Thirty seconds cannot establish
long-term capacity or behavior with weeks of retained events.

### Persistence and restoration

After graceful restart, the direct database verifies 139,598 acknowledged events
and the queued database verifies 139,904, each including the same 50,000-event
preload. All complete payloads, browser/IP fields, OTLP envelopes, pagination and
summary checks pass. No persisted events lack an acknowledgement. The separate
integration tests cover abrupt process termination and replay of the crash window.

All 13 development containers are restored, with Postgres and LocalStack healthy.
No disposable benchmark containers or volumes remain.

## Conclusion

Keep the durable queue as an opt-in prototype. This pair supports the architectural
direction: durable HTTP acknowledgement can stay responsive during DuckDB pauses,
and the consumer catches up within the measured workload. It does not establish a
fully passing 3,000 requests/s service or a production capacity guarantee.

The next implementation candidate is independent read execution, followed by a
focused comparison of the affected read latency. Shared database and storage
contention can remain. Generator omissions also remain a separate limit on a
full-delivery claim. Further broad configuration sweeps are not needed to show
that this prototype reduced acknowledgement latency in this pair.

The comparison uses a checkpointed preload and a fresh process for each condition,
so its timings should be compared within this pair. The older read-impact runs
continued directly after preload. Host activity, run order, differing generated
identities and the added queue-status requests remain limitations. Explicit Node
garbage collection between conditions also affects host pressure and is outside
the timed workload. Queue metrics and native 202 responses reflect durable
receipt; direct responses reflect availability in DuckDB.

## Review

### Standards

No remaining actionable findings after correcting the prepared-byte limits,
worker health reporting, commit-failure test and memory-permit release ordering.

### Spec

No remaining actionable findings after checking durable acknowledgement, replay,
fixture equivalence, queue-drain accounting and retention of unverified data. A
final evidence review confirms that the results and limitations match the raw report.

Review totals: zero remaining Standards findings; zero remaining Spec findings.
