# Verification ledger memory and longer probes

Measured on 2026-10-19 after the [bounded buffering comparison](buffering.md).
The [raw reports](verification-ledger-2026-10-19.json) include three completed
load probes, an isolated ledger-memory comparison, host samples and restoration
details. All 497,482 accepted events across these probes pass full verification
after graceful restart, including the 50,000 preloaded events in the final case.

## Changes

The benchmark now retains a compact replay description for each submitted event:
the captured template selection, original dynamic fields and first submitted
format. It reconstructs the full canonical payload and original OTLP record when
checking the read API after restart. Browser/IP checks, duplicate detection,
pagination and summary comparisons remain in place. After checking a payload,
the verifier retains only the fields used to calculate summary expectations.

Two preloaded native cases allow the same traffic to run against an existing
database. A separate `meets_full_delivery_thresholds` result requires the existing
latency and query thresholds plus every scheduled request being submitted and
succeeding. The older `meets_probe_thresholds` result still permits up to 1%
omissions and 1% failed ingestion requests.

An isolated check retained 180,000 generated records before and after the change.
After forced garbage collection, the additional reachable heap fell from
312.40 MiB to 142.26 MiB, a 54.46% reduction. This measures the verification ledger
only; it excludes HTTP traffic and is not a throughput measurement. The raw
reports describe the method and preserve the before/after memory counters.

## Setup

All three load probes use the same collector image and unchanged collector source
hashes as the earlier buffering comparison. The benchmark source hash changes
and is recorded. Each probe uses a fresh load-generator process, a disposable
database, two container CPUs and 2 GiB of RAM, a 1,024-request server queue and
512 generator requests in flight. The prepared-write budget remains 16 MiB.
The 100-event / 1 MiB / 10 ms batch settings and 16 MiB checkpoint threshold remain
unchanged. Each timed request contains one native event with browser context.
IP collection, dashboard reads, retention scans and profiling remain enabled.

The 13 development services were stopped externally for the measurements.
Docker Desktop was not restarted again. The assistant did not close applications.
All existing disk, container-memory, generator-memory and event-count guards
remain intact. Collector defaults are unchanged.

Each traffic window lasts 180 seconds. The final case first inserts 50,000 events
through OTLP, taking another 135.01 seconds and producing a 192.81 MiB data
directory before timed traffic. Preload and new events share the 200,000-event
safety limit.

## Results

All submitted requests and all dashboard reads succeed in every case.
Delivered rates include the final drain of outstanding requests.

| Offered requests/s | Preloaded events | Delivered requests/s | Generator omissions | Ingest p95, ms | Highest dashboard p95, ms | Full-delivery thresholds |
| ---: | ---: | ---: | ---: | ---: | ---: | --- |
| 750 | 0 | 744.82 | 866 / 0.641% | 137.72 | 479.49 | Failed |
| 1,000 | 0 | 991.76 | 1,386 / 0.770% | 106.36 | 143.76 | Failed |
| 750 | 50,000 | 748.24 | 266 / 0.197% | 170.27 | 304.68 | Failed |

All three pass the older probe thresholds. None meets the stricter full-delivery
requirement because some scheduled requests were not submitted. At 750/s on an
empty database, 645 omissions hit the concurrency cap and 221 missed the
scheduling deadline. All omissions in the other two cases hit the concurrency
cap. These omissions are distinct from collector HTTP rejections or lost
acknowledged events; neither occurred in these runs.

| Probe | Peak generator RSS, MiB | Peak container memory, MiB | Mean collector CPU cores | Longest commit, ms |
| --- | ---: | ---: | ---: | ---: |
| 750/s, empty | 625.69 | 828.80 | 0.44 | 810.44 |
| 1,000/s, empty | 751.27 | 951.30 | 0.46 | 731.09 |
| 750/s, preloaded | 712.19 | 1,201.63 | 0.49 | 805.38 |

Generator peaks are sampled during the load measurement; container memory also
includes filesystem cache. No safety guard fires. The runner previously exceeded
its 1 GiB generator guard with 135,000 submissions. The revised runner completes
178,614 submissions at 1,000/s with a sampled generator peak of 751.27 MiB.
The isolated ledger measurement supports the memory improvement without relying
on identical host conditions across the load probes.

## Interpretation and next step

The memory change removes the earlier obstacle to these three-minute runs while
preserving the stored-data checks. The collector accepts every request actually
submitted, but the offered load still exceeds the generator's 512-request
in-flight allowance during some response stalls. Commit pauses remain around
0.7 to 0.8 seconds.

The next comparison should use a higher, explicitly bounded generator
concurrency to exercise the 1,024-request server queue, while retaining the
existing memory and disk guards. At 1,000/s, a 731 ms pause alone can accumulate
about 731 requests. A 512-request generator cap cannot submit all of that traffic.
Increasing concurrency would expose more of the collector's queueing and latency
behavior; it does not guarantee a passing full-delivery result.

Do not adopt the larger queue as the default or claim a supported 1,000 requests/s
rate from these probes. Full delivery, latency under the additional queued work,
repeatability and longer retention workloads remain unverified.

These remain shared-Mac, Docker Desktop measurements with loopback HTTP and no
TLS. Host samples cover setup, traffic and verification. They record no swap-outs,
but approximately 97, 43 and 3 MiB of swap-ins and 10.0, 3.3 and 6.3 GiB of
compression across the three cases. Host activity differs between runs, so the
preloaded case's lower omission count does not show that a larger database is
faster. The preload adds data volume, but does not simulate weeks of expired-data
deletions or production traffic patterns.

## Verification and review

Both updated Docker benchmark tests pass. They cover large enriched payloads,
mixed native/OTLP traffic, gzip, duplicate accounting, forced-crash recovery,
rate-limit rejections and profiling counts. The regression assertion for the
full-delivery verdict failed before its implementation and passed afterward.
The new preloaded case was also checked through the CLI plan output. No
machine-specific throughput assertion was added to CI.

### Standards review

One finding identified a missing Chromium installation in the container CI job
after browser-context capture was enabled in the accounting test. The job now
installs Chromium and its dependencies before the Docker tests. Review confirmed
the fix. No remaining findings.

### Spec review

The review identified the same CI dependency and confirmed its resolution.
Replay preserves the original dynamic fields and submitted format, the summary
projection retains its calculation inputs, and the full-delivery verdict rejects
omitted and failed requests. No remaining findings.

Review summary: zero remaining Standards findings and zero remaining Spec findings.
The GitHub workflow itself was not run remotely during this work.

All 13 development services were restored. Postgres and LocalStack report
healthy. Disposable benchmark containers and volumes were removed.
