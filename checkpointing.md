# Checkpoint and sustained-load investigation

Measured on 2026-10-17, following [write batching](batching.md). These runs use
the shared Apple M2 development machine and disposable Docker volumes. The
collector has two CPUs and 2 GiB RAM. Fourteen unrelated containers remain running.
The [raw reports](checkpointing-2026-10-17.json) retain all five completed probes
and the later host-memory diagnostic. Across these probes, all 509,259 acknowledged
events survive restart and pass payload, context and summary verification.

The [October 19 local retest](local-retest-2026-10-19.md) repeats the default
configuration after stopping the development stack and restarting Docker. It
records much less paging and a passing 500/s probe; 750/s still misses the
delivery threshold. The measurements below retain the original environment.

## Question

The earlier 60-second tests missed scheduled traffic while database commits
occasionally stalled. Does changing DuckDB's automatic checkpoint threshold
reduce those pauses enough to sustain native ingestion with browser context?

The bundled DuckDB 1.5.6 source sets a 16 MiB default and performs automatic
checkpoint work within the commit path. The
[configuration documentation](https://duckdb.org/docs/stable/configuration/overview)
describes the threshold. Increasing it preserves durable commits but permits a
larger WAL between checkpoints. It can increase recovery work and checkpoint
duration. The threshold is a trigger, not a hard WAL-size limit.

## Method

Each probe starts with an empty database and runs for 180 seconds. Native requests
contain one event, about 1.63 KiB of JSON, with browser context captured through
the Chunkify adapter. IP collection, dashboard queries and five-second retention
scans remain enabled. The generator permits 256 in-flight requests. Builds and
integration tests finish before timed traffic starts.

The configured admission rate is 10,000 requests/s to avoid confusing the token
bucket with database capacity. Write batches retain the defaults of 100 records,
1 MiB and 10 ms, with 256 queued operations and a 16 MiB prepared-buffer budget.
Only the checkpoint threshold changes between the 500/s comparisons.

The first baseline uses the previously built `:batched-queue` image. Subsequent
runs use `:checkpoint-profile`, which adds checkpoint configuration and records
the effective database settings once at startup. Raw reports retain the image
identities and local source hashes. The instrumented image reports two DuckDB
threads and a 1.5 GiB memory limit, consistent with the container's allocation.

After each completed run, the collector restarts and the runner checks every
acknowledged payload, browser/IP context, event pagination, summary and per-video
totals. These load runs use graceful restart. A separate integration test checks
native and OTLP events after SIGKILL with the 64 MiB override.

## Initial comparison

| Offered requests/s | Checkpoint MiB | Successful requests/s | HTTP 503 | Omitted by generator | Successful p95, ms | Commit maximum, ms | Probe thresholds |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 500 | 16 | 486.40 | 9 | 2,417 | 118.22 | 1,592.11 | Failed |
| 500 | 64 | 476.01 | 365 | 3,922 | 114.90 | 2,110.51 | Failed |
| 500 | 4 | 468.30 | 9 | 5,396 | 520.84 | 1,732.28 | Failed |

The 64 MiB run has three observed WAL-size drops versus fourteen in the baseline.
This agrees with less frequent checkpointing, but delivery worsened and the
longest commit grew. Total commit time was 33.69 seconds versus 30.30 seconds.
Neither run establishes 500/s as an operating rate. The shared host and sampling
do not isolate checkpoint execution from storage waits and scheduling delays.

The 4 MiB run has 79 observed WAL-size drops. Commit time rises to 53.43 seconds,
and successful request p95 exceeds the 250 ms threshold. Neither increasing nor
decreasing the checkpoint threshold improved the 500/s result in these runs.
This does not rule out checkpointing as a contributor to the stalls. It rules
out these two threshold changes as a demonstrated fix on this host.

## 750 requests/s

With the default 16 MiB checkpoint threshold and 10 ms collection window, the
three-minute run acknowledges 125,524 of 135,000 scheduled requests. It delivers
696.90 requests/s, returns 55 HTTP 503s and omits 9,421 scheduled requests.
Successful p95 is 157.24 ms and p99 is 1,065.67 ms. Dashboard p95 ranges from
744.80 to 815.03 ms, above the 500 ms threshold. All acknowledged events survive
restart. The longest commit takes 1,490.58 ms.

Increasing only the batch collection window to 50 ms reduces the transaction
count from 9,725 to 2,725 and ingestion execution time from 68.01 to 41.93 seconds.
Total commit time falls from 36.32 to 22.89 seconds. This is evidence that larger
batches reduce repeated database work, subject to the shared-host limitation.

Delivery remains almost unchanged at 698.97 requests/s, with 65 HTTP 503s and
9,082 omitted requests. Successful median latency grows from 16.64 to 45.97 ms;
p95 grows from 157.24 to 178.15 ms. Dashboard p95 remains above the threshold,
between 694.93 and 764.13 ms. The longest commit still takes 1,212.40 ms. The
comparison fails the same delivery and query thresholds. It does not justify
changing the default 10 ms collection window.

## Host memory pressure

During the subsequent batching-window comparison, the 16 GiB Mac reports
24,991.75 MiB of swap in use. A 30-second `vm_stat` interval records 154.75 MiB
of swap-ins and 301.19 MiB of swap-outs. It also records about 16 GiB each of
memory compression and decompression. These are system-wide counters, not
collector or Docker-only measurements.

Across the full 90.027-second sample, swap-ins total 493.48 MiB and swap-outs total
790.19 MiB. Compression and decompression each process about 56 GiB of pages.
The diagnostic covers only part of the 50 ms run; it cannot quantify memory
pressure in the preceding comparisons.

The host is actively paging while both Docker and the generator run. Container
CPU and memory measurements alone do not capture that pressure. The observations
do not prove which individual commit stalled because of swapping, and they do
not excuse dropped requests. They prevent attributing this measured ceiling
solely to the collector or treating it as a production capacity limit. Further
capacity sweeps need an isolated host with sufficient memory and disk headroom.

## Configuration and verification

`COLLECTOR_CHECKPOINT_THRESHOLD_MIB` accepts integers from 1 through 1024 and
defaults to 16. Invalid values fail startup. Profiling reports the effective
threshold, database version, threads and memory limit without event data or keys.

All 36 native integration tests pass with the 64 MiB override, including migration,
failed writes, retention, queries and forced restart. Clippy with warnings denied
and Rust formatting pass. Three Docker tests pass on the final image, covering
non-root lifecycle, crash recovery and benchmark accounting. Separate standards
and spec reviews found no actionable issues in the code change. The checkpoint
default remains 16 MiB after these comparisons.

## Decision and next step

Keep the 16 MiB checkpoint threshold and 10 ms batch window. The new configurable
threshold and effective-settings profile make further comparisons reproducible.
No tested 180-second workload passes all delivery and latency thresholds, so
neither 500 nor 750 requests/s is established as a reliable operating rate here.

Repeat the 500/s and 750/s tests on an otherwise idle Linux host, with enough
memory for the generator and Docker outside the collector's two-CPU, 2 GiB limit.
Confirm that the host is not paging and keep the same payloads, query workload,
duration and resource guards. Compare the 10 ms and 50 ms batch windows again.
If commit pauses persist there, isolate checkpoint and storage synchronization
cost before changing the architecture or raising the collector's CPU/RAM
requirements. Testing a representative retained dataset remains necessary.

## Reproduce

Build the image before starting a measurement:

```sh
docker build -t chunkify-analytics-collector:checkpoint-profile services/analytics-collector
COLLECTOR_IMAGE=chunkify-analytics-collector:checkpoint-profile \
COLLECTOR_LOAD_PROFILE=1 \
COLLECTOR_LOAD_CASES=native-500 \
COLLECTOR_LOAD_SECONDS=180 \
COLLECTOR_LOAD_CONCURRENCY=256 \
COLLECTOR_LOAD_BROWSER_CONTEXT=1 \
COLLECTOR_LOAD_CHECKPOINT_THRESHOLD_MIB=64 \
COLLECTOR_LOAD_REPORT=/tmp/collector-checkpoint64.json \
node tests/collector/docker/load.mjs
```

Use the same command with `16` or `4` for the other thresholds. The runner retains
its resource guards, including at least 4 GiB free disk and at most 200,000 unique
events per case. No CPU, RAM, queue or durability limits are relaxed for these
comparisons.

For the isolated baseline, use threshold `16` and run `native-500` and `native-750`
in separate processes. Repeat `native-750` with `COLLECTOR_LOAD_BATCH_DELAY_MS=50`
for the batching comparison. Keep builds and other tests outside the timed runs.
