# Expanded collector benchmarks

Measured on 16 October 2026. This report covers the collector before database
write batching. The immutable collector image is unchanged from the first
capacity measurement. That measured image predates browser/device/IP enrichment;
these results do not measure the later flat event-context storage.

## Setup and scope

- Apple M2, eight logical CPUs; Docker Desktop with eight CPUs and about 7.65 GiB
  available to its VM; Node.js 24.18.0 as the load generator.
- Each temporary collector had two CPUs, 2 GiB memory, no additional swap, and a
  local Docker named volume. Traffic used loopback HTTP without TLS.
- The host had 15 other running containers during the main matrix and 14 during
  the follow-ups. Their activity was not controlled. These are measurements on a
  shared development machine.
- All seven SDK event types were replayed from actual SDK/adapter output. Views
  included a snapshot and a nonzero final playing time. The distribution used ten
  video IDs and up to 1,000 viewer IDs; larger-payload cases added synthetic text.
- Up to 128 requests were in flight. The offered schedule was independent of
  response completion. Skipped work, generator lag, preparation time and CPU are
  included in the raw reports. Summary, per-video and paginated raw-event queries
  ran concurrently. Retention scanned every five seconds with a 30-day lifetime.
- The rate cap was raised to 10,000 requests/s except for the explicit rate-cap
  experiment. The body bound was 1 MiB and the OTLP record bound was 100.
- The main matrix used 30-second offered windows. Follow-ups used 60-second
  windows. Throughput includes any final request drain. These durations do not
  establish sustained deployment capacity or capacity for 30 days of data.

The [runner documentation](README.md#container-checks-and-capacity-measurements)
contains the commands, complete scenario definitions, safety stops, and thresholds.
Thresholds were selected before measurement: at least 99% of scheduled requests
sent, at least 99% HTTP success, successful-ingest p95 at most 250 ms, successful
query p95 at most 500 ms with no query failures, and generator-lag p95 at most 20 ms.
Passing allows a small ingest error rate and does not mean zero dropped events.

## Main matrix

| Case | Target requests/s | New events/s | Success p95, ms | HTTP/transport failures | Thresholds met |
| --- | ---: | ---: | ---: | --- | --- |
| `native-100` | 100 | 99.92 | 9.96 | None | Yes |
| `native-400` | 400 | 382.59 | 138.27 | 503: 307 | No |
| `native-800` | 800 | 460.67 | 243.46 | 503: 9,750 | No |
| `native-1600` | 1600 | 450.23 | 250.66 | 503: 34,243 | No |
| `otlp-100` | 100 | 99.55 | 91.42 | None | Yes |
| `otlp-400` | 400 | 384.19 | 202.45 | 503: 471 | No |
| `otlp-800` | 800 | 390.43 | 247.75 | 503: 11,443 | No |
| `otlp-batch10` | 100 | 646.67 | 2583.41 | 503: 966 | No |
| `otlp-batch25` | 100 | 335.48 | 8726.70 | 503: 2,040 | No |
| `otlp-batch25-gzip` | 100 | 387.31 | 8251.00 | 503: 2,178 | No |
| `mixed` | 100 | 423.54 | 737.81 | 503: 87 | No |
| `mixed-large` | 50 | 218.10 | 497.68 | None | No |
| `preloaded-mixed` | 100 | 357.63 | 1443.20 | 503: 538 | No |
| `burst-recovery` | 100 / 1600 / 100 | 246.41 | 228.94 | 503: 12,743 | No |
| `rate-cap` | 400 | 654.54 | 1371.05 | 429: 8,527, 503: 1,017 | No |

## Follow-up measurements

The follow-ups narrow the 100-to-400 requests/s interval and compare OTLP batch
sizes at the same offered 250 records/s. A temporary macOS idle-sleep assertion
kept the machine awake for this command.

| Case | Target requests/s | New events/s | Success p95, ms | HTTP/transport failures | Thresholds met |
| --- | ---: | ---: | ---: | --- | --- |
| `native-200` | 200 | 199.26 | 25.57 | 503: 4 | Yes |
| `otlp-200` | 200 | 196.45 | 157.61 | 503: 81 | No |
| `otlp-batch10-low` | 25 | 249.82 | 94.59 | None | Yes |
| `otlp-batch25-low` | 10 | 249.15 | 78.92 | None | Yes |

## What the measurements show

- The 100-request/s native and single-record OTLP probes passed the chosen
  thresholds with no ingest failures. Keep the deployment default at 100
  requests/s pending measurements on the intended server and retained dataset.
- Increasing offered traffic to 400 requests/s or more caused queue rejection
  responses. The roughly 450 native events/s observed during overload is not a
  recommended operating rate: it excluded many rejected requests.
- Large OTLP requests can exceed the writer's capacity even when requests/s
  looks modest. The 10-record, 100-request/s case offered 1,000 records/s; the
  25-record case offered 2,500. Latency and errors must accompany throughput.
- The rate-cap case returned both 429 and 503. A limit on requests/s does not bound
  events/s when a request contains multiple records.
- Preloading 50,000 events took 169.9 seconds through the current HTTP
  write path. In the subsequent mixed load, successful-request p95 was
  1443.20 ms, versus the fresh-database mixed case in the table.
  This is an observed difference on the shared host, not an isolated causal estimate.
- After the burst, the recovery phase sent 1,200 requests at 100 requests/s.
  All 1,200 succeeded; request p95 was 10.37 ms.
  Outstanding burst requests were drained before that phase, with drain time
  recorded separately.
- All 274,817 acknowledged unique events checked across these cases survived
  container replacement. Full payloads, original OTLP context, deduplication,
  view/unique-viewer counts and cumulative playing time matched the read API.
  This total includes preload events. 0 extra persisted events lacked an acknowledgement.
- The compressed 25-record case used SIGKILL before restart. Other cases used
  graceful termination. Peak sampled container memory across these runs was
  598.23 MiB, including filesystem cache. Resource sampling can miss peaks.

## Interrupted longer attempt

An earlier pair of intended 120-second cases encountered execution gaps of about
15 minutes. Actual elapsed times exceeded 945 seconds and the generator skipped
most scheduled traffic. Those throughput values are invalid as collector capacity
measurements and are excluded from the tables and conclusions above. Their raw
record has an explicit post-run assessment. The runner now stops if its generator
makes no progress for more than five seconds, instead of continuing the matrix.
The cause of those pauses was not established.

## Remaining measurement limits

The largest initial dataset here was 50,000 events. These runs do not test a
month of production data, large expired-data purges, remote/slow disks, public TLS,
or many simultaneous dashboard users. Retention scans ran, but no events were
old enough to expire. Internal database queue depth/wait, commit timing and write
batch sizes are unavailable without instrumentation and should be measured when
implementing the writer changes. The earlier six-case report used different
fixtures, scheduling and query traffic; its headline rates are not directly
comparable with this matrix.

Use the same runner, environment, dataset and workload when comparing future
collector-side batching. Repeat longer tests on the intended deployment host
before publishing a sustainable operating range.

## Raw reports

- [Main matrix](capacity-2026-10-16-expanded.json)
- [Follow-ups](capacity-2026-10-16-followup.json)
- [Interrupted attempt, invalid for capacity](capacity-2026-10-16-interrupted.json)
- [Original six-case baseline](capacity.md)

Each report records the image ID, local collector source revision, runner hash,
settings, counters, latency distributions and sampled resources. The follow-up
runner added cases, cancellation/pause handling and sample validation after the
main matrix. The collector executable stayed the same.
