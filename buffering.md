# Bounded request buffering

Measured on 2026-10-19, following the [local retest](local-retest-2026-10-19.md).
The [raw reports](buffering-2026-10-19.json) include all four probes, host memory
samples, source hashes and service restoration. All 431,539 acknowledged events
pass payload, context, pagination, summary and per-video verification after
graceful restart, including those accepted during the safety-stopped probe.

## Question and controls

The previous 750 requests/s run omitted 1.92% of scheduled requests, mostly
because its load generator reached 256 concurrent requests during commit pauses.
This comparison first raises generator concurrency to 512, then changes the
collector request queue from 256 to 1,024. The prepared-data byte budget remains
16 MiB. This budget bounds prepared write buffers, not total process memory.

All runs use the same `:checkpoint-profile` image as the local retest, with
matching collector and benchmark source hashes. Each uses a fresh process and
disposable database, two CPUs and 2 GiB of container memory, one native event per
request, browser/IP context, dashboard reads, retention scans and profiling.
Batches remain limited to 100 events, 1 MiB and a 10 ms collection window; the
checkpoint threshold remains 16 MiB. The ingestion rate cap is 10,000 requests/s
for these tests; the normal application default is 100.

The 13 development containers were recorded and stopped before testing. Docker
Desktop had been restarted during the earlier local retest and was not restarted
again. No other containers were running during these probes. The assistant did
not close applications. Existing disk, memory and latency guards remain intact.
No collector code, benchmark implementation or application defaults changed.

## Results

All rows below use 512 generator requests in flight. Latency is for successful
ingestion requests. Delivered rates include the time needed to drain outstanding
requests at the end of traffic.

| Offered requests/s | Duration, s | Server queue | Delivered requests/s | Ingest HTTP 503 | Generator omissions | Ingest p95, ms | Dashboard failures | Verdict |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 750 | 180 planned | 256 | 738.45 | 2,019 | 0 | 57.54 | 9 | Memory guard triggered |
| 750 | 120 | 256 | 745.51 | 500 | 0 | 38.30 | 3 | Failed |
| 750 | 120 | 1,024 | 749.47 | 0 | 0 | 43.04 | 0 | Passed |
| 1,000 | 120 | 1,024 | 988.43 | 0 | 942 | 112.00 | 0 | Passed |

The first run submitted all 135,000 scheduled requests, but the load generator
peaked at 1,032.59 MiB RSS and triggered its 1 GiB guard near the end. It is
recorded as `safety_stopped` and does not establish a valid three-minute capacity
result. Its 2,019 ingestion rejections and nine failed dashboard reads show
admission pressure once the generator can submit beyond the collector queue's
capacity. The next two runs use equal two-minute durations to stay within the
same memory guard.

At 750/s with queue 256, all 90,000 scheduled requests are sent; 89,500 succeed.
Each dashboard endpoint also returns one HTTP 503. The ingestion error fraction
alone is within the probe's 1% allowance, but dashboard failures make the run fail.

At 750/s with queue 1,024, all 90,000 requests succeed, all dashboard reads succeed,
and all probe thresholds pass. Dashboard p95 ranges from 63.34 to 68.69 ms.
Ingestion p99 is 325.85 ms. The longest commit remains 537.24 ms, compared with
526.74 ms for queue 256. Buffering absorbs the stalls without removing them.

At 1,000/s with queue 1,024, all 119,058 submitted requests succeed. The generator
omits 942 of 120,000 scheduled requests, or 0.785%; 898 hit its concurrency bound
and 44 miss the scheduling deadline. Ingestion p99 is 549.80 ms. Dashboard p95
ranges from 49.47 to 50.80 ms with no failures. The longest commit is 773.43 ms.
The result passes the current thresholds, which allow up to 1% generator
omissions and 1% HTTP failures. It is not full delivery of the offered load.

## Resources and limitations

| Two-minute probe | Mean CPU cores | Peak container memory, MiB | Peak generator RSS, MiB | Host swap-ins, MiB | Host swap-outs, MiB |
| --- | ---: | ---: | ---: | ---: | ---: |
| 750/s, queue 256 | 0.38 | 542.58 | 777.27 | 0.78 | 0 |
| 750/s, queue 1,024 | 0.38 | 645.47 | 818.03 | 0.47 | 0 |
| 1,000/s, queue 1,024 | 0.43 | 706.49 | 917.36 | 1.20 | 0 |

Container memory includes filesystem cache. Host memory samples include setup,
traffic and verification. The two-minute runs still record roughly 1.1 to 2.2 GiB
of memory compression, despite minimal swapping. This is a shared Mac running
Docker Desktop, not a dedicated Linux capacity measurement.

These are single probes against small, initially empty databases with loopback
HTTP and no TLS. They do not establish sustained capacity, performance with weeks
of retained data, or a hardware sizing guarantee. The 512-request generator cap
also means this experiment does not exercise a fully occupied 1,024-request
server queue. Read latency and overload behavior at that occupancy remain open.

## Decision and next step

A 1,024-request queue with the existing 16 MiB byte budget is a useful candidate
for further testing. The equal-duration comparison removes the observed 750/s
rejections while keeping p95 latency within the current limits. Commit pauses
remain, and the 1,000/s probe still omits some offered traffic.

Keep this as a benchmark configuration until repeated and longer probes support
changing the default. First reduce the generator's retained verification data
without weakening full-payload and restart checks. That will allow further runs
within the existing 1 GiB generator and event-count guards. Then repeat the
candidate configuration and test a larger retained database. Average CPU usage
in these runs gives no evidence that adding CPUs alone resolves the remaining
commit stalls.

All 13 recorded development services were restored afterward. Postgres and
LocalStack report healthy. No disposable benchmark containers or volumes remain.
