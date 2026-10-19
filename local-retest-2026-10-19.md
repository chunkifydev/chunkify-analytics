# Local benchmark after reducing background load

Measured on 2026-10-19, following the [checkpoint investigation](checkpointing.md).
The user authorized a local comparison with the development containers stopped.
The [raw reports and host samples](local-retest-2026-10-19.json) preserve both
completed probes and the setup/restoration details. All 222,378 acknowledged
events pass verification after restart.

## Setup

The 14 running Chunkify development containers were recorded and stopped without
removing containers or volumes. Stopping them alone did not settle host paging:
the next 30 seconds recorded about 249 MiB of swap-ins and 295 MiB of swap-outs.
Docker Desktop was then restarted with no containers running. After its startup
paging subsided, a 30-second interval had about 67 MiB of swap-ins and no
swap-outs or new compression.

The tests reuse `chunkify-analytics-collector:checkpoint-profile` from October 17.
Collector source hashes and the benchmark runner hash match that previous build's
measurement. No collector or benchmark implementation changes were made for this
comparison. The assistant did not close other applications.

Each run uses a fresh process and empty disposable database, two CPUs and 2 GiB
of container RAM, 180 seconds of native traffic, and 256 generator requests in
flight. Every request contains one event with captured browser context. IP
collection, dashboard reads, retention scans and profiling remain enabled.
Checkpoint threshold is 16 MiB and the batch collection window is 10 ms. All
existing disk, memory, event-count and latency guards remain in place.

## Results

| Offered requests/s | Environment | Delivered requests/s | HTTP 503 | Generator omissions | Successful p95, ms | Maximum commit, ms | Probe thresholds |
| ---: | --- | ---: | ---: | ---: | ---: | ---: | --- |
| 500 | October 17, development stack running | 486.40 | 9 | 2,417 | 118.22 | 1,592.11 | Failed |
| 500 | October 19, stack stopped and Docker restarted | 499.69 | 0 | 28 | 30.69 | 544.70 | Passed |
| 750 | October 17, development stack running | 696.90 | 55 | 9,421 | 157.24 | 1,490.58 | Failed |
| 750 | October 19, stack stopped and Docker restarted | 735.30 | 0 | 2,594 | 60.91 | 758.08 | Failed |

At 500/s, the collector acknowledges all 89,972 submitted requests. The generator
omits 28 of 90,000 scheduled requests, about 0.03%, because its concurrency bound
is reached. Dashboard p95 ranges from 47.27 to 57.83 ms with no failures. Every
acknowledged event, its browser/IP context, pagination, summaries and per-video
totals pass verification after graceful restart.

The host memory sampler covers setup, timed traffic and restart verification.
Across its 195.083-second interval for the 500/s case, it records 23.05 MiB of
swap-ins and no swap-outs. The collector peaks at 560.27 MiB of cgroup memory,
including filesystem cache, and averages 0.35 CPU cores. The generator peaks at
755.75 MiB RSS.

At 750/s, all 132,406 submitted requests succeed and survive verification after
restart. The generator omits 2,594 of 135,000 scheduled requests, or 1.92%.
Of these, 2,556 hit its 256-request concurrency bound and 38 miss the scheduling
deadline. This exceeds the 1% omission allowance; all other probe thresholds pass.
Successful request p99 is 532.38 ms. Dashboard p95 ranges from 79.71 to 94.99 ms
with no query failures, versus 744.80 to 815.03 ms in the earlier 750/s run.

The 750/s host sampler records 24.58 MiB of swap-ins and no swap-outs over
210.094 seconds including setup and verification. Collector cgroup memory peaks
at 866.63 MiB and mean CPU is 0.41 cores. Generator RSS peaks at 966.67 MiB,
below its 1 GiB guard. Neither probe triggers a safety stop.

## Interpretation

The 500/s result supports host contention as a contributor to the earlier
failures. The experiment combines stopping the development stack and restarting
Docker, so it does not isolate the effect of either action. The two measurements
also occurred on different days. The earlier 500/s baseline uses the prior
`:batched-queue` image; the checkpoint report describes the configuration and
profiling additions in the image used here. The write-batching algorithm and
defaults are unchanged.

Passing means meeting the existing delivery and latency thresholds, which permit
up to 1% generator omissions and 1% HTTP failures. It does not mean every
scheduled request was delivered. A three-minute run against a small database
does not establish sustained production capacity or behavior with 30 days of data.
In the 500/s run, the longest commit is still about half a second, so database stalls
remain measurable even with much less host paging.

At 750/s, commits still pause for up to 758 ms. A 256-request generator can fill
its in-flight allowance during such pauses, and the measured omissions are
mostly concurrency-limited. This identifies the next comparison: evaluate
commit/checkpoint pauses and bounded buffering together while retaining the
delivery and latency requirements. Increasing the queue alone would not prove
that the underlying pauses were fixed.

The 500/s probe now passes under the quieter local conditions. The 750/s probe
still fails its delivery threshold. Neither result certifies a production
capacity limit, and no hardware or performance defaults have changed.

## Restoration

All 13 development Compose services were restarted after the runs. Postgres and
LocalStack report healthy. The additional ephemeral LocalStack Lambda runtime
from the initial inventory was no longer present at restoration; it was not
recreated manually. No development volumes were removed. The benchmark uses
separate disposable volumes and verifies their cleanup.
