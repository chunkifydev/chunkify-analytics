# Five-minute capacity comparison with 4 GiB RAM

On 2026-10-19, the collector completed the five-minute workload at an offered
3,000 requests/s with 2 CPUs and 4 GiB RAM. **The capacity target failed.** It
returned 48,411 HTTP 503 responses, and all three dashboard query p95 values
exceeded 500 ms. Every acknowledged event survived graceful restart.

The [raw report](capacity-4g-2026-10-19.json) preserves request windows, queue and
resource samples, profiles, source hashes, host-memory samples and container
restoration evidence. The observed 2,832.35 successful requests/s is an average
under overload. It does not establish sustainable capacity at that rate.

## Configuration

The immutable image ID, collector source hashes, workload plan and service
thresholds match the [previous 2 GiB attempt](stability-retry.md). Container RAM
increased to 4 GiB, with the proportional safety guard increasing from 1,800 to
3,600 MiB. DuckDB's automatic memory limit also increased from 1.5 to 3.1 GiB.
CPU remained at 2, with no container swap. The durable queue and separate reader
were enabled, with a 64 MiB checkpoint trigger and the default 64 MiB pending
spool payload budget.

The workload preloaded 50,000 OTLP events, then offered 900,000 enriched native
events over 300 seconds, one event per request. Summary, video-list and
event-list queries ran concurrently. Each read endpoint completed 207 requests.
Retention scanned every five seconds with a 30-day TTL; no expiry workload was
measured. No explicit generator garbage collection ran during traffic.

The wrapper stopped 13 development containers before the run. The generator and
Docker still shared an Apple M2 Mac with other applications. Docker had 8 CPUs
and about 7.65 GiB RAM. The collector image was pinned to the previous capacity
implementation; the subsequently added browser/OS/device query filters were
not part of this load test.

## Results

| Metric | Observed result |
| --- | ---: |
| Offered / elapsed traffic | 300 s / 300.418 s |
| Scheduled / sent requests | 900,000 / 899,301 |
| Successful requests | 850,890 |
| HTTP 503 responses | 48,411, or 5.38% of sent requests |
| Requests omitted for lateness / concurrency | 699 / 0 |
| Successful requests/s, including HTTP drain | 2,832.35 |
| Successful ingestion p95 / p99 | 44.92 ms / 88.26 ms |
| Summary p95 / p99 | 630.41 ms / 857.78 ms |
| Video-list p95 / p99 | 531.19 ms / 700.23 ms |
| Event-list p95 / p99 | 626.55 ms / 1,043.23 ms |
| Dashboard HTTP failures | 0 |
| Scheduling-lag p95 among sent requests | 2.43 ms |
| Mean collector CPU consumption | 1.46 cores |
| Peak sampled container memory | 2,984.14 MiB |
| Peak sampled collector process RSS | 1,685.86 MiB |
| Peak sampled generator RSS during traffic and drain | 609.55 MiB |
| Largest sampled pending event count | 36,443 |
| Oldest queued event after HTTP completion | 19.453 s |
| Completed queue drain after HTTP completion | 12.787 s |
| Checkpointed data-directory size | 927.63 MiB |
| Events verified after restart, including preload | 900,890 |

Successful ingestion p95 passed the 250 ms target. HTTP failures exceeded the
1% allowance, and each aggregate query p95 failed its 500 ms target. Both
`meets_probe_thresholds` and `meets_full_delivery_thresholds` are false.
The 699 generator omissions also prevent complete delivery of the offered load.
The runner's successful exit means execution and correctness verification
completed; it does not mean the performance thresholds passed.

## Backlog and persistence

All HTTP 503 responses occurred in the final minute. Before then, the queue
grew from hundreds of pending events to tens of thousands. Its largest sampled
pending payload was 67,108,774 bytes, just below its 67,108,864-byte limit.
This supports queue exhaustion as the rejection mechanism. The benchmark does
not retain failure bodies or per-rejection reason counters, so it cannot assign
every HTTP 503 to that cause individually.

At HTTP completion, 35,118 events remained queued. The queue reached zero after
12.787 seconds, before graceful shutdown. An HTTP 202 acknowledges durable
spooling; its latency does not measure how soon DuckDB queries can see an event.
The growing backlog shows that persistence could not keep pace with the offered
rate under this mixed workload.

After restart, the verifier checked all 900,890 acknowledged events, including
the 50,000 preload events. Complete payloads, browser/IP context, original OTLP
envelopes, pagination, global summary and per-video summary passed. No persisted
event lacked an acknowledgement. Rejected requests were not retried.

## What the memory measurements show

The run stayed below its 3,600 MiB guard. At the peak cgroup sample during queue
drain, anonymous memory was about 1,659.87 MiB and file memory about 1,063.04 MiB.
Cgroup memory also includes other charges. Inactive-file and dirty-file counters
overlap file memory and must not be added to it as separate allocations.

All sampled cgroup high/max/OOM/OOM-kill event counters and memory-pressure
some/full totals were zero through traffic and queue drain. These samples show
no container memory-limit pressure during that interval. They do not establish
that the Mac host was free of memory pressure or that the old 2 GiB guard could
have been removed safely.

Across the broader host sample interval, which includes preload, verification
and restoration, macOS recorded 5,981 swapped-in pages and 49,000 swapped-out
pages at 16 KiB per page, plus compression activity. Those system-wide counters
cannot attribute activity to the collector. This remains a local comparison,
with no production capacity guarantee.

## Where to optimize next

The writer profile recorded 238.40 seconds of ingest execution. The main event
insert SQL took 155.10 seconds and commits took 66.71 seconds, about 65% and 28%
of that execution respectively. The longest commit took 2.43 seconds. These
totals include preload and drain as well as timed traffic. Preparation has a
separate timer outside ingest execution and must not be added as a component.

The writer processed 10,351 transactions containing up to 100 events each. Its
profiled request count describes replay batches, not individual HTTP requests.
The separate reader also accumulated execution and queue-wait time. It shares
the CPU budget, database and storage with the writer. These measurements do not
isolate query contention, checkpoints or insert cost as the sole bottleneck.

The next implementation step is to inspect and reduce the main event insert
cost and commit frequency while preserving deduplication and durable
acknowledgements. Use a focused comparison at the larger retained dataset to
check that change before repeating the full workload. Increasing queue capacity
alone would delay rejections and permit a larger visibility lag.

## Cleanup

All 13 development containers were restored, with Postgres and LocalStack
healthy. No disposable benchmark containers or volumes remained. The host-memory
sampler stopped. This report changes no collector code or performance thresholds.
