# Collector capacity measurement

Measured on 16 October 2026. These are short runs against fresh databases, not a
production capacity guarantee. The [raw JSON report](capacity-2026-10-16.json)
records the image identity, environment, counts, and latency distributions.

## Setup

- Apple M2 Mac with eight logical CPUs. Node.js 24.18.0 generated the traffic.
- Docker Engine 27.5.1 in Docker Desktop, Linux ARM64. The Docker VM had eight
  CPUs and approximately 7.65 GiB RAM available.
- Each collector container had a two-CPU quota, 2 GiB RAM, and no additional swap.
  The database used a local Docker named volume. Other development containers
  were stopped during measurement.
- Release build with Rust 1.99.0 and the committed Cargo.lock. The collector's
  Rust source was at `a4dedd6`; the Docker packaging is introduced by this change.
  Cargo reported 13 minutes 22 seconds for the first release compilation.
- Six cases, each with a fresh database and 30 seconds of ingestion. Native HTTP
  JSON and OTLP/HTTP JSON each sent one event per request through the real SDK
  adapter. No gzip, batching, TLS, reverse proxy, or artificial network delay.
- Real SDK start/destroy attempts produced start, state-change, and final events
  across ten videos and up to 1,000 viewer IDs. No media played, so playing-time
  totals were zero. These were synthetic viewers and videos.
- At most eight client requests were in flight. Paced cases shared a target
  schedule; unpaced cases sent the next event after the previous response. A
  summary query ran approximately once per second during ingestion.
- The ingestion rate limit was raised to 10,000 requests/s for this measurement.
  Normal installations still default to 100 requests/s.

## Results

Each accepted request contained one new event. Request latency includes adapter
serialization, HTTP delivery through Docker's loopback port, and durable commit.
All cases had zero failed ingestion requests and zero failed summary queries.

| Format | Target events/s | Accepted events | Achieved events/s | Request p95 | Summary p95 | Sampled peak RSS | Data after checkpoint |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Native JSON | 50 | 1,500 | 50.02 | 10.49 ms | 17.44 ms | 52.39 MiB | 1.52 MiB |
| Native JSON | 100 | 3,000 | 100.02 | 11.75 ms | 12.36 ms | 56.55 MiB | 2.52 MiB |
| Native JSON | Unpaced | 16,758 | 558.04 | 21.56 ms | 54.78 ms | 101.41 MiB | 15.27 MiB |
| OTLP JSON | 50 | 1,500 | 50.02 | 13.10 ms | 24.72 ms | 55.33 MiB | 2.52 MiB |
| OTLP JSON | 100 | 3,000 | 100.02 | 10.94 ms | 20.19 ms | 61.95 MiB | 4.02 MiB |
| OTLP JSON | Unpaced | 14,607 | 486.62 | 23.59 ms | 50.07 ms | 131.59 MiB | 26.52 MiB |

After every case, the benchmark stopped the container, created a replacement with
the same volume, and read all event pages. All 40,365 acknowledged events matched
their original SDK payloads. View and unique-viewer totals also matched the
retained starts. Temporary containers and volumes were removed afterward.

RSS is sampled process memory, not the complete container memory footprint or an
exact peak. Data size is the logical size of the data directory after checkpoint,
including keys and database overhead. OTLP also stores the original log/context,
so it uses more space for the same canonical events.

## How to use these numbers

The default 100-request/s limit was below the measured throughput of this setup
for single-event requests. Keep it as an initial setting until a representative
deployment measurement supports changing it. The unpaced results describe this
workload and machine; they are not a sustained operating limit.

The largest database here contained 16,758 events. At a continuous 100 events/s,
30 days would contain 259.2 million events. This run does not establish storage,
memory, query, retention-sweep, or shutdown capacity for that volume. Measure the
intended retained dataset and dashboard query frequency on the target disk before
sizing a deployment. Large payloads and OTLP batches also change the workload;
the rate limit counts requests, not records within a batch.

The test used loopback HTTP and normal local filesystem caching. Public TLS traffic,
remote or slower disks, longer runs, concurrent dashboards, and retention work
need their own measurements. No browser/device/IP enrichment was implemented in
this version, so that planned work is not included in these results.

## Reproduce

From the repository root with Node.js 24, dependencies installed, and Docker
running:

```sh
COLLECTOR_LOAD_REPORT=/tmp/collector-capacity.json npm run benchmark:collector
```

Set `COLLECTOR_LOAD_SECONDS` to an integer from 5 to 300 to change each case's
duration. The script builds the image first, then records results after each
completed case. See the [collector README](README.md#container-checks-and-capacity-measurements)
for the test design and container setup.
