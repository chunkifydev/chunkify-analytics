# Concurrency comparison after the video call

Measured on 2026-10-19 after the user reported that their video call had ended.
The [raw reports](postcall-2026-10-19.json) contain three completed probes, host
memory samples and service-restoration details. This follows the
[earlier concurrency probe](concurrency.md), which ran during the call.

## Setup

The 13 development services were stopped, and Docker Desktop was restarted once
before the comparison. A 30-second settling sample recorded no swapping or new
page compression. Docker was not restarted between probes. The order was
512 generator requests in flight, then 1,024, then a repeat at 1,024.

Every probe offers 1,000 native requests/s for 180 seconds, with one event per
request and an empty disposable database. Browser context, IP collection,
dashboard reads, retention scans and profiling remain enabled. Each probe starts
a new generator process. The same collector image and source hashes are used
throughout. The benchmark source, thresholds and safety guards also match the
earlier 1,024-concurrency probe. No application or benchmark code changed for
this comparison.

The collector container has two CPUs and 2 GiB of RAM. Its request queue allows
1,024 requests with a 16 MiB prepared-write budget. Batch settings remain
100 events, 1 MiB and 10 ms, with a 16 MiB checkpoint threshold. These are explicit
benchmark settings; the collector's default request queue remains 256.

## Results

| Generator concurrency | Successful / scheduled | Skipped | Ingest p95, ms | Highest dashboard p95, ms | Full-delivery thresholds |
| ---: | ---: | ---: | ---: | ---: | --- |
| 512 | 178,642 / 180,000 | 1,358 | 103.44 | 117.98 | Failed |
| 1,024 | 180,000 / 180,000 | 0 | 136.39 | 115.73 | Passed |
| 1,024, repeat | 180,000 / 180,000 | 0 | 120.19 | 118.93 | Passed |

All submitted requests succeed in all three runs. No HTTP failures, dashboard
errors, scheduling-deadline omissions or safety stops occur. Every omission in
the first run happens at the 512-request generator cap. The two passing runs
reach 846 and 818 requests in flight, respectively.

Both passing runs submit and complete all 180,000 requests scheduled at 1,000/s.
Their reported averages are 999.51 and 999.44 successful requests/s because the
elapsed-time denominator includes finishing outstanding requests after the
180-second traffic window. These averages do not indicate skipped requests.

| Generator concurrency | Ingest p99, ms | Longest commit, ms | Peak generator RSS, MiB | Peak container memory, MiB | Mean collector CPU cores |
| ---: | ---: | ---: | ---: | ---: | ---: |
| 512 | 502.70 | 739.06 | 755.11 | 976.51 | 0.45 |
| 1,024 | 502.71 | 779.55 | 764.53 | 1,047.73 | 0.46 |
| 1,024, repeat | 498.25 | 787.51 | 755.56 | 964.17 | 0.46 |

The pass conditions include ingestion p95 at most 250 ms, dashboard p95 at most
500 ms, scheduling lag p95 at most 20 ms, and every scheduled request submitted
and successful. They do not require every request to finish within 250 ms.
Long commit pauses and approximately 500 ms ingestion p99 remain observable.

The runner verifies all 538,642 acknowledged events after graceful restart,
including full payloads, browser/IP context, pagination and global/per-video
summaries. These three probes exercise native ingestion. Earlier integration
tests cover OTLP and forced-crash recovery; this comparison does not repeat
those scenarios.

## Host activity

Host sampling covers roughly 210 seconds per probe, including setup and
verification. No swap-outs occur. Swap-ins are 0, 0.125 and 0.031 MiB. Page
compression totals are 1.87, 1.25 and 1.05 GiB, compared with 41.41 GiB during the
earlier 1,024-concurrency run. These are cumulative host-wide counters and do not
identify the application responsible.

Ending the call and restarting Docker both changed host conditions. We cannot
assign the improvement to the call alone. The 512-concurrency omissions recur
with no swapping, which supports the generator-cap explanation independently
of the earlier call.

## Conclusion and next step

The collector passes this three-minute, 1,000 requests/s workload twice with the
tested queue and generator settings. Raising the generator cap allows the
scheduled traffic to be submitted through response stalls. It does not shorten
the underlying commit pauses.

This establishes a repeatable local result for this workload. It does not
establish multi-hour production capacity, capacity above 1,000 requests/s, or
behavior with a large retained database. The next capacity check should use an
existing database while preserving the resource limits, before adopting the
larger queue as a deployment default. Storage optimization remains deferred.

All 13 development services were restored. Postgres and LocalStack report
healthy. Disposable benchmark containers and volumes were removed.
