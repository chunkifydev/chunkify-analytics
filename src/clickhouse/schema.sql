-- The tables after every migration in src/clickhouse/migrations, as ClickHouse
-- 26.10 prints them. Generated: do not edit. To change a table, add a migration,
-- then regenerate this file (see docs/development.md).

CREATE TABLE claims
(
    `kind` LowCardinality(String),
    `key` Int64,
    `term` Int64,
    `owner` String,
    `written_at` DateTime64(3) DEFAULT now64(3)
)
ENGINE = MergeTree
ORDER BY (kind, key, term)
TTL toDateTime(written_at) + toIntervalDay(1)
SETTINGS non_replicated_deduplication_window = 100000, index_granularity = 8192;

CREATE TABLE events
(
    `ingest_id` Int64,
    `event_id` String,
    `view_id` String,
    `video_id` String,
    `viewer_id` Nullable(String),
    `event_name` LowCardinality(String),
    `sequence` Int64,
    `occurred_at_ms` Int64,
    `playing_time_ms` Nullable(Float64),
    `received_at_ms` Int64,
    `event_remainder` String,
    `context_remainder` Nullable(String),
    `context_format` Int32,
    `context_browser_name` LowCardinality(Nullable(String)),
    `context_os_name` LowCardinality(Nullable(String)),
    `context_device_type` LowCardinality(Nullable(String)),
    `e_schema_version` Nullable(Int64),
    `e_occurred_at` Nullable(String),
    `e_sdk_name` Nullable(String),
    `e_sdk_version` Nullable(String),
    `e_connector_name` Nullable(String),
    `e_connector_version` Nullable(String),
    `e_player_name` Nullable(String),
    `e_player_version` Nullable(String),
    `e_delivery_hostname` Nullable(String),
    `e_environment` Nullable(String),
    `e_reason` Nullable(String),
    `e_startup_outcome` Nullable(String),
    `e_error_category` Nullable(String),
    `e_error_fatal` Nullable(Bool),
    `e_error_playhead_seconds` Nullable(Float64),
    `e_rebuffer_count` Nullable(Float64),
    `e_rebuffer_duration_ms` Nullable(Float64),
    `e_seek_count` Nullable(Float64),
    `e_seek_duration_ms` Nullable(Float64),
    `e_maximum_playhead_seconds` Nullable(Float64),
    `e_media_duration_seconds` Nullable(Float64),
    `e_startup_time_ms` Nullable(Float64),
    `e_first_video_frame_time_ms` Nullable(Float64),
    `e_rebuffer_ratio` Nullable(Float64),
    `e_furthest_progress_ratio` Nullable(Float64),
    `e_media_width_px` Nullable(Float64),
    `e_media_height_px` Nullable(Float64),
    `e_player_width_px` Nullable(Float64),
    `e_player_height_px` Nullable(Float64),
    `e_dropped_video_frames` Nullable(Float64),
    `e_total_video_frames` Nullable(Float64),
    `e_selected_rendition_width` Nullable(Float64),
    `e_selected_rendition_height` Nullable(Float64),
    `e_selected_rendition_bitrate_bps` Nullable(Float64),
    `e_selected_rendition_codec` Nullable(String),
    `e_selected_rendition_frame_rate` Nullable(Float64),
    `e_rendition_switch_count` Nullable(Float64),
    `e_delivery_request_count` Nullable(Float64),
    `e_delivery_transferred_bytes` Nullable(Float64),
    `e_delivery_throughput_bps` Nullable(Float64),
    `e_playhead_seconds` Nullable(Float64),
    `e_from_state` Nullable(String),
    `e_to_state` Nullable(String),
    `e_previous_state_duration_ms` Nullable(Float64),
    `e_previous_rendition_width` Nullable(Float64),
    `e_previous_rendition_height` Nullable(Float64),
    `e_previous_rendition_bitrate_bps` Nullable(Float64),
    `e_previous_rendition_frame_rate` Nullable(Float64),
    `e_previous_rendition_codec` Nullable(String),
    `e_seek_started_at` Nullable(String),
    `e_last_seeked_at` Nullable(String),
    `e_playback_resumed_at` Nullable(String),
    `e_state_before_seek` Nullable(String),
    `e_state_after_seek` Nullable(String),
    `e_from_playhead_seconds` Nullable(Float64),
    `e_to_playhead_seconds` Nullable(Float64),
    `e_seek_resume_wait_ms` Nullable(Float64),
    `e_seek_interaction_duration_ms` Nullable(Float64),
    `e_seek_outcome` Nullable(String),
    `e_user_id` Nullable(String),
    `e_video_title` Nullable(String),
    `e_subtitle_language` Nullable(String),
    `e_previous_subtitle_language` Nullable(String),
    `e_playback_rate` Nullable(Float64),
    `e_previous_playback_rate` Nullable(Float64),
    `b_schema_version` Nullable(Int64),
    `b_user_agent` Nullable(String),
    `b_browser_name` Nullable(String),
    `b_browser_version` Nullable(String),
    `b_os_name` Nullable(String),
    `b_os_version` Nullable(String),
    `b_device_type` Nullable(String),
    `b_device_family` Nullable(String),
    `b_language` Nullable(String),
    `b_language_prefix` Nullable(String),
    `b_timezone` Nullable(String),
    `b_timezone_offset_minutes` Nullable(Float64),
    `b_screen_width_px` Nullable(Float64),
    `b_screen_height_px` Nullable(Float64),
    `b_viewport_width_px` Nullable(Float64),
    `b_viewport_height_px` Nullable(Float64),
    `b_pixel_ratio` Nullable(Float64),
    `project_id` LowCardinality(String),
    `ingest_key_id` Nullable(String),
    `otlp_payload` Nullable(String) CODEC(ZSTD(3)),
    `ctx_continent` Nullable(String) MATERIALIZED if(JSONType(ifNull(context_remainder, '{}'), 'continent') = 'String', JSONExtractString(ifNull(context_remainder, '{}'), 'continent'), NULL),
    `ctx_country` Nullable(String) MATERIALIZED if(JSONType(ifNull(context_remainder, '{}'), 'country') = 'String', JSONExtractString(ifNull(context_remainder, '{}'), 'country'), NULL),
    `ctx_city` Nullable(String) MATERIALIZED if(JSONType(ifNull(context_remainder, '{}'), 'city') = 'String', JSONExtractString(ifNull(context_remainder, '{}'), 'city'), NULL),
    `ctx_page_origin` Nullable(String) MATERIALIZED if(JSONType(ifNull(context_remainder, '{}'), 'page_origin') = 'String', JSONExtractString(ifNull(context_remainder, '{}'), 'page_origin'), NULL),
    `ctx_page_path` Nullable(String) MATERIALIZED if(JSONType(ifNull(context_remainder, '{}'), 'page_path') = 'String', JSONExtractString(ifNull(context_remainder, '{}'), 'page_path'), NULL),
    `d_playback_rate` Nullable(String) MATERIALIZED toString(round(if(isNotNull(e_playback_rate), e_playback_rate, if((JSONType(event_remainder, 'playback_rate') IN ('Int64', 'UInt64', 'Double')), JSONExtractFloat(event_remainder, 'playback_rate'), NULL)), 2)),
    `c_custom` Map(String, String) MATERIALIZED if(event_name = 'chunkify.video.view_started', CAST(arrayMap(entry -> (entry.1, if(JSONType(entry.2) = 'String', JSONExtractString(entry.2), entry.2)), JSONExtractKeysAndValuesRaw(event_remainder, 'custom')), 'Map(String, String)'), map()),
    `n_startup_time_ms` Nullable(Float64) MATERIALIZED if((event_name NOT IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')), NULL, if(isNotNull(e_startup_time_ms), e_startup_time_ms, if((JSONType(event_remainder, 'startup_time_ms') IN ('Int64', 'UInt64', 'Double')), JSONExtractFloat(event_remainder, 'startup_time_ms'), NULL))),
    `n_rebuffer_duration_ms` Nullable(Float64) MATERIALIZED if((event_name NOT IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')), NULL, if(isNotNull(e_rebuffer_duration_ms), e_rebuffer_duration_ms, if((JSONType(event_remainder, 'rebuffer_duration_ms') IN ('Int64', 'UInt64', 'Double')), JSONExtractFloat(event_remainder, 'rebuffer_duration_ms'), NULL))),
    `n_media_duration_seconds` Nullable(Float64) MATERIALIZED if((event_name NOT IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')), NULL, if(isNotNull(e_media_duration_seconds), e_media_duration_seconds, if((JSONType(event_remainder, 'media_duration_seconds') IN ('Int64', 'UInt64', 'Double')), JSONExtractFloat(event_remainder, 'media_duration_seconds'), NULL))),
    `n_furthest_progress_ratio` Nullable(Float64) MATERIALIZED if((event_name NOT IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')), NULL, if(isNotNull(e_furthest_progress_ratio), e_furthest_progress_ratio, if((JSONType(event_remainder, 'furthest_progress_ratio') IN ('Int64', 'UInt64', 'Double')), JSONExtractFloat(event_remainder, 'furthest_progress_ratio'), NULL))),
    `stored_at_ms` Int64 MATERIALIZED toUnixTimestamp64Milli(now64(3)),
    INDEX events_event event_id TYPE bloom_filter(0.0001) GRANULARITY 1,
    INDEX events_stored stored_at_ms TYPE minmax GRANULARITY 1,
    INDEX events_view view_id TYPE bloom_filter GRANULARITY 4,
    INDEX events_video video_id TYPE bloom_filter GRANULARITY 4,
    INDEX events_occurred occurred_at_ms TYPE minmax GRANULARITY 4
)
ENGINE = MergeTree
PARTITION BY toYYYYMM(toDateTime(intDiv(received_at_ms, 1000), 'UTC'))
ORDER BY (project_id, ingest_id)
SETTINGS index_granularity = 8192;

CREATE TABLE registry
(
    `version` UInt64,
    `write_id` String,
    `document` String CODEC(ZSTD(3)),
    `written_at` DateTime64(3) DEFAULT now64(3)
)
ENGINE = MergeTree
ORDER BY version
SETTINGS non_replicated_deduplication_window = 100000, index_granularity = 8192;

CREATE TABLE `rollup`
(
    `project_id` LowCardinality(String),
    `dim` LowCardinality(String),
    `hour_ms` Int64,
    `generation` Int64,
    `value` Nullable(String),
    `video_id` String,
    `views` UInt64,
    `viewers` AggregateFunction(uniqExact, Nullable(String)),
    `playing_sum` Float64,
    `reached_end` UInt64,
    `progress_n` UInt64,
    `p25` UInt64,
    `p50` UInt64,
    `p75` UInt64,
    `p100` UInt64,
    `playing_n` UInt64,
    `playing_q` AggregateFunction(quantilesExactInclusive(0.5, 0.95), Nullable(Float64)),
    `b10` UInt64,
    `b30` UInt64,
    `b60` UInt64,
    `b300` UInt64,
    `bmax` UInt64,
    `startup_n` UInt64,
    `startup_q` AggregateFunction(quantilesExactInclusive(0.5, 0.95), Nullable(Float64)),
    `buffering_n` UInt64,
    `buffering_playing` Float64,
    `rebuffer_sum` Float64,
    `fatal_n` UInt64,
    `out_started` UInt64,
    `out_failed` UInt64,
    `out_ended_before` UInt64,
    `out_unknown` UInt64,
    `score_sum` Float64,
    `score_n` UInt64,
    `title` AggregateFunction(argMaxIf, Nullable(String), Int64, UInt8)
)
ENGINE = MergeTree
ORDER BY (project_id, dim, hour_ms, generation, value, video_id)
SETTINGS allow_nullable_key = 1, index_granularity = 8192;

CREATE TABLE rollup_hours
(
    `project_id` LowCardinality(String),
    `hour_ms` Int64,
    `generation` Int64,
    `min_received_ms` Int64,
    `max_received_ms` Int64
)
ENGINE = MergeTree
ORDER BY (project_id, hour_ms, generation)
SETTINGS index_granularity = 8192;

CREATE TABLE rollup_runs
(
    `version` Int64,
    `write_id` String,
    `since_ms` Int64,
    `through_ms` Int64,
    `completed` UInt8,
    `written_at` DateTime64(3) DEFAULT now64(3)
)
ENGINE = MergeTree
ORDER BY (version, completed)
SETTINGS non_replicated_deduplication_window = 100000, index_granularity = 8192;

CREATE TABLE rollup_views
(
    `project_id` LowCardinality(String),
    `generation` Int64,
    `view_id` String,
    `video_id` String,
    `viewer_id` Nullable(String),
    `hour_ms` Int64,
    `occurred_at_ms` Int64,
    `start_ingest_id` Int64,
    `hours` Array(Int64),
    `min_received_ms` Int64,
    `max_received_ms` Int64,
    `context_browser_name` LowCardinality(Nullable(String)),
    `context_os_name` LowCardinality(Nullable(String)),
    `context_device_type` LowCardinality(Nullable(String)),
    `ctx_continent` Nullable(String),
    `ctx_country` Nullable(String),
    `ctx_city` Nullable(String),
    `ctx_page_origin` Nullable(String),
    `ctx_page_path` Nullable(String),
    `e_subtitle_language` LowCardinality(Nullable(String)),
    `d_playback_rate` LowCardinality(Nullable(String)),
    `e_user_id` Nullable(String),
    `e_video_title` Nullable(String),
    `c_custom` Map(String, String),
    `playing_time_ms` Float64,
    `m_playing_time_ms` Nullable(Float64),
    `m_startup_time_ms` Nullable(Float64),
    `m_rebuffer_duration_ms` Nullable(Float64),
    `m_startup_outcome` Nullable(String),
    `m_reached_end` Nullable(UInt8),
    `m_progress_ratio` Nullable(Float64),
    `has_fatal_error` UInt8,
    `playback_score` Nullable(Float64),
    `startup_time_ms` Nullable(Float64),
    `rebuffer_duration_ms` Nullable(Float64),
    `startup_outcome` String,
    `has_buffering_sample` UInt8,
    INDEX rollup_views_view view_id TYPE bloom_filter GRANULARITY 1
)
ENGINE = MergeTree
ORDER BY (project_id, generation, view_id)
SETTINGS index_granularity = 8192;

CREATE TABLE schema_migrations
(
    `version` Int64,
    `dirty` UInt8,
    `sequence` UInt64
)
ENGINE = MergeTree
ORDER BY sequence
SETTINGS index_granularity = 8192;

CREATE TABLE view_states
(
    `project_id` LowCardinality(String),
    `view_id` String,
    `video_id` String,
    `viewer_id` Nullable(String),
    `first_start` AggregateFunction(argMinIf, Tuple(
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Map(String, String)), Tuple(
        Int64,
        Int64,
        Int64), UInt8),
    `start_order` AggregateFunction(minIf, Tuple(
        Int64,
        Int64,
        Int64), UInt8),
    `start_count` AggregateFunction(countIf, UInt8),
    `measurement` AggregateFunction(argMaxIf, Tuple(
        Nullable(Float64),
        Nullable(Float64),
        Nullable(Float64),
        Nullable(String),
        Nullable(UInt8),
        Nullable(Float64)), Tuple(
        UInt8,
        Int64,
        Int64,
        Int64), UInt8),
    `fatal_events` AggregateFunction(countIf, Nullable(UInt8)),
    `start_hours` AggregateFunction(groupUniqArrayIf, Int64, UInt8),
    `min_received` AggregateFunction(min, Int64),
    `max_received` AggregateFunction(max, Int64)
)
ENGINE = AggregatingMergeTree
ORDER BY (project_id, view_id, video_id, viewer_id)
SETTINGS allow_nullable_key = 1, index_granularity = 8192;

CREATE MATERIALIZED VIEW view_states_insert TO view_states
(
    `project_id` LowCardinality(String),
    `view_id` String,
    `video_id` String,
    `viewer_id` Nullable(String),
    `first_start` AggregateFunction(argMinIf, Tuple(
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Nullable(String),
        Map(String, String)), Tuple(
        Int64,
        Int64,
        Int64), UInt8),
    `start_order` AggregateFunction(minIf, Tuple(
        Int64,
        Int64,
        Int64), UInt8),
    `start_count` AggregateFunction(countIf, UInt8),
    `measurement` AggregateFunction(argMaxIf, Tuple(
        Nullable(Float64),
        Nullable(Float64),
        Nullable(Float64),
        Nullable(String),
        Nullable(UInt8),
        Nullable(Float64)), Tuple(
        UInt8,
        Int64,
        Int64,
        Int64), UInt8),
    `fatal_events` AggregateFunction(countIf, Nullable(UInt8)),
    `start_hours` AggregateFunction(groupUniqArrayIf, Int64, UInt8),
    `min_received` AggregateFunction(min, Int64),
    `max_received` AggregateFunction(max, Int64)
)
AS SELECT
    project_id,
    view_id,
    video_id,
    viewer_id,
    argMinIfState(tuple(context_browser_name, context_os_name, context_device_type, ctx_continent, ctx_country, ctx_city, ctx_page_origin, ctx_page_path, e_subtitle_language, d_playback_rate, e_user_id, e_video_title, c_custom), tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS first_start,
    minIfState(tuple(sequence, occurred_at_ms, ingest_id), event_name = 'chunkify.video.view_started') AS start_order,
    countIfState(event_name = 'chunkify.video.view_started') AS start_count,
    argMaxIfState(tuple(toNullable(playing_time_ms), toNullable(n_startup_time_ms), toNullable(n_rebuffer_duration_ms), toNullable(if(event_name = 'chunkify.video.view_ended', e_startup_outcome, NULL)), toNullable((event_name = 'chunkify.video.view_ended') AND (e_reason = 'ended')), toNullable(if((n_media_duration_seconds > 0) AND ((n_furthest_progress_ratio >= 0) AND (n_furthest_progress_ratio <= 1)), n_furthest_progress_ratio, NULL))), tuple(event_name = 'chunkify.video.view_ended', sequence, occurred_at_ms, ingest_id), (event_name IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended'))) AS measurement,
    countIfState((event_name = 'chunkify.video.playback_error') AND (e_error_fatal = true)) AS fatal_events,
    groupUniqArrayIfState((occurred_at_ms - (((occurred_at_ms % 3600000) + 3600000) % 3600000)), event_name = 'chunkify.video.view_started') AS start_hours,
    minState(received_at_ms) AS min_received,
    maxState(received_at_ms) AS max_received
FROM events
WHERE event_name IN ('chunkify.video.view_started', 'chunkify.video.view_snapshot', 'chunkify.video.view_ended', 'chunkify.video.playback_error')
GROUP BY
    project_id,
    view_id,
    video_id,
    viewer_id;
