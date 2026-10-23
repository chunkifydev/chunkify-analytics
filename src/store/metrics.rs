use super::*;
use crate::query::TimeseriesQuery;

impl Store {
    pub async fn summary(
        &self,
        project: String,
        query: Query,
        per_video: bool,
    ) -> Result<Value, ApiError> {
        if query.event_name.is_some()
            || (!per_video && (query.cursor.is_some() || query.limit.is_some()))
        {
            return Err(ApiError::bad("Unsupported summary query parameter"));
        }
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<String>::decode(value, "videos", &filters, &project))
            .transpose()?;
        self.read(if per_video { "videos" } else { "summary" }, move |connection| {
            let through = match &cursor { Some(c) => c.through, None => watermark(connection, &project)? };
            let (sql, mut values) = view_cohort(project.clone(), through, &filters, CohortMetrics::Basic);
            if !per_video {
                let result = connection.query_row(&format!("{sql}
                    SELECT COUNT(*), COUNT(DISTINCT viewer_id), COALESCE(SUM(playing_time_ms), 0) FROM view_metrics"),
                    params_from_iter(values.iter()), |row| Ok(json!({
                        "views": row.get::<_, i64>(0)?, "unique_viewers": row.get::<_, i64>(1)?,
                        "total_playing_time_ms": row.get::<_, f64>(2)?
                    })))?;
                return Ok(result);
            }
            let after = cursor.map_or(String::new(), |c| c.after);
            values.extend([SqlValue::Text(after.clone()), SqlValue::BigInt((limit + 1) as i64)]);
            let mut statement = connection.prepare(&format!("{sql}
                SELECT video_id, COUNT(*), COUNT(DISTINCT viewer_id), SUM(playing_time_ms) FROM view_metrics
                WHERE video_id > ? GROUP BY video_id ORDER BY video_id LIMIT ?"))?;
            let rows = statement.query_map(params_from_iter(values.iter()), |row| Ok((row.get::<_, String>(0)?, json!({
                "video_id": row.get::<_, String>(0)?, "views": row.get::<_, i64>(1)?,
                "unique_viewers": row.get::<_, i64>(2)?, "total_playing_time_ms": row.get::<_, f64>(3)?
            }))))?;
            let mut videos = Vec::new();
            let mut last = after;
            let mut next_cursor = None;
            for row in rows {
                let (id, video) = row?;
                if videos.len() == limit {
                    next_cursor = Some(Cursor { version: 1, project_id: project.clone(), kind: "videos".into(), after: last, through, filters: filters.clone() }.encode()?);
                    break;
                }
                last = id;
                videos.push(video);
            }
            Ok(json!({ "videos": videos, "next_cursor": next_cursor }))
        }).await
    }

    pub async fn engagement(&self, project: String, query: Query) -> Result<Value, ApiError> {
        if query.event_name.is_some() || query.cursor.is_some() || query.limit.is_some() {
            return Err(ApiError::bad("Unsupported engagement query parameter"));
        }
        let filters = query.filters()?;
        self.read("engagement", move |connection| {
            let through = watermark(connection, &project)?;
            let (sql, values) = view_cohort(project, through, &filters, CohortMetrics::Engagement);
            let (views, playing, reached_end) = connection.query_row(
                &format!(
                    "{sql}
                SELECT COUNT(*), COALESCE(SUM(playing_time_ms), 0),
                    COUNT(*) FILTER (WHERE measurement.reached_end)
                FROM view_metrics"
                ),
                params_from_iter(values.iter()),
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, f64>(1)?,
                        row.get::<_, i64>(2)?,
                    ))
                },
            )?;
            if !playing.is_finite() {
                return Err(ApiError::internal());
            }
            Ok(json!({
                "views": views, "total_playing_time_ms": playing,
                "average_playing_time_ms": (views > 0).then(|| playing / views as f64),
                "reached_end_views": reached_end,
                "reached_end_ratio": (views > 0).then(|| reached_end as f64 / views as f64),
            }))
        })
        .await
    }

    pub async fn quality(&self, project: String, query: Query) -> Result<Value, ApiError> {
        if query.event_name.is_some() || query.cursor.is_some() || query.limit.is_some() {
            return Err(ApiError::bad("Unsupported quality query parameter"));
        }
        let filters = query.filters()?;
        self.read("quality", move |connection| {
            let through = watermark(connection, &project)?;
            let (sql, values) = view_cohort(project, through, &filters, CohortMetrics::Quality);
            let (views, startup_samples, median, p95, buffering_samples, playing, buffering, affected) =
                connection.query_row(&format!("{sql}, quality_samples AS (
                SELECT *, measurement.startup_time_ms AS startup_time_ms,
                    measurement.rebuffer_duration_ms AS rebuffer_duration_ms,
                    measurement.playing_time_ms IS NOT NULL
                        AND measurement.rebuffer_duration_ms IS NOT NULL
                        AND (measurement.playing_time_ms > 0 OR measurement.rebuffer_duration_ms > 0)
                        AS has_buffering_sample
                FROM view_metrics
            )
            SELECT COUNT(*), COUNT(startup_time_ms),
                quantile_cont(startup_time_ms, 0.5), quantile_cont(startup_time_ms, 0.95),
                COUNT(*) FILTER (WHERE has_buffering_sample),
                COALESCE(SUM(playing_time_ms) FILTER (WHERE has_buffering_sample), 0),
                COALESCE(SUM(rebuffer_duration_ms) FILTER (WHERE has_buffering_sample), 0),
                COUNT(*) FILTER (WHERE has_fatal_error)
            FROM quality_samples"), params_from_iter(values.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?,
                    row.get::<_, Option<f64>>(2)?, row.get::<_, Option<f64>>(3)?,
                    row.get::<_, i64>(4)?, row.get::<_, f64>(5)?, row.get::<_, f64>(6)?, row.get::<_, i64>(7)?))
            })?;
            // Reject an unrepresentable aggregate instead of serializing infinity
            // as null or reporting a misleading zero ratio.
            if !playing.is_finite() || !buffering.is_finite()
                || median.is_some_and(|v| !v.is_finite()) || p95.is_some_and(|v| !v.is_finite()) {
                return Err(ApiError::internal());
            }
            // Scaling avoids overflow when two valid finite durations have a
            // sum outside DOUBLE's range.
            let scale = playing.max(buffering);
            let ratio = (scale > 0.0).then(|| (buffering / scale) / (playing / scale + buffering / scale));
            Ok(json!({
                "views": views,
                "startup_time_ms": {"sample_views": startup_samples, "median": median, "p95": p95},
                "buffering": {"sample_views": buffering_samples,
                    "total_playing_time_ms": playing, "total_rebuffer_duration_ms": buffering, "ratio": ratio},
                "fatal_errors": {"affected_views": affected,
                    "ratio": (views > 0).then(|| affected as f64 / views as f64)},
            }))
        }).await
    }

    pub async fn timeseries(
        &self,
        project: String,
        query: TimeseriesQuery,
    ) -> Result<Value, ApiError> {
        let window = query.validate()?;
        self.read("timeseries", move |connection| {
            let through = watermark(connection, &project)?;
            let (sql, mut values) =
                view_cohort(project, through, &window.filters, CohortMetrics::Basic);
            let width = window.interval.milliseconds();
            values.extend([
                SqlValue::BigInt(window.first_bucket_ms),
                SqlValue::BigInt(width),
            ]);
            // UTC bucket indices use integer arithmetic. The filtered starts are
            // never before the first bucket, including dates before Unix epoch.
            let mut statement = connection.prepare(&format!(
                "{sql}
                SELECT (occurred_at_ms - ?) // ? AS bucket_index,
                    COUNT(*), COUNT(DISTINCT viewer_id), SUM(playing_time_ms)
                FROM view_metrics GROUP BY bucket_index ORDER BY bucket_index"
            ))?;
            let rows = statement.query_map(params_from_iter(values.iter()), |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, f64>(3)?,
                ))
            })?;
            let mut buckets: Vec<_> = (0..window.bucket_count).map(|index| json!({
                "bucket_start": utc_timestamp(window.first_bucket_ms + index as i64 * width),
                "views": 0, "unique_viewers": 0, "total_playing_time_ms": 0.0,
            })).collect();
            for row in rows {
                let (index, views, viewers, playing) = row?;
                let bucket = buckets
                    .get_mut(index as usize)
                    .ok_or_else(ApiError::internal)?;
                bucket["views"] = json!(views);
                bucket["unique_viewers"] = json!(viewers);
                bucket["total_playing_time_ms"] = json!(playing);
            }
            Ok(json!({
                "from": utc_timestamp(window.from_ms), "to": utc_timestamp(window.to_ms),
                "interval": window.interval, "timezone": "UTC", "buckets": buckets,
            }))
        })
        .await
    }
}

fn utc_timestamp(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .expect("validated RFC3339 time")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

enum CohortMetrics {
    Basic,
    Quality,
    Engagement,
}

fn view_cohort(
    project: String,
    through: i64,
    filters: &crate::query::Filters,
    metrics: CohortMetrics,
) -> (String, Vec<SqlValue>) {
    let (conditions, filter_values) = filters.sql();
    let mut values = vec![SqlValue::BigInt(through), SqlValue::Text(project.clone())];
    values.extend(filter_values);
    // Date, identity and context filters select starts. Each selected view contributes its
    // whole cumulative value, even when its final event occurs after the date range.
    // Group starts and measurements by identity in one scan. Then choose the
    // earliest start across identities, before applying any cohort filters.
    // Selecting a struct keeps all measurements from the same winning event,
    // including NULL values. Basic metrics avoid reading extra quality columns.
    let (measurement_fields, error_aggregate, measurement_projection, error_events) = match metrics
    {
        CohortMetrics::Basic => (String::new(), "", "", ""),
        CohortMetrics::Quality => (
            format!(
                ", startup_time_ms := {}, rebuffer_duration_ms := {}",
                stored_number("startup_time_ms"),
                stored_number("rebuffer_duration_ms")
            ),
            ", bool_or(event_name = 'chunkify.video.playback_error' AND e_error_fatal = TRUE) AS has_fatal_error",
            ", measurement, has_fatal_error",
            ", 'chunkify.video.playback_error'",
        ),
        CohortMetrics::Engagement => (
            ", reached_end := (event_name = 'chunkify.video.view_ended' AND e_reason = 'ended')"
                .into(),
            "",
            ", measurement",
            "",
        ),
    };
    let sql = format!("WITH per_identity AS (
        SELECT view_id, video_id, viewer_id,
            arg_min(struct_pack(
                occurred_at_ms := occurred_at_ms,
                context_browser_name := context_browser_name,
                context_os_name := context_os_name,
                context_device_type := context_device_type
            ), (sequence, occurred_at_ms, ingest_id))
                FILTER (WHERE event_name = 'chunkify.video.view_started') AS first_start,
            min((sequence, occurred_at_ms, ingest_id))
                FILTER (WHERE event_name = 'chunkify.video.view_started') AS start_order,
            arg_max(struct_pack(playing_time_ms := playing_time_ms{measurement_fields}),
                (event_name = 'chunkify.video.view_ended', sequence, occurred_at_ms, ingest_id))
                FILTER (WHERE event_name IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')) AS measurement
            {error_aggregate}
        FROM events
        WHERE ingest_id <= ? AND project_id = ?
            AND event_name IN ('chunkify.video.view_started', 'chunkify.video.view_snapshot', 'chunkify.video.view_ended'{error_events})
        GROUP BY view_id, video_id, viewer_id
    ), started AS (
        SELECT view_id, video_id, viewer_id,
            first_start.occurred_at_ms AS occurred_at_ms,
            first_start.context_browser_name AS context_browser_name,
            first_start.context_os_name AS context_os_name,
            first_start.context_device_type AS context_device_type,
            COALESCE(measurement.playing_time_ms, 0) AS playing_time_ms{measurement_projection}
        FROM per_identity WHERE start_order IS NOT NULL
        QUALIFY ROW_NUMBER() OVER (PARTITION BY view_id ORDER BY start_order) = 1
    ), view_metrics AS (
        SELECT occurred_at_ms, video_id, viewer_id, playing_time_ms{measurement_projection} FROM started WHERE {conditions}
    )");
    (sql, values)
}

// Storage preserves some numeric representations in JSON, notably integral
// floats. These field names are fixed by this module, never supplied by callers.
fn stored_number(field: &str) -> String {
    format!(
        "COALESCE(e_{field}, CASE
        WHEN json_type(event_remainder, '$.{field}') IN ('BIGINT', 'UBIGINT', 'DOUBLE')
        THEN TRY_CAST(json_extract(event_remainder, '$.{field}') AS DOUBLE) END)"
    )
}
