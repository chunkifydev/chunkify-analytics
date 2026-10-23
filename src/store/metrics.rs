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
            let (sql, mut values) = view_cohort(project.clone(), through, &filters);
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

    pub async fn timeseries(
        &self,
        project: String,
        query: TimeseriesQuery,
    ) -> Result<Value, ApiError> {
        let window = query.validate()?;
        self.read("timeseries", move |connection| {
            let through = watermark(connection, &project)?;
            let (sql, mut values) = view_cohort(project, through, &window.filters);
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

fn view_cohort(
    project: String,
    through: i64,
    filters: &crate::query::Filters,
) -> (String, Vec<SqlValue>) {
    let (conditions, filter_values) = filters.sql();
    let mut values = vec![SqlValue::BigInt(through), SqlValue::Text(project.clone())];
    values.extend(filter_values);
    // Date, identity and context filters select starts. Each selected view contributes its
    // whole cumulative value, even when its final event occurs after the date range.
    // Group starts and measurements by identity in one scan. Then choose the
    // earliest start across identities, before applying any cohort filters.
    // arg_max_null keeps a winning NULL measurement instead of using an older value.
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
            arg_max_null(playing_time_ms,
                (event_name = 'chunkify.video.view_ended', sequence, occurred_at_ms, ingest_id))
                FILTER (WHERE event_name IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended')) AS playing_time_ms
        FROM events
        WHERE ingest_id <= ? AND project_id = ?
            AND event_name IN ('chunkify.video.view_started', 'chunkify.video.view_snapshot', 'chunkify.video.view_ended')
        GROUP BY view_id, video_id, viewer_id
    ), started AS (
        SELECT view_id, video_id, viewer_id,
            first_start.occurred_at_ms AS occurred_at_ms,
            first_start.context_browser_name AS context_browser_name,
            first_start.context_os_name AS context_os_name,
            first_start.context_device_type AS context_device_type,
            COALESCE(playing_time_ms, 0) AS playing_time_ms
        FROM per_identity WHERE start_order IS NOT NULL
        QUALIFY ROW_NUMBER() OVER (PARTITION BY view_id ORDER BY start_order) = 1
    ), view_metrics AS (
        SELECT occurred_at_ms, video_id, viewer_id, playing_time_ms FROM started WHERE {conditions}
    )");
    (sql, values)
}
