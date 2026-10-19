# Generator memory investigation

The [five-minute stability attempt](stability.md) stopped when the load generator
crossed its 1,024 MiB RSS guard. On 2026-10-19, isolated sender tests reproduced
that failure twice. Giving Node an explicit heap budget prevented the same RSS
failure in two subsequent runs, including one that sent 966,746 events.

This is a generator fix, not a new collector capacity result. The
[raw evidence](generator-memory-2026-10-19.json) contains resource samples,
diagnostic scripts, commands/settings, and the two Docker launcher checks.

## Diagnosis

On this Mac, Node 24.18.0 permitted a 4,288 MiB V8 heap while the benchmark stopped
at 1,024 MiB process RSS. V8 could therefore grow its heap beyond our process
budget before collecting enough transient allocations. At the final sample of
the first reproduction, RSS was about 1,079 MiB and heap capacity about 975 MiB,
although only about 363 MiB of the heap was in use.

We considered both retained state (replay descriptions, acknowledgement IDs and
latency arrays) and temporary HTTP allocations. The controlled change was the
Node heap budget. These results support fixing that mismatch first; they do not
establish that retained state can grow indefinitely or rule out other costs.

### Isolated sender test

The diagnostic used the actual benchmark sender, enriched workload, timing arrays
and verification ledgers. A separate local Node process consumed request bodies
and returned HTTP 202. Docker lifecycle/statistics and database verification were
stubbed. No collector, disk persistence, dashboard reads or preload participated.
No explicit garbage collection ran during traffic. The script and its source
substitution are archived in the raw evidence as one-off diagnostic material.

All runs offered 10,000 requests/s with one event per request, up to 4,096 requests
in flight and a one-million-event safety ceiling. Development services and other
applications stayed running on the shared 16 GiB Apple M2 Mac.

| Run | Planned duration | Elapsed | Acknowledged events | Late / concurrency omissions | Peak sampled RSS | Outcome |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| Default heap, first | 50 s | 30.395 s | 299,386 | 2,868 / 0 | 1,078.81 MiB | RSS guard stopped traffic |
| Default heap, repeat | 50 s | 12.922 s | 117,489 | 10,651 / 329 | 1,041.98 MiB | RSS guard stopped traffic |
| 512 MiB old generation | 50 s | 50.145 s | 491,775 | 8,225 / 0 | 594.17 MiB | Completed without memory abort |
| Same budget, extended | 115 s | 115.417 s | 966,746 | 182,463 / 791 | 659.23 MiB | Completed without memory abort; count assertion failed |

The extended run expected exactly one million acknowledgements and exited with
status 1 because scheduling omissions left it at 966,746. It exceeded the
950,000-event dataset planned for the collector stability test, but did not
exercise a full million events. Neither bounded-heap run establishes complete
delivery at 10,000 requests/s. The receiver also offers no persistence guarantee.

## Change

The public `load.mjs` command now starts `load-worker.mjs` with:

```text
--max-old-space-size=512 --max-semi-space-size=64
```

The tested runtime reports a 704 MiB total V8 heap limit. This leaves room for
other process allocations below the existing 1,024 MiB RSS guard. The worker
rejects an effective heap limit above 768 MiB before any Docker operation, so an
overriding runtime setting cannot silently invalidate the intended budget.
Heap limits are not RSS limits; the sampled RSS guard still applies.

The diagnostic changed only the old-generation setting; its observed total heap
limit was also 704 MiB. The launcher fixes both settings explicitly and was then
checked through the Docker tests below.

The launcher forwards arguments, environment, SIGINT/SIGTERM and worker exit
status. The optional `--expose-gc` runtime flag remains available. Report version
5 records runtime flags and the effective heap limit; samples now include used
and total heap, external memory and array-buffer memory. Source hashing includes
the launcher and worker. The collector and benchmark thresholds did not change.

## Regression and integration checks

The initial regression test failed before the launcher fix: the observed heap
limit was 4,288 MiB against the test's 768 MiB ceiling. After the change, ten
focused checks passed using:

```sh
node --max-old-space-size=512 --max-semi-space-size=64 \
  --test tests/collector/docker/runtime.test.mjs \
  tests/collector/docker/verification.test.mjs
```

These check the heap budget and unchanged RSS guard, argument/environment/error
forwarding, refusal of an oversized worker heap, complete payload verification,
missing acknowledgements, repeated pages, altered canonical payloads, altered
OTLP context, and preload/fixture event ceilings.

Two small tests used the public launcher and the existing collector image, with
the durable queue, separate reader and browser context enabled:

- **Normal completion:** 100 requests/s for five seconds. All 500 requests were
  acknowledged, with no omissions or HTTP failures. Full payloads and summaries
  were verified after restart; both benchmark threshold sets passed.
- **Cancellation:** SIGTERM was sent to the launcher during a planned 60-second
  run at 100 requests/s. The worker stopped traffic, verified all 802 acknowledged
  events after restart and cleaned up. The launcher exited with the worker's
  interruption code, 130, and the report status was `interrupted`. Its overall
  benchmark pass flags correctly remained false.

The image/source identity and complete reports are in the raw evidence. These
checks establish launcher and verification behavior, not sustained capacity.
No disposable test containers or volumes remained afterward. Development
services were not stopped for these checks.

## Review and next step

Independent Standards and Spec reviews found no actionable implementation issues
or discrepancies between this report and the recorded evidence.

The reproduced generator RSS failure is addressed within these test conditions.
The next capacity test must still check request omissions, collector memory,
query latency, queue drain and complete persistence after restart. The earlier
short 3,000-request/s pass remains the strongest complete-delivery result.

Keep the one-million-event ceiling and existing service thresholds. Choose and
record the collector resource budget before repeating sustained traffic. No
disk-backed ledger rewrite is justified by this diagnosis alone.
