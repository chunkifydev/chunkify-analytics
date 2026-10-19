# Writer batch comparison, 19 October 2026

A focused diagnostic compared batch caps of 100 and 1,000 events before choosing
settings for another full capacity run. The local debug binary includes commit
`fc340c3`, which removes repeated copies of the whole OTLP request during decoding.

## Results

Each case added 10,000 events to a copy of the same stopped preload database.
Runs used the order 100, 1,000, 1,000, 100 to check whether the result depended on
which setting ran first.

| Existing events | Batch cap | Elapsed time, two runs | Transactions per run |
| --- | --- | --- | --- |
| 10,000 | 100 | 5.55 s, 5.41 s | 100 |
| 10,000 | 1,000 | 2.63 s, 2.58 s | 20 |
| 200,000 | 100 | 6.90 s, 6.31 s | 100 |
| 200,000 | 1,000 | 2.74 s, 2.72 s | 20 |

Every measured event's native payload and client IP matched after graceful
restart. No measured event was missing or duplicated. The 1,000-event cap
produced batches averaging 500 events; it did not force every transaction to
contain 1,000 events. The larger cap reduced both insertion and commit time.

[Raw measurements](writer-batching-2026-10-19.json) include writer-stage timings,
queue waits, prepared bytes and the executable hash.

## Scope

These measurements used a native debug build on a shared Mac, with DuckDB
reporting 8 threads and a 12.7 GiB memory limit. The traffic was one enriched
SDK view-start template with unique identities, 100 events per OTLP request and
ten concurrent requests. Both cases used direct ingestion, a 10 ms batching
delay, a 16 MiB batch-byte budget, a 64 MiB prepared queue and a 64 MiB checkpoint
threshold. No reads ran during timed writes.

The result supports testing larger write batches. It does not establish
production requests per second, sustained throughput, crash durability or
performance with concurrent dashboard reads. It also does not transfer directly
to the default batch-byte budget, one-event requests or durable-queue mode.
No deployment defaults changed.

## Reproduction and next check

The reusable command is documented in the README's focused writer-batch
comparison section. Its smoke run checked all four cases with 1,000 preloaded
and 1,000 measured events. It additionally verified every original OTLP envelope
and every browser-context field after restart.

The next capacity comparison should use the current release image under the
intended CPU and RAM limits, changing only the batch limits. Keep the request
shape, read traffic, run duration and durability checks fixed. Judge the result
by accepted requests, rejections, generator omissions, backlog and latency,
not only average throughput.
