-- Adds delivery_format (hls or file) as a first-start dimension. Every
-- statement is safe to run again.
--
-- The derived tables are dropped and recreated with the new column, so no
-- server may run during this migration: stop the servers, migrate, then start
-- them. Reports keep the same data because every dropped table is derived
-- from events.

-- Events stored before this migration keep a delivery_format they carried in
-- event_remainder: servers of earlier releases accept unknown fields there.
-- The default reads valid values back, so existing parts report them.
ALTER TABLE events ADD COLUMN IF NOT EXISTS e_delivery_format Nullable(String) DEFAULT if(JSONExtractString(event_remainder, 'delivery_format') IN ('hls', 'file'), JSONExtractString(event_remainder, 'delivery_format'), NULL) AFTER e_previous_playback_rate;

-- Drop the derived tables, the materialized view first so no insert writes
-- to a dropped table. ClickHouse otherwise refuses to drop a table larger
-- than 50 GB.
DROP VIEW IF EXISTS view_states_insert SETTINGS max_table_size_to_drop = 0;
DROP TABLE IF EXISTS view_states SETTINGS max_table_size_to_drop = 0;
DROP TABLE IF EXISTS rollup SETTINGS max_table_size_to_drop = 0;
DROP TABLE IF EXISTS rollup_hours SETTINGS max_table_size_to_drop = 0;
DROP TABLE IF EXISTS rollup_views SETTINGS max_table_size_to_drop = 0;
DROP TABLE IF EXISTS rollup_runs SETTINGS max_table_size_to_drop = 0;

-- view_states with the delivery format of each view's first start, in its own
-- state (a tuple, so that a NULL value is kept). first_start keeps the type
-- 0001 gave it: migrate repeats 0001 after a partial restore, and 0001's
-- materialized view must still be able to write to this table. The SELECT
-- only gives the table its columns; view_states_insert below must keep the
-- same SELECT.
CREATE TABLE IF NOT EXISTS view_states
ENGINE = AggregatingMergeTree
ORDER BY (project_id, view_id, video_id, viewer_id)
SETTINGS allow_nullable_key = 1
EMPTY AS
SELECT
    project_id,
    view_id,
    video_id,
    viewer_id,
    argMinIfState(tuple(context_browser_name, context_os_name, context_device_type, ctx_continent, ctx_country, ctx_city, ctx_page_origin, ctx_page_path, e_subtitle_language, d_playback_rate, e_user_id, e_video_title, c_custom), tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS first_start,
    argMinIfState(tuple(e_delivery_format), tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS first_delivery_format,
    minIfState(tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS start_order,
    countIfState(event_name = 'chunkify.video.view_started') AS start_count,
    argMaxIfState(tuple(toNullable(playing_time_ms), toNullable(n_startup_time_ms), toNullable(n_rebuffer_duration_ms), toNullable(if(event_name = 'chunkify.video.view_ended', e_startup_outcome, NULL)), toNullable(event_name = 'chunkify.video.view_ended' AND e_reason = 'ended'), toNullable(if(n_media_duration_seconds > 0 AND n_furthest_progress_ratio BETWEEN 0 AND 1, n_furthest_progress_ratio, NULL))), tuple(event_name = 'chunkify.video.view_ended', sequence, occurred_at_ms, ingest_id), event_name IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')) AS measurement,
    countIfState(event_name = 'chunkify.video.playback_error' AND e_error_fatal = true) AS fatal_events,
    groupUniqArrayIfState((occurred_at_ms - ((occurred_at_ms % 3600000) + 3600000) % 3600000), event_name = 'chunkify.video.view_started') AS start_hours,
    minState(received_at_ms) AS min_received,
    maxState(received_at_ms) AS max_received
FROM events
WHERE event_name IN ('chunkify.video.view_started', 'chunkify.video.view_snapshot', 'chunkify.video.view_ended', 'chunkify.video.playback_error')
GROUP BY project_id, view_id, video_id, viewer_id;

-- Keeps view_states current as events are inserted.
CREATE MATERIALIZED VIEW IF NOT EXISTS view_states_insert TO view_states AS
SELECT
    project_id,
    view_id,
    video_id,
    viewer_id,
    argMinIfState(tuple(context_browser_name, context_os_name, context_device_type, ctx_continent, ctx_country, ctx_city, ctx_page_origin, ctx_page_path, e_subtitle_language, d_playback_rate, e_user_id, e_video_title, c_custom), tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS first_start,
    argMinIfState(tuple(e_delivery_format), tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS first_delivery_format,
    minIfState(tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS start_order,
    countIfState(event_name = 'chunkify.video.view_started') AS start_count,
    argMaxIfState(tuple(toNullable(playing_time_ms), toNullable(n_startup_time_ms), toNullable(n_rebuffer_duration_ms), toNullable(if(event_name = 'chunkify.video.view_ended', e_startup_outcome, NULL)), toNullable(event_name = 'chunkify.video.view_ended' AND e_reason = 'ended'), toNullable(if(n_media_duration_seconds > 0 AND n_furthest_progress_ratio BETWEEN 0 AND 1, n_furthest_progress_ratio, NULL))), tuple(event_name = 'chunkify.video.view_ended', sequence, occurred_at_ms, ingest_id), event_name IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')) AS measurement,
    countIfState(event_name = 'chunkify.video.playback_error' AND e_error_fatal = true) AS fatal_events,
    groupUniqArrayIfState((occurred_at_ms - ((occurred_at_ms % 3600000) + 3600000) % 3600000), event_name = 'chunkify.video.view_started') AS start_hours,
    minState(received_at_ms) AS min_received,
    maxState(received_at_ms) AS max_received
FROM events
WHERE event_name IN ('chunkify.video.view_started', 'chunkify.video.view_snapshot', 'chunkify.video.view_ended', 'chunkify.video.playback_error')
GROUP BY project_id, view_id, video_id, viewer_id;

-- Fill the states of every stored event. A repeated migration drops the
-- table above first, so no state is merged twice.
INSERT INTO view_states
SELECT
    project_id,
    view_id,
    video_id,
    viewer_id,
    argMinIfState(tuple(context_browser_name, context_os_name, context_device_type, ctx_continent, ctx_country, ctx_city, ctx_page_origin, ctx_page_path, e_subtitle_language, d_playback_rate, e_user_id, e_video_title, c_custom), tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS first_start,
    argMinIfState(tuple(e_delivery_format), tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS first_delivery_format,
    minIfState(tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS start_order,
    countIfState(event_name = 'chunkify.video.view_started') AS start_count,
    argMaxIfState(tuple(toNullable(playing_time_ms), toNullable(n_startup_time_ms), toNullable(n_rebuffer_duration_ms), toNullable(if(event_name = 'chunkify.video.view_ended', e_startup_outcome, NULL)), toNullable(event_name = 'chunkify.video.view_ended' AND e_reason = 'ended'), toNullable(if(n_media_duration_seconds > 0 AND n_furthest_progress_ratio BETWEEN 0 AND 1, n_furthest_progress_ratio, NULL))), tuple(event_name = 'chunkify.video.view_ended', sequence, occurred_at_ms, ingest_id), event_name IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')) AS measurement,
    countIfState(event_name = 'chunkify.video.playback_error' AND e_error_fatal = true) AS fatal_events,
    groupUniqArrayIfState((occurred_at_ms - ((occurred_at_ms % 3600000) + 3600000) % 3600000), event_name = 'chunkify.video.view_started') AS start_hours,
    minState(received_at_ms) AS min_received,
    maxState(received_at_ms) AS max_received
FROM events
WHERE event_name IN ('chunkify.video.view_started', 'chunkify.video.view_snapshot', 'chunkify.video.view_ended', 'chunkify.video.playback_error')
GROUP BY project_id, view_id, video_id, viewer_id;

-- Rollups with the delivery format. Empty tables make the first refresh
-- recompute every hour; until it completes, reports read events.
CREATE TABLE IF NOT EXISTS rollup_runs (
    version Int64,
    write_id String,
    since_ms Int64,
    through_ms Int64,
    completed UInt8,
    written_at DateTime64(3) DEFAULT now64(3)
)
ENGINE = MergeTree
ORDER BY (version, completed)
SETTINGS non_replicated_deduplication_window = 100000;

CREATE TABLE IF NOT EXISTS rollup_views (
    project_id LowCardinality(String),
    generation Int64,
    view_id String,
    video_id String,
    viewer_id Nullable(String),
    hour_ms Int64,
    occurred_at_ms Int64,
    start_ingest_id Int64,
    hours Array(Int64),
    min_received_ms Int64,
    max_received_ms Int64,
    context_browser_name LowCardinality(Nullable(String)),
    context_os_name LowCardinality(Nullable(String)),
    context_device_type LowCardinality(Nullable(String)),
    ctx_continent Nullable(String),
    ctx_country Nullable(String),
    ctx_city Nullable(String),
    ctx_page_origin Nullable(String),
    ctx_page_path Nullable(String),
    e_subtitle_language LowCardinality(Nullable(String)),
    d_playback_rate LowCardinality(Nullable(String)),
    e_delivery_format LowCardinality(Nullable(String)),
    e_user_id Nullable(String),
    e_video_title Nullable(String),
    c_custom Map(String, String),
    playing_time_ms Float64,
    m_playing_time_ms Nullable(Float64),
    m_startup_time_ms Nullable(Float64),
    m_rebuffer_duration_ms Nullable(Float64),
    m_startup_outcome Nullable(String),
    m_reached_end Nullable(UInt8),
    m_progress_ratio Nullable(Float64),
    has_fatal_error UInt8,
    playback_score Nullable(Float64),
    startup_time_ms Nullable(Float64),
    rebuffer_duration_ms Nullable(Float64),
    startup_outcome String,
    has_buffering_sample UInt8,
    INDEX rollup_views_view view_id TYPE bloom_filter GRANULARITY 1
)
ENGINE = MergeTree
ORDER BY (project_id, generation, view_id);

CREATE TABLE IF NOT EXISTS rollup_hours (
    project_id LowCardinality(String),
    hour_ms Int64,
    generation Int64,
    min_received_ms Int64,
    max_received_ms Int64
)
ENGINE = MergeTree
ORDER BY (project_id, hour_ms, generation);

CREATE TABLE IF NOT EXISTS rollup (
    project_id LowCardinality(String),
    dim LowCardinality(String),
    hour_ms Int64,
    generation Int64,
    value Nullable(String),
    video_id String,
    views UInt64,
    viewers AggregateFunction(uniqExact, Nullable(String)),
    playing_sum Float64,
    reached_end UInt64,
    progress_n UInt64,
    p25 UInt64,
    p50 UInt64,
    p75 UInt64,
    p100 UInt64,
    playing_n UInt64,
    playing_q AggregateFunction(quantilesExactInclusive(0.5, 0.95), Nullable(Float64)),
    b10 UInt64,
    b30 UInt64,
    b60 UInt64,
    b300 UInt64,
    bmax UInt64,
    startup_n UInt64,
    startup_q AggregateFunction(quantilesExactInclusive(0.5, 0.95), Nullable(Float64)),
    buffering_n UInt64,
    buffering_playing Float64,
    rebuffer_sum Float64,
    fatal_n UInt64,
    out_started UInt64,
    out_failed UInt64,
    out_ended_before UInt64,
    out_unknown UInt64,
    score_sum Float64,
    score_n UInt64,
    title AggregateFunction(argMaxIf, Nullable(String), Int64, UInt8)
)
ENGINE = MergeTree
ORDER BY (project_id, dim, hour_ms, generation, value, video_id)
SETTINGS allow_nullable_key = 1;
