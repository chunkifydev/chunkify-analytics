# Effect of concurrent reads on ingestion

Measured on 2026-10-19, following the [higher-rate probes](throughput.md).
The [raw reports](read-impact-2026-10-19.json) preserve both conditions and their
host-memory samples.

## Question

Does removing concurrent dashboard reads resolve the ingestion latency failure
with an existing dataset? The mixed workload remains the acceptance test.
Ingestion-only traffic is a diagnostic comparison.

## Method

Both conditions use the same immutable collector image, Rust source and benchmark
code. Each starts with a fresh disposable database and preloads 50,000 events
through OTLP. The timed workload schedules 90,000 enriched native events, one per
request, at 3,000 requests/s for 30 seconds. Fresh identities are generated for
each run; the workload generation rules are the same.

The mixed run comes first. Its monitor requests `/v1/summary`, `/v1/videos` and
`/v1/events` together, samples resources, then waits one second before repeating.
The second run disables only these concurrent reads. Both retain scheduled
retention, resource sampling and full verification after restart.

The collector container has two CPUs and 2 GiB RAM. Both runs use a 4,096-request
queue, a 16 MiB prepared-write budget, a 64 MiB checkpoint threshold, a maximum
batch of 100 events or 1 MiB, and a 10 ms collection delay. The ingestion rate limit
is explicitly 10,000 requests/s. Generator concurrency is 4,096. Deployment
defaults remain unchanged.

The 1 GiB generator-memory guard and all other memory, disk, event-count and
latency gates remain unchanged. Thirty seconds reduces the planned dataset to
140,000 events including preload, compared with the earlier 40-second probe that
hit the generator-memory guard.

The 13 development services are stopped for the comparison. Docker is not
restarted for this pair. Tests run on the shared M2 Mac through Docker Desktop.

## Results

| Condition | Successful ingestion / scheduled | Concurrent read requests | Ingest p95, ms | Ingest p99, ms | Longest commit, ms | Full pass |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Mixed | 90,000 / 90,000 | 72 | 689.86 | 1,049.54 | 1,230.80 | No |
| Ingestion only | 90,000 / 90,000 | 0 | 457.30 | 790.67 | 917.44 | No |

Both runs complete with no generator omissions, HTTP failures, safety stops or
unacknowledged persisted events. Both fail the 250 ms ingestion p95 target. The
mixed run also fails the 500 ms read p95 target, at 570.43 ms for summary,
553.82 ms for videos and 680.65 ms for events. There are 24 requests to each read
endpoint, or one read per 1,250 ingestion requests across the traffic window.

The measured ingestion p95 is 232.56 ms lower with reads disabled, a 33.7% reduction
in this pair. This is an observed difference, not a repeatable performance claim.
The ingestion-only run still has long commits and queued work. Its maximum write
queue wait is 921.14 ms. Removing reads does not resolve the latency failure.

Read execution totals 2,800.03 ms in the mixed run, comprising 1,954.57 ms for event
listing, 406.40 ms for summaries and 439.06 ms for video statistics. This is worker
execution time, not CPU time. Its sum is about 9.3% of the 30-second offered-traffic
window. Read queue waits sum to 7,220.65 ms across concurrent requests; that sum is
not an additional 7.22 seconds of worker execution.

Aggregate commit time, including preload, is 8.75 seconds with reads and
8.84 seconds without reads. These totals do not isolate checkpoint work or timed
traffic. They show why the p95 difference should not be described as all commit
work disappearing when reads are removed.

| Condition | Peak generator RSS, MiB | Peak collector container memory, MiB | Mean collector CPU cores | Maximum requests in flight |
| --- | ---: | ---: | ---: | ---: |
| Mixed | 756.59 | 1,244.09 | 0.86 | 3,780 |
| Ingestion only | 893.09 | 1,022.92 | 0.65 | 2,826 |

The shorter traffic window completes within the existing memory guards. This makes
it usable for the next diagnostic; it does not resolve the generator's memory cost
for larger or longer tests.

Host sampling records no swap-outs. Across setup, preload, traffic and verification,
swap-ins are 0.047 MiB for the mixed run and 0.469 MiB for ingestion only. Cumulative
page compression is 9,469.23 and 8,185.83 MiB respectively. These host-wide counters
measure pages processed, not memory occupied, and cannot attribute activity to a
particular application. The shared host remains a limitation for causal comparisons.

## Decision

Prioritize the slow commit path before treating separate read workers as the
solution to the populated-database failure. The ingestion-only result demonstrates
that a latency problem remains when concurrent analytics reads are absent.
Separate read execution is still a candidate for reducing query-related waiting,
but its benefit must be measured after implementation.

The next diagnostic should record when slow commits occur alongside queue growth
and checkpoint activity, then distinguish checkpoint work from storage waits.
Keep the mixed workload as the acceptance test and preserve durable responses,
resource limits and post-restart verification. This comparison changes no Rust
code, database schema, worker scheduling or deployment defaults.

## Interpretation limits

The query toggle changes more than SQL execution time. It also removes the reads'
batch-flush barriers and the generator's query-response work. The monitoring loop
waits for reads before sampling resources, so its sampling cadence also changes
slightly. This comparison measures the combined effect of enabling those reads.

The shutdown database profile includes preload, startup and timed traffic. Its
aggregate commit totals cannot be attributed exclusively to the 30-second traffic
window. HTTP latency measurements cover timed requests. No claim about individual
checkpoint durations can be made from whole-commit timings.

One run per condition on a shared host cannot establish the exact causal effect
or sustained production capacity. In particular, a better ingestion-only result
would not itself validate a future implementation with separate read workers.

## Verification and cleanup

Each run verifies all 140,000 distinct acknowledged events after graceful restart,
including its 50,000-event preload. Across the pair, 280,000 events pass complete
payload, browser/IP and OTLP-context checks, pagination and global/per-video
summary checks. Neither run reports persisted events without acknowledgements.
These two runs do not repeat the separate forced-crash checks from the earlier
[throughput report](throughput.md).

All 13 development services were restored; Postgres and LocalStack report healthy.
Docker lists no remaining disposable benchmark containers or volumes.

## Review

### Standards

No findings. The report separates observed measurements from causal conclusions
and explains the limits of toggling concurrent reads.

### Spec

No findings. Counts, timings, resource measurements and verification totals match
the raw reports. Both latency failures remain explicit, and mixed traffic remains
the acceptance workload.

Review totals are zero Standards findings and zero Spec findings.
