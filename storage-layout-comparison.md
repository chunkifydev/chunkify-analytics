# Collector storage layout comparison

Measured on 2026-10-19. Both comparisons change storage representation while preserving event data.

## Production implementation

The collector now uses schema version 4, with a fixed typed field map and versioned context reconstruction in `src/storage.rs`. Row and bulk ingestion share the encoder. Original OTLP objects remain unchanged. Event retrieval reconstructs the full event and context; browser filters read typed columns. The page query limits rows before the OTLP join and decodes typed columns directly in Rust.

Startup migrates versions 1–3 transactionally. Migration preserves IDs, receipt times, ordering, stored context and the sequence. Legacy durable requests have their prepared memory estimates recalculated before replay. No deployed database was modified during implementation.

The final production comparison used 50,000 events per case, one pass through fresh databases, with the same fixture shared by all four cases. The mixed cases retained 7,143 original OTLP objects. Every event, context, receipt and original OTLP object passed reconstruction checks after reopening.

| Input | Previous layout MiB | Production MiB | Reduction | 100-event page median ms, previous → production |
| --- | ---: | ---: | ---: | ---: |
| Native | 95.26 | 33.26 | 65.08% | 71.39 → 97.93 |
| Mixed | 107.76 | 49.26 | 54.29% | 95.03 → 118.56 |

The 50,000-event sample has different per-event sizes from the earlier 200,000-event experiment. DuckDB block allocation, checkpoint timing and the newly captured SDK fixture affect the result. Use roughly 50–60% savings as an initial sizing estimate for comparable traffic, not a guaranteed ratio for every workload. A larger proportion of original OTLP data reduces the expected percentage because that representation is retained.

Full-event retrieval still costs about 24–27 ms more per 100-event page in this local development build. Summary timing remained similar; the buffering-total query was faster. These are warm sequential queries on a shared host, not release-server latency or concurrent-load guarantees. The comparison excludes the durable SQLite spool.

The migration can leave reusable blocks in an existing file. Fresh-file savings do not imply immediate physical file shrinkage after an in-place upgrade. Allow space for the new table and WAL, and back up before upgrading. Older collectors cannot open schema version 4. A stopped database may need a separate compaction operation to return all free space to the filesystem; automatic compaction is not part of this change.

Validation passed 62 HTTP/CLI integration tests, including upgrade from a v3 executable with pending durable requests, plus 9 Rust tests. The Rust tests cover numeric edge cases, null/missing values, historical context, multi-page migration, rollback and old queue estimates. Formatting, Clippy and OpenAPI checks also passed. The Rust tests now run in collector CI.

Measurements and source hashes: [storage-production-2026-10-19.json](storage-production-2026-10-19.json). Reproduce with a built SDK and `storage_comparison` example:

```sh
STORAGE_EVENTS=50000 STORAGE_REPEATS=1 STORAGE_VARIANTS=current,production \
  STORAGE_REPORT=services/analytics-collector/storage-production-local.json \
  node tests/collector/storage-comparison.mjs
```

## Earlier prototype comparison

The earlier experiment below used inferred field types and did not itself change the production collector.

## Measured results

Each case contains the same 200,000 canonical events. Sizes below are checkpointed database files, using MiB = 1,048,576 bytes.

| Layout | Native MiB | Native bytes/event | Mixed OTLP MiB | Mixed bytes/event |
| --- | ---: | ---: | ---: | ---: |
| Current | 259.76 | 1,361.90 | 323.26 | 1,694.82 |
| Typed fields | 123.26 | 646.25 | 179.76 | 942.47 |
| Typed fields and no second browser-context copy | 101.01 | 529.59 | 162.51 | 852.03 |

The native typed layout is 52.55% smaller than the current layout. Removing the second browser-context copy increases the saving to 61.11%. This latter step saves another 22.25 MiB, or 18.05% of the typed file. Every native case has passed full event/context reconstruction checks.

The current mixed case retains 28,572 original OTLP objects and passed the same verification. Its file is 63.50 MiB larger than the current native case. This is the observed marginal file-size cost for this mixed fixture, not an exact table-size attribution or a projection for an all-OTLP workload.

The mixed typed case is 44.39% smaller than its current-layout baseline. Adding context deduplication increases that saving to 49.73%. That extra step saves 17.25 MiB, or 9.60% of the mixed typed file.

All six cases passed verification: 200,000 reconstructed events per case, with all 28,572 original OTLP objects checked in each mixed case. View summary and buffering totals matched across layouts. No context information, event fields or original OTLP records were discarded. Temporary fixtures and databases have been removed. Full machine-readable measurements are in [storage-comparison-2026-10-19.json](/Users/ftatilon/Dev/GitHub/chunkify-player/services/analytics-collector/storage-comparison-2026-10-19.json).

## Timing observations

| Case | Insert seconds | Final checkpoint seconds | Summary ms | Buffering query ms | 100-event page ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| native / current | 142.31 | 0.00 | 132.53 | 205.17 | 155.66 |
| native / typed | 113.25 | 3.38 | 139.63 | 5.90 | 316.81 |
| native / typed_context | 92.36 | 3.21 | 133.53 | 5.71 | 327.72 |
| mixed / current | 254.60 | 9.27 | 170.58 | 266.41 | 318.72 |
| mixed / typed | 168.59 | 5.87 | 133.95 | 5.70 | 392.42 |
| mixed / typed_context | 139.82 | 5.88 | 129.28 | 5.73 | 392.96 |

The typed layouts reduced write time and the buffering query time in this run. Summary queries use existing scalar query columns in all layouts and showed no consistent storage-layout penalty. Returning full events was slower in the typed prototype: about twice the baseline time for native pages and 23% longer for mixed pages. These page times include SQL, JSON reconstruction and serialization; they do not isolate each cost. The query path needs profiling and measurements in a production build before migration.

Context deduplication added little to the typed page time in this run: 316.81 to 327.72 ms for native events, and 392.42 to 392.96 ms for mixed events. The added storage savings were 18.05% and 9.60% respectively. These small timing differences do not establish a repeatable regression on a shared host; the file-size savings are directly measured.

## Layouts

- `current` reproduces the collector's v3 tables, including the full event JSON, scalar query fields, derived context JSON and original OTLP table.
- `typed` moves scalar event properties and scalar browser properties into typed columns. Values already present in the scalar query columns are not repeated in the JSON remainder. Arrays, objects, mixed-type fields, explicit nulls and unknown properties remain in JSON. The derived context JSON remains unchanged.
- `typed_context` also removes the second stored copy of browser context from the derived context column. It retains request context and reconstructs the combined context when reading the event.

All layouts retain context at event granularity. This experiment does not move context to a shared view record. Viewport changes and IP changes within a view remain observable.

## Method

The driver captures real SDK event templates and generates one shared fixture with 200,000 events, eight events per view, 100 videos and 10,000 possible viewer IDs. It varies identifiers, cumulative metrics, browser context and request IPs. Some events contain nested custom data, Unicode or mixed string/numeric error codes. The captured template metadata in the JSON report describes the original workload helper; the driver overrides its video/viewer counts and adds synthetic browser context.

The native cases store no OTLP copies. The mixed cases retain an original SDK-generated OTLP object for every seventh event, giving 28,572 original records. Both cases use the same canonical event fixture. This isolates the added storage for OTLP; it does not exercise the HTTP decoder or claim that a real native event and its OTLP normalization preserve every custom null identically.

Each case uses a fresh database, 1,000-event transactions, the same primary/unique indexes, two DuckDB threads, a 512 MiB buffer memory limit and a 16 MiB checkpoint threshold. File sizes are measured after checkpointing and closing the connection. They include indexes, metadata and any reusable blocks. They are not compressed-column byte counts or minimum compacted sizes. The run excludes the collector's separate durable intake spool.

After reopening each database, the runner reconstructs and compares every event, derived context, receipt timestamp and original OTLP object against the fixture. Missing fields, explicit nulls, arrays and unknown properties are checked. JSON numbers compare by value without a floating point tolerance in this round-trip check.

Read measurements cover a view summary, summed buffering duration for ended views, and retrieval plus serialization of a 100-event page. Queries run warm, with seven samples after a warm-up. The summary preserves the relevant cohort/final-value rules for this fixture but omits API filters, cursor handling and duplicate starts. This is a local storage experiment using a development Rust build, not a release-server throughput benchmark. Shared-host timing is exploratory.

The measured comparison uses one complete pass through the six cases. The report records fixture and source hashes, engine version, host, schema fields, compression methods and individual timing samples. It does not establish run-to-run timing variance. The driver also supports repeated passes with reversed layout order. Capturing live SDK timings means a new invocation produces a different fixture; every case within an invocation uses exactly the same fixture.

## Why original OTLP records are retained

The saved implementation specification for [issue 381](https://github.com/chunkifydev/chunkify/issues/381) explicitly requires: "Preserve each original log record and its resource/scope context alongside the normalized event."

This is a collector product requirement. The [OTLP specification](https://opentelemetry.io/docs/specs/otlp/) defines telemetry encoding, transport and delivery; our decision to retain a second source representation comes from the collector specification.

The decoder in [src/otlp.rs](/Users/ftatilon/Dev/GitHub/chunkify-player/services/analytics-collector/src/otlp.rs) extracts log attributes into a canonical playback event, restores SDK nullable fields and validates it. It separately retains the original log record and its enclosing resource and scope metadata. Normalization alone does not preserve the whole source record. Examples include a sender's resource attributes, instrumentation scope, log body, trace/span identifiers and the original typed attribute representation when these are supplied.

The practical rationale is to retain evidence for diagnosing mapping problems and allow later processing of metadata that today's analytics schema does not use. Those are benefits of the implementation; the specification itself does not spell out all of those motivations. The retained object is parsed JSON, not a byte-for-byte archive of the HTTP request, headers or original batching.

The collector's event retrieval API returns this object under `otlp`. Summary calculations use the normalized event columns. The current retention and viewer-deletion paths delete both representations together. Thus original OTLP retention is a data-retrieval guarantee, not a prerequisite for computing views, engagement or buffering metrics.

Keeping it has a cost because the original attributes repeat much of the canonical playback event. Removing it would change that guarantee. A future alternative could retain only information needed to reconstruct the original record, including OTLP types and field presence, but that requires its own correctness work. This experiment preserves the original OTLP object in every layout.

## Recommendation

Keep the context data. Browser/device details and event-specific viewport or request information remain useful for investigating playback problems. First consider storing event scalars once as typed fields, with a lossless JSON remainder for properties that do not fit the schema.

Include removal of the second materialized copy of browser context in the proposed design, provided reads reconstruct the same result and the retrieval cost remains acceptable. Version that reconstruction so a later change in context derivation does not silently change historical API responses. The prototype verifies current behavior; it does not implement that production versioning or a migration.

Moving stable context to a view table is a separate option. A single unchanging context per view cannot represent all viewport or IP changes. It would need overrides or context versions. This experiment has not measured that design, and the observed savings do not require it.

Preserve original OTLP records while evaluating these layout changes. Reducing their retention or dropping them should be an explicit change to the retrieval guarantee, supported by a decision about whether original-record debugging and future reprocessing are needed.

## Reproduce

Run from the repository root:

```sh
npm run build --workspace @chunkify/analytics
cargo build --locked --manifest-path services/analytics-collector/Cargo.toml --example storage_comparison
cargo test --locked --manifest-path services/analytics-collector/Cargo.toml --example storage_comparison
STORAGE_REPEATS=1 node tests/collector/storage-comparison.mjs
```

The driver needs a temporary loopback HTTP listener to capture SDK wire output and at least 4 GiB of free temporary disk. It creates disposable databases and refuses to replace an existing database. It removes its temporary directory on normal completion or a caught failure. `STORAGE_EVENTS`, `STORAGE_REPEATS`, `STORAGE_BINARY` and `STORAGE_REPORT` override the defaults. A 1,000-event smoke run checks the procedure but is too small for useful byte-per-event comparisons because DuckDB allocates blocks.

The prototype discovers scalar types from the fixture before timed ingestion. A production implementation would need a versioned schema, migration, handling for future event fields, and measurements with production payload distributions and retention churn.
