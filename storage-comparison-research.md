# Published analytics storage measurements

Checked on 2026-10-19. These are real published measurements, but the datasets and measurement boundaries differ. They establish useful reference points, not a storage target for this collector.

| Dataset and engine | Published rows | Published stored size | Derived bytes per row |
| --- | ---: | ---: | ---: |
| ClickBench web traffic, DuckDB, 2026-05-11 c6a.4xlarge run | 99,997,497 | 20,457,467,904 bytes, about 19.05 GiB | 204.58 |
| ClickHouse's GA4 website events, ClickHouse | About 5.75 million | 525.34 MiB compressed column data | About 95.8 |
| Statable web analytics event archive, ClickHouse | About 1.15 billion | 56 GiB on disk | About 52.3 |
| This collector's recorded mixed native/OTLP run | 344,991 | 476,356,786 bytes for the data directory after checkpoint | 1,380.78 |

The arithmetic divides stored bytes by rows. MiB and GiB use binary units. Rounded source counts make the GA4 and Statable figures approximate. Our directory includes more than event column data and contains 50,000 OTLP events plus 294,991 native events. It is not a pure native-event measurement.

## DuckDB reference

ClickBench publishes a production-derived, anonymized web analytics dataset with exactly 99,997,497 rows. Its DuckDB result records `data_size` as 20,457,467,904 bytes. The schema has 105 typed columns, including browser, OS, URL, referrer, screen and viewport dimensions, identifiers, timing measurements and campaign data. It is a broad event table, with context repeated in columns on each row. It does not store our full JSON envelopes or duplicate OTLP representation.

This is a useful example that repeated context can coexist with compact columnar storage without first moving it into a separate context table. It does not establish what size DuckDB should achieve on our records.

Sources: [result JSON](https://raw.githubusercontent.com/ClickHouse/ClickBench/main/duckdb/results/20260511/c6a.4xlarge.json), [schema](https://raw.githubusercontent.com/ClickHouse/ClickBench/main/duckdb/create.sql), [dataset and methodology](https://github.com/ClickHouse/ClickBench/blob/main/README.md).

## GA4 reference

ClickHouse reports 42 days of its own website's GA4 events. About 5.75 million rows occupy 525.34 MiB of compressed column data, versus 4.56 GiB uncompressed. Its schema retains event parameters and device, geography and traffic-source context as typed columns, maps and tuples.

The measurement is `sum(data_compressed_bytes)` for the daily table. It excludes the separate intraday table and is narrower than a whole database directory, which can also include indexes, metadata, logs and other tables. This is not Google's internal GA4 database footprint.

Source: [Enhancing Google Analytics Data with ClickHouse](https://clickhouse.com/blog/enhancing-google-analytics-data-with-clickhouse).

## Statable reference

Statable's August 2026 engineering article reports 1.15 billion web analytics events in 56 GiB on disk, compared with 283 GiB uncompressed. It stores events ordered by site and time, with monthly partitions. It also maintains separate session and bot tables, so event bytes alone do not measure the whole service's storage. The deployment has four replicas; the reported event archive should not be multiplied into a whole-cluster estimate without clarifying its measurement boundary.

The article reports hostname compression at 34.37 times, while random user and session identifiers compress only about 1.25 times. Repetition can therefore be inexpensive in a columnar representation; random identifiers and arbitrary payloads require separate measurement.

Source: [Every number Statable shows you comes out of ClickHouse](https://dev.statable.com/blog/statable-runs-on-clickhouse/).

## What Mux stores

Mux's 2020 architecture article describes a `views` table with almost 200 metric and dimension columns per video view. Its 2023 processing article distinguishes incoming event beacons from finalized views stored in the columnar database. A long view sends more beacons, so incoming event count and stored view count are different denominators.

These articles publish no absolute byte-per-view figure suitable for this table. They also do not prove that every raw beacon is discarded or describe all event-detail storage. Their historical retention statements should not replace current product retention documentation.

Sources: [How ClickHouse saved our data](https://www.mux.com/blog/from-russia-with-love-how-clickhouse-saved-our-data), [Migration to BadgerDB](https://www.mux.com/blog/migrating-to-badgerdb).

## Implication for our collector

Our roughly 1.35 KiB per event is higher than these structured analytics examples. That warrants measuring the contribution of JSON payloads, duplicated context, OTLP records, indexes and free blocks. It does not show that DuckDB itself is inefficient or that a separate context table is the right fix.

A useful comparison would ingest identical representative events into the current schema and a version using typed fields with one retained payload representation, then compare checkpointed files and ingestion/query performance. Keep the event count, event contents and query outputs identical. Test view-level records separately because they change the unit of stored data and may change available detail.
