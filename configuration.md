# Recommended collector configuration

Decision recorded on 2026-10-21 from the local experiments and administrator
acceptance. This selects the first-release operating mode and its starting
settings. The collector is still a development version; sustained capacity and
final release checks remain open in [issue #381](https://github.com/chunkifydev/chunkify/issues/381).

## Start here

Use one collector process with a persistent local data directory, direct ingestion,
bulk writes and bounded batching. Keep the experimental durable queue, replay
pipeline and separate reader disabled. Successful native and OTLP responses then
follow the DuckDB commit. A subsequent fresh read can see that committed event;
an older pagination cursor retains its existing snapshot boundary.

Use [collector.env.example](collector.env.example) with Docker's `--env-file`
option as shown in the [Docker quickstart](README.md#run-with-docker). Change the
website origin, retention and IP policy to suit the installation. Configure proxy
trust only for the administrator's actual ingress. Credentials remain in the
persistent data directory, with read tokens kept on the dashboard backend.

Only the server administrator configures these settings. Environment changes take
effect when the process starts. Recreate a Docker container to apply changes to
its environment file, keeping the same data volume; `docker restart` alone keeps
the old environment. The executable does not automatically load an `.env` file.
For a standalone installation, export the same variables through the service
manager or shell and use the appropriate `COLLECTOR_DATA_DIR` and bind address.

## Recommended values and defaults

The example explicitly selects one DuckDB execution thread. The executable's
built-in default remains automatic when that variable is absent. Other values
below retain the existing defaults; no Rust defaults or API contract changed.

| Setting | Recommended starting value | Why / trade-off |
| --- | --- | --- |
| Ingestion mode | Direct | A successful response follows the analytics database commit. |
| `COLLECTOR_INSERT_MODE` | `bulk` | Batching and bulk insertion reduced repeated INSERT and commit work. |
| `COLLECTOR_DUCKDB_THREADS` | `1` | Start with bounded database parallelism on a small server. Our queued-load experiments favored one thread; direct-mode administrator checks also passed with one. This is not proof that one is optimal for every direct workload. |
| `COLLECTOR_BATCH_MAX_EVENTS` | `100` | Bound each transaction's record count; whole OTLP requests must fit. |
| `COLLECTOR_BATCH_MAX_BYTES` | `1048576` | Bound each transaction's prepared buffers to 1 MiB. Counts prepared buffers, not HTTP body bytes. |
| `COLLECTOR_BATCH_DELAY_MS` | `10` | Give concurrent requests a short window to share a commit. This excludes queue wait and database execution. |
| `COLLECTOR_QUEUE_MAX_BYTES` | `16777216` | Bound admitted prepared buffers, including the active batch, to 16 MiB. |
| `COLLECTOR_QUEUE_MAX_REQUESTS` | `256` | Bound waiting operations. Bigger queues can absorb bursts but increase waiting time. |
| `COLLECTOR_CHECKPOINT_THRESHOLD_MIB` | `16` | Keep the existing checkpoint trigger. Larger thresholds did not establish reliable high-rate capacity and can lengthen individual pauses. |
| `COLLECTOR_INGEST_PER_SECOND` | `100` initially | An adjustable admission policy, not the maximum processing capability or a certified operating rate. Raise it after testing the intended workload. |
| `COLLECTOR_MAX_BODY_BYTES` / `COLLECTOR_MAX_OTLP_RECORDS` | `65536` / `100` | Bound request size and OTLP records. A whole request must also fit the prepared-batch and project byte budgets. |
| `COLLECTOR_RETENTION_SECONDS` / `COLLECTOR_RETENTION_INTERVAL_SECONDS` | `2592000` / `3600` | Default to 30 days by receipt time and sweep hourly. Projects may override retention. Choose durations for the actual event volume and disk budget. |

The [configuration reference](README.md#configuration) documents accepted ranges.
Administrators may override supported tuning settings within those bounds.
The queue byte limit is not a process memory limit: DuckDB, parsed requests,
response buffers and the operating system consume additional memory. Set process
or container resource limits separately and monitor disk space and memory.
Retention also does not impose a hard database-size ceiling. Use
`project-retention <id> <seconds|inherit>` to change one project's duration live
on Linux/macOS. `0` disables that project's expiration, and `inherit` restores the
installation default. Changes affect existing data at the next sweep and persist
across restart. The command preserves the project's other settings. Full
[retention semantics](README.md#retention) include rollback/restore considerations.

A single request may contain many OTLP events, so requests/s and events/s differ.
Project rate and queue allowances inherit a share of the global limits, including
an otherwise unused `default` project. For example, three projects with a global
rate of 100 inherit a rate of 33 each unless overridden. Global limits still apply.
See [project limits](README.md#project-limits-and-origins).

## Options available to administrators

| Category | Options | First-release policy |
| --- | --- | --- |
| Installation and data policy | Data path, bind address, website origins, retention, IP collection and trusted proxies | Normal configuration for each installation. |
| Admission limits | Request body/OTLP bounds, global and project rates, queue limits | Supported controls with documented rejection behavior. |
| Performance tuning | Batch limits/delay, DuckDB thread count, checkpoint threshold | Optional overrides. Compare latency, errors and resource use before raising them. |
| Diagnostics | `COLLECTOR_PROFILE=true`, `COLLECTOR_INSERT_MODE=row` | Troubleshooting and controlled comparisons. The normal write mode remains `bulk`. |
| Experimental execution | `COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE`, `COLLECTOR_EXPERIMENTAL_REPLAY_PIPELINE`, `COLLECTOR_EXPERIMENTAL_READ_WORKER` | Opt-in experiments, excluded from the recommended v1 configuration. |

The experimental durable queue acknowledges receipt into SQLite before replay
into DuckDB, so query visibility can lag acknowledgements. The replay pipeline
requires that queue. A separate reader changes scheduling but still shares the
database's CPU, memory, disk and checkpoint work. These are behavior choices,
not interchangeable speed presets. See the [queue](README.md#experimental-durable-queue)
and [reader](README.md#experimental-separate-reader) documentation. Drain pending
queue entries before disabling queued mode; startup refuses to skip pending data.

## Evidence behind the choice

- The [bounded batching comparison](batching.md) favored bulk insertion over the
  per-row path. It supports retaining bulk batching; its old short-run rates do
  not certify the current version's capacity.
- The [administrator/Docker walkthrough](administrator-walkthrough.md) passed
  with direct ingestion and one DuckDB thread, including real browser playback,
  two projects, credential management, replacement, backup/restore and upgrade.
  It used tiny fixtures at two CPUs / 1 GiB and did not measure capacity.
- October 20 thread/replay comparisons at four CPUs / 4 GiB reached about
  9,793 and 9,873 accepted single-event requests/s in two 60-second queued runs
  without HTTP failures. Both omitted scheduled requests and exceeded the 500 ms
  dashboard p95 target. Larger admission queues did not eliminate omissions.
- The ten-minute queued run at six CPUs / 4 GiB accepted all 2,992,035 requests
  that reached it, about 4,987/s over the offered window. The generator omitted
  7,965 requests; dashboard p95 was 1.17-1.35 seconds. All 3,192,035 acknowledged
  events including preload survived graceful restart and API verification. This
  supports the implementation's integrity in that run, not a stable 5,000/s claim.
- Raising the checkpoint trigger from 64 to 256 MiB in a matched two-minute
  queued comparison increased the maximum observed writer commit from 1.79 to
  3.69 seconds and did not pass the capacity gates. Increasing CPU quota from
  four to six removed measured CPU throttling but also failed the gates.

The October 20 experiments used a shared M2 Mac, Docker Desktop, one project and
local HTTP. The local reports `thread-tuning-summary.md`,
`soak5000-ten-minute-summary.md`, `checkpoint-frequency-summary.md` and
`query-cpu-comparison.md` remain under
`/Users/ftatilon/Dev/benchmarks/chunkify-analytics/2026-10-20`; large raw evidence
stays outside the repository. The retained production image used by the soak and
administrator checks is `sha256:38dff245041d410fe55b0c04b4528b285b40674216b4f8223cdc4b63b62a953b`.
The high-rate results used substantially larger batch/queue budgets and enabled
experimental modes. They are not measurements of the recommended direct profile.

DuckDB also documents that excessive parallelism can slow workloads and recommends
checking memory per thread and storage speed. Our one-thread choice is a starting
recommendation informed by local measurements, not a universal DuckDB setting.
[Thread guidance](https://duckdb.org/docs/current/guides/performance/how_to_tune_workloads#too-many-threads),
[memory and storage guidance](https://duckdb.org/docs/current/guides/performance/my_workload_is_slow).

## Before raising limits

Measure the current direct profile on the intended machine with representative
payload sizes, OTLP batch sizes, project count, retained data and dashboard reads.
Include expiration/deletion work and a burst followed by recovery. Record sent,
omitted, accepted and rejected requests separately, response latency, resource
use and exact acknowledged-event integrity after restart.

HTTP 429 means a global or project rate policy rejected the request. HTTP 503 can
mean admission pressure or a worker failure; inspect health and logs. HTTP 413
means the whole request exceeds an applicable body, record or prepared-buffer
limit. Raising a limit cannot fix insufficient processing capacity. Acknowledged
events and rejected or unsent events must remain separate in any delivery claim.

No current result establishes a sustained rate for the recommended profile across
multiple projects and a growing retained dataset. That focused capacity check is
still required before publishing an operating-rate recommendation. The supported
mode and tuning policy are selected; the measured rate remains pending.

## Example verification

On 2026-10-21, Docker loaded `collector.env.example` into an isolated current-image
container. A two-event check passed native and OTLP ingestion, direct-mode status,
immediate read visibility, browser origin policies, protected reads, duplicate
accounting, complete payload/context/metric comparison and container replacement.
A temporary profiling override confirmed one DuckDB thread, the 16 MiB checkpoint
trigger and a writer-only profile. The test volume and containers were removed.
This was a configuration smoke check, not another load benchmark. The executable's
defaults, SDK and existing running demo were unchanged.
