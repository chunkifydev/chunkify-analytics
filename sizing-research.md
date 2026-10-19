# Collector sizing research

Checked on 2026-10-19 against DuckDB's current 1.5 documentation and the local
benchmark reports. This note proposes a validation baseline; it does not change
the application, benchmark configuration or supported capacity.

## Conclusion

Treat sustained 3,000 ingestion requests/s with concurrent dashboard queries on
2 CPUs and 2 GiB as an ambitious efficiency target. We have no basis to call it
impossible, and our short test passed. We also have no evidence that this small
configuration is a reliable deployment baseline for the complete workload.

For the main capacity test, use **4 vCPU, 8 GiB RAM and fast SSD storage** as an
engineering starting point. That recommendation follows the memory guidance and
leaves more room for the HTTP server, durable queue and concurrent database work.
It is our judgment, not a DuckDB guarantee of 3,000 requests/s.

## What DuckDB recommends

| Topic | Published guidance | Relevance here |
| --- | --- | --- |
| Memory | Aim for 1–4 GB per execution thread. Aggregation-heavy workloads typically need 1–2 GB per thread; join-heavy workloads need 3–4 GB. Query shape affects the requirement. | Our measured database uses two execution threads. The aggregation guideline alone suggests roughly 2–4 GB before allowing for the rest of this service. This is a heuristic, not a fixed reservation or hard minimum. |
| Storage | Persistent DuckDB performs best with SSD or NVMe storage. | Durable ingestion, checkpoints and query spills all make storage performance relevant. |

Source: [DuckDB environment guidance](https://duckdb.org/docs/current/guides/performance/environment).
The documentation uses GB; our container limits use GiB.

DuckDB's memory limit does not cover every allocation. Its troubleshooting guide
recommends fewer execution threads and a lower buffer memory limit when memory is
tight. It suggests 50–60% of available memory when allocations outside the buffer
manager cause process termination. That is troubleshooting advice, not a mandatory
setting for every installation.
[DuckDB out-of-memory guidance](https://duckdb.org/docs/current/guides/performance/oom)

DuckDB advises grouping inserts and avoiding a separate auto-committed transaction
for every row. Each commit requires a disk sync, so many small transactions can
reduce throughput. Our server already batches database writes; 3,000 HTTP requests
does not mean 3,000 DuckDB commits each second.
[DuckDB insertion guidance](https://duckdb.org/docs/current/data/insert)

None of these sources promises an HTTP request rate or a dashboard latency for
our schema, durable queue, queries and retention period.

## What our measurements establish

The [short comparison](read-worker.md) passed with 90,000 requests over 30 seconds,
concurrent reads and complete event verification. With the separate reader,
mean container CPU consumption was 0.90 cores and sampled peak container memory
was 853.40 MiB. This makes an absolute claim of impossibility unjustified.

The [longer attempt](stability.md) stopped after 101.189 seconds because the
separate load generator exceeded its own 1 GiB memory guard. The collector did
not suffer an out-of-memory termination. Its sampled peak container memory was
1,673.03 MiB, and summary/event-list p95 exceeded 500 ms. Host memory pressure,
write pauses and query waits also increased. We cannot attribute the slowdown
solely to the collector's CPU or RAM allocation.

Both reports record DuckDB 1.5.6 with two execution threads and a 1.5 GiB memory
limit. That limit belongs to the shared database instance; the separate reader
does not receive another independent 1.5 GiB allowance. The Rust server, SQLite
queue and other allocations also fit within the container's 2 GiB ceiling.

The measured mean request body was 1,666 bytes, so 3,000 requests/s corresponds to
about 5 MB/s of incoming bodies. Parsing, deduplication, durable writes and reads
add work beyond that byte rate. Dataset size and read frequency must accompany
any future capacity claim. At continuous operation, this rate adds 259.2 million
events per day; a five-minute pass would not establish performance over 30 days
of retained data.

## Proposed next step

Fix the generator's live memory usage, then validate the complete workload using
4 vCPU / 8 GiB and SSD storage. Keep the same delivery, durability and latency
checks. Keep 2 CPU / 2 GiB as an optional later efficiency test rather than the
condition that blocks progress.

Record the effective DuckDB settings when changing machine size. More available
CPUs can increase its automatic execution-thread count and therefore its memory
needs. Reserve memory for the rest of the process instead of giving the database
the whole container allowance. A 2 CPU / 4 GiB comparison could isolate RAM's
effect if that becomes the specific question; it is not another required run.

The generator needs its own adequate memory budget. Increasing collector RAM
alone will not repair the generator's memory failure. Avoid total host memory
pressure when running both on the same computer.
