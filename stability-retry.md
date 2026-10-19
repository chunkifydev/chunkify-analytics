# Stability retry with bounded generator memory

On 2026-10-19, we repeated the [five-minute attempt](stability.md) with the
[generator heap fix](generator-memory.md). **The retry did not pass:** traffic
stopped after 109.413 seconds when collector container memory crossed the
1,800 MiB safety guard. The generator stayed below its own guard through traffic
and restart verification. All 377,112 acknowledged events survived restart.

The [raw report](stability-retry-2026-10-19.json) contains source hashes, resource
samples, profiles, request windows, host-memory samples and restoration evidence.

## Matched configuration

The collector image, source hashes, workload plan, thresholds and safety limits
match the previous attempt. The collector still had 2 CPUs, 2 GiB RAM, no swap,
the durable queue and separate reader enabled, and a 64 MiB checkpoint trigger.
The database reported two execution threads and a 1.5 GiB memory limit.

After preloading 50,000 OTLP events, the generator offered enriched native events
at 3,000 requests/s, one event per request. Summary, video-list and event-list
queries ran concurrently. The planned traffic was 900,000 requests over five
minutes; the dataset ceiling was one million events. The new launcher imposed a
704 MiB V8 heap limit. No explicit garbage collection ran during traffic.

The wrapper stopped the same 13 development containers before testing and
restored them afterward. The generator and Docker still shared the Mac with
other applications. This is a local comparison, not a dedicated server result.

## Results

| Metric | Observed result |
| --- | ---: |
| Planned / elapsed traffic | 300 s / 109.413 s |
| Successful ingestion requests | 327,112 |
| Successful requests/s, including HTTP drain | 2,989.71 |
| HTTP ingestion failures / rejected events | 0 / 0 |
| Requests omitted for lateness / concurrency | 702 / 0 |
| Planned requests not reached before stopping | 572,186 |
| Ingestion p95 / p99 | 38.96 ms / 189.16 ms |
| Summary p95 / p99 | 349.03 ms / 595.29 ms |
| Video-list p95 / p99 | 391.17 ms / 978.27 ms |
| Event-list p95 / p99 | 420.32 ms / 715.13 ms |
| Reads per dashboard endpoint / failures | 84 / 0 |
| Scheduling-lag p95 among sent requests | 2.87 ms |
| Mean collector CPU consumption | 1.29 cores |
| Peak sampled container memory | 1,809.34 MiB |
| Peak sampled collector process RSS | 1,286.55 MiB |
| Peak sampled generator RSS during traffic and drain | 463.47 MiB |
| Peak sampled generator RSS during verification | 584.81 MiB |
| Largest sampled pending event count | 4,850 |
| Longest profiled DuckDB commit | 2,009.91 ms |
| Checkpointed data-directory size | 444.20 MiB |
| Events verified after restart | 377,112 |

Aggregate ingestion p95 was below the 250 ms target. All three aggregate query
p95 values were below 500 ms. The final partial minute's video-list p95 was
535.34 ms, so the aggregate result does not mean every interval met the target.
Both overall benchmark pass flags remain false because the run stopped early.
The achieved rate describes the shortened run only.

## What caused the stop

The recorded stop reason is `container memory threshold`. At the stopping sample,
the cgroup accounted for 1,809.34 MiB while collector process RSS was
1,286.55 MiB. Cgroup memory includes filesystem cache and other charged memory.
These are different measurements: their difference cannot be assigned entirely
to reclaimable cache without `memory.stat` data, which this run did not record.

The test's 1,800 MiB guard stopped traffic before the container's 2 GiB hard
limit. This does not establish an OOM kill, a memory leak, or that Linux could
not reclaim enough memory to continue. It also does not justify removing the
guard. The generator memory failure from the previous attempt did not recur.

The collector still had 4,850 pending events at the first check after HTTP
completion. The queue wait returned early when the memory guard fired, without
recording an empty queue. The reported 70.48 ms is therefore an aborted wait, not
a completed drain interval or per-event visibility latency. Graceful shutdown
subsequently completed persistence; its 2,079.25 ms includes other shutdown work.
After restart, the verifier checked every acknowledged payload, browser/IP
context, original OTLP envelope, pagination, global summary and per-video summary.
No persisted event lacked an acknowledgement.

## Next step

Make the benchmark's CPU/RAM budget configurable and record the container's
anonymous memory, file cache and memory-pressure counters. Then test a larger
RAM budget with CPU unchanged, rather than repeat this same 2 GiB configuration.
A 2 CPU / 4 GiB comparison is a practical next diagnostic on the current Docker
VM. Record DuckDB's effective settings too: its automatic memory budget may
change with the container limit.

Keep the proposed 4 vCPU / 8 GiB deployment baseline as a separate validation
configuration once the host and Docker VM have enough headroom. Neither resource
configuration has a capacity guarantee from this test. The existing short
3,000-request/s pass remains the strongest complete-delivery result.

## Cleanup

All 13 development containers were restored, with Postgres and LocalStack healthy.
No disposable benchmark containers or volumes remained. The host-memory sampler
stopped. No collector code or test thresholds changed for this retry.

## Evidence review

The Standards review found no issues. The Spec review identified the aborted
queue-wait interpretation above; the narrative and raw-report notes were
corrected and rechecked. Neither review has remaining findings.
