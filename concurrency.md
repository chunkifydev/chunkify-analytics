# Higher generator concurrency

Measured on 2026-10-19 after the [verification ledger probes](verification-ledger.md).
The [raw report](concurrency-2026-10-19.json) preserves the completed run, host
memory samples and development-service restoration.

Follow-up: the [comparison after the call](postcall.md) repeats this workload
with lower host memory activity. It records two full-delivery passes at
1,000 requests/s with the 1,024-request cap. The original failed result below
remains part of the measurement history.

## Change and setup

The load CLI now permits up to 1,024 requests in flight. Its default remains 128.
The collector code, defaults and all memory, disk and event-count guards remain
unchanged. CLI checks accept 1,024 and reject zero, 1,025 and fractional values.
Standards and spec reviews of this change found no issues.

This probe offers 1,000 requests/s for 180 seconds, with one native event per
request, browser context, IP collection, dashboard reads, retention and profiling.
It uses an empty disposable database and the same collector image as the earlier
512-concurrency probe. The container has two CPUs and 2 GiB of RAM. The server
queue allows 1,024 requests with a 16 MiB prepared-write budget. Batch settings
remain 100 events, 1 MiB and 10 ms, with a 16 MiB checkpoint threshold.

The 13 development services were stopped before the run. Docker Desktop was not
restarted. After the run, the user reported a simultaneous video call. The host
measurements show much greater memory compression than in the baseline. The
comparison therefore cannot isolate the effect of the concurrency change.

## Results

Both runs schedule 180,000 requests. Delivered rates include the final drain.

| Measurement | Earlier concurrency 512 | Concurrency 1,024 |
| --- | ---: | ---: |
| Submitted and successful requests | 178,614 | 178,886 |
| Delivered requests/s | 991.76 | 992.78 |
| Omitted at concurrency cap | 1,386 | 534 |
| Omitted after scheduling deadline | 0 | 580 |
| Total omissions | 1,386 | 1,114 |
| Ingest p95, ms | 106.36 | 319.75 |
| Highest dashboard p95, ms | 143.76 | 459.59 |
| Longest commit, ms | 731.09 | 1,091.80 |
| Peak generator RSS, MiB | 751.27 | 759.55 |
| Peak container memory, MiB | 951.30 | 1,000.10 |
| Original probe thresholds | Passed | Failed |
| Full-delivery thresholds | Failed | Failed |

The new run fails the 250 ms ingestion p95 requirement and omits 0.619% of planned
requests. It reaches the new 1,024-request concurrency cap. Every submitted
request succeeds, with no HTTP failures or dashboard errors. All 178,886
acknowledged events pass the payload, context, pagination and summary checks
after graceful restart. No safety guard fires.

Host samples cover 210.11 seconds including setup and verification. They record
14.98 MiB of swap-ins, no swap-outs and 41.41 GiB of page compression. The earlier
1,000/s run recorded 3.27 GiB of compression over a similar interval. These are
cumulative host-wide counters, not the amount of RAM occupied. They do not tell
us which application caused the activity.

## Conclusion and next step

Allowing more requests in flight did not produce a passing full-delivery run.
The result confirms that submitted events were stored correctly, but does not
establish a supported rate of 1,000 requests/s. The simultaneous call and changed
host activity prevent attributing the latency increase to the concurrency change
alone.

The planned repeat was deferred when the user reported the call. Repeat the
512-versus-1,024 comparison after the call, with the same workload and resource
limits and similar host conditions. Keep the latency and full-delivery checks.
Use those results before choosing further collector changes or changing defaults.

All 13 development services were restored. Postgres and LocalStack report
healthy. Disposable benchmark containers and volumes were removed.
