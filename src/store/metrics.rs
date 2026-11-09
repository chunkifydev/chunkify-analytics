use super::*;
use crate::query::{GroupPosition, TimeseriesQuery, VideoSort};

use crate::query::MetricKind as MetricSet;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VideoPosition {
    video_id: String,
    views: i64,
    total_playing_time_ms: f64,
    unique_viewers: i64,
    score: Option<f64>,
}

impl Store {
    async fn breakdown(
        &self,
        project: String,
        query: Query,
        metrics: MetricSet,
    ) -> Result<Value, ApiError> {
        let group_by = query
            .group_by
            .ok_or_else(|| ApiError::bad("group_by is required"))?;
        if query.event_name.is_some() || query.sort_by.is_some() || query.order.is_some() {
            return Err(ApiError::bad("Unsupported breakdown query parameter"));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let kind = format!(
            "breakdown_{}_{}_views_desc",
            group_by.column(),
            metrics.name()
        );
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<GroupPosition>::decode(value, &kind, &filters, &project))
            .transpose()?;
        self.read_report(
            "breakdown",
            project.clone(),
            (&query, metrics.name()),
            export.is_some(),
            move |connection| {
                let through = match &cursor {
                    Some(cursor) => cursor.through,
                    None => watermark(connection, &project)?,
                };
                let cohort = match metrics {
                    MetricSet::Summary => CohortMetrics::Basic,
                    MetricSet::Quality => CohortMetrics::Quality,
                    MetricSet::Engagement => CohortMetrics::Engagement,
                };
                let (sql, mut values) = view_cohort(project.clone(), through, &filters, cohort);
                let column = group_by.column();
                let continuation = match cursor {
                    None => "TRUE".to_owned(),
                    Some(cursor) => cursor.after.continuation(column, &mut values),
                };
                values.push(SqlValue::BigInt((limit + 1) as i64));
                let (samples, source) = match metrics {
                    MetricSet::Quality => (QUALITY_SAMPLES, "quality_samples"),
                    _ => ("", "view_metrics"),
                };
                // Each view belongs to its first retained start's context. Keep
                // unknown context as NULL and group the full cohort in one snapshot.
                let mut statement = connection.prepare(&format!(
                    "{sql}{samples} SELECT {column}, {} FROM {source}
                GROUP BY {column} HAVING {continuation}
                ORDER BY COUNT(*) DESC, {column} ASC NULLS FIRST LIMIT ?",
                    metric_columns(metrics)
                ))?;
                let rows = query_rows(&mut statement, &values, export.is_some())?.mapped(|row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        read_metrics(row, 1, metrics)?,
                    ))
                });
                let mut groups = Vec::new();
                let mut last = None;
                let mut next_cursor = None;
                for row in rows {
                    let (value, views, data) = row?;
                    if groups.len() == limit {
                        next_cursor = Some(
                            Cursor {
                                version: 1,
                                project_id: project.clone(),
                                kind: kind.clone(),
                                after: last.expect("nonempty page"),
                                through,
                                filters: filters.clone(),
                            }
                            .encode()?,
                        );
                        break;
                    }
                    last = Some(GroupPosition {
                        views,
                        value: value.clone(),
                    });
                    crate::exports::emit(
                        &export,
                        &mut groups,
                        json!({"value":value,"data":metric_json(data)?}),
                    )?;
                }
                Ok(json!({"group_by":group_by,"groups":groups,"next_cursor":next_cursor}))
            },
        )
        .await
    }

    pub async fn summary(
        &self,
        project: String,
        query: Query,
        per_video: bool,
    ) -> Result<Value, ApiError> {
        if query.group_by.is_some() {
            if per_video {
                return Err(ApiError::bad("group_by is not supported by videos"));
            }
            return self.breakdown(project, query, MetricSet::Summary).await;
        }
        if query.event_name.is_some()
            || (!per_video
                && (query.cursor.is_some()
                    || query.limit.is_some()
                    || query.order.is_some()
                    || query.sort_by.is_some()))
        {
            return Err(ApiError::bad("Unsupported summary query parameter"));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let sort = query.sort_by.unwrap_or_default();
        let order = query.order.unwrap_or_default();
        let kind = format!("videos_{}_{}", sort.column(), order.as_str());
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<VideoPosition>::decode(value, &kind, &filters, &project))
            .transpose()?;
        if cursor.as_ref().is_some_and(|cursor| {
            cursor.after.views < 1
                || !cursor.after.total_playing_time_ms.is_finite()
                || cursor.after.total_playing_time_ms < 0.0
                || cursor.after.unique_viewers < 0
                || cursor
                    .after
                    .score
                    .is_some_and(|v| !v.is_finite() || !(0.0..=100.0).contains(&v))
        }) {
            return Err(ApiError::bad("Invalid video cursor position"));
        }
        self.read_report(if per_video { "videos" } else { "summary" }, project.clone(), (&query, per_video), export.is_some(), move |connection| {
            let through = match &cursor { Some(c) => c.through, None => watermark(connection, &project)? };
            let (sql, mut values) = view_cohort_with_labels(project.clone(), through, &filters, if per_video { CohortMetrics::Quality } else { CohortMetrics::Basic }, per_video);
            if !per_video {
                let result = connection.query_row(&format!("{sql}
                    SELECT COUNT(*), COUNT(DISTINCT viewer_id), COALESCE(SUM(playing_time_ms), 0) FROM view_metrics"),
                    params_from_iter(values.iter()), |row| Ok(json!({
                        "views": row.get::<_, i64>(0)?, "unique_viewers": row.get::<_, i64>(1)?,
                        "total_playing_time_ms": row.get::<_, f64>(2)?
                    })))?;
                return Ok(result);
            }
            let (direction, comparison) = match order {
                SortOrder::Asc => ("ASC", ">"),
                SortOrder::Desc => ("DESC", "<"),
            };
            let column = sort.column();
            let tie_breaker = if matches!(sort, VideoSort::VideoId) { "" } else { ", video_id ASC" };
            let mut continuation = "TRUE".to_owned();
            if let Some(cursor) = cursor {
                let after = cursor.after;
                if matches!(sort, VideoSort::VideoId) {
                    continuation = format!("video_id {comparison} ?");
                    values.push(SqlValue::Text(after.video_id));
                } else if matches!(sort, VideoSort::PlaybackScore) {
                    if let Some(score) = after.score {
                        continuation = format!("(score {comparison} ? OR (score = ? AND video_id > ?) OR score IS NULL)");
                        values.extend([SqlValue::Double(score), SqlValue::Double(score), SqlValue::Text(after.video_id)]);
                    } else {
                        continuation = "(score IS NULL AND video_id > ?)".into();
                        values.push(SqlValue::Text(after.video_id));
                    }
                } else {
                    continuation = format!("({column} {comparison} ? OR ({column} = ? AND video_id > ?))");
                    let metric = match sort {
                        VideoSort::Views => SqlValue::BigInt(after.views),
                        VideoSort::TotalPlayingTimeMs => SqlValue::Double(after.total_playing_time_ms),
                        VideoSort::UniqueViewers => SqlValue::BigInt(after.unique_viewers),
                        VideoSort::VideoId | VideoSort::PlaybackScore => unreachable!(),
                    };
                    values.extend([metric.clone(), metric, SqlValue::Text(after.video_id)]);
                }
            }
            values.push(SqlValue::BigInt((limit + 1) as i64));
            // Aggregate the entire filtered cohort before ranking or applying
            // the cursor. Only fixed enum values become SQL identifiers.
            // A fixed summation order keeps floating-point sort keys stable
            // when the next page recomputes the same cohort.
            let mut statement = connection.prepare(&format!("{sql}
                , video_metrics AS (
                    SELECT video_id, COUNT(*) AS views, COUNT(DISTINCT viewer_id) AS unique_viewers,
                        SUM(playing_time_ms ORDER BY view_id) AS total_playing_time_ms,
                        AVG(playback_score ORDER BY view_id) AS score, COUNT(playback_score) AS score_samples,
                        arg_max(e_video_title, (occurred_at_ms, view_id)) FILTER (WHERE e_video_title IS NOT NULL) AS video_title
                    FROM view_metrics GROUP BY video_id
                )
                SELECT video_id, views, unique_viewers, total_playing_time_ms, score, score_samples, video_title FROM video_metrics
                WHERE {continuation} ORDER BY {column} {direction} NULLS LAST{tie_breaker} LIMIT ?"))?;
            let rows = query_rows(&mut statement,&values,export.is_some())?.mapped(|row| Ok((VideoPosition {
                video_id: row.get(0)?, views: row.get(1)?, total_playing_time_ms: row.get(3)?, unique_viewers: row.get(2)?, score: row.get(4)?,
            }, json!({
                "video_id": row.get::<_, String>(0)?, "video_title": row.get::<_, Option<String>>(6)?,
                "views": row.get::<_, i64>(1)?,
                "unique_viewers": row.get::<_, i64>(2)?, "total_playing_time_ms": row.get::<_, f64>(3)?,
                "playback_score":score_json(row.get(4)?, row.get(5)?)
            }))));
            let mut videos = Vec::new();
            let mut last = None;
            let mut next_cursor = None;
            for row in rows {
                let (position, video) = row?;
                if !position.total_playing_time_ms.is_finite() {
                    return Err(ApiError::internal());
                }
                if videos.len() == limit {
                    next_cursor = Some(Cursor { version: 1, project_id: project.clone(), kind: kind.clone(), after: last.expect("nonempty video page"), through, filters: filters.clone() }.encode()?);
                    break;
                }
                last = Some(position);
                crate::exports::emit(&export,&mut videos,video)?;
            }
            Ok(json!({ "videos": videos, "next_cursor": next_cursor }))
        }).await
    }

    pub async fn engagement(&self, project: String, query: Query) -> Result<Value, ApiError> {
        if query.group_by.is_some() {
            return self.breakdown(project, query, MetricSet::Engagement).await;
        }
        if query.event_name.is_some()
            || query.sort_by.is_some()
            || query.cursor.is_some()
            || query.limit.is_some()
            || query.order.is_some()
        {
            return Err(ApiError::bad("Unsupported engagement query parameter"));
        }
        let filters = query.filters()?;
        self.read_report(
            "engagement",
            project.clone(),
            &query,
            query.export.is_some(),
            move |connection| {
                let through = watermark(connection, &project)?;
                let (sql, values) =
                    view_cohort(project, through, &filters, CohortMetrics::Engagement);
                let values = connection.query_row(
                    &format!(
                        "{sql} SELECT {} FROM view_metrics",
                        metric_columns(MetricSet::Engagement)
                    ),
                    params_from_iter(values.iter()),
                    |row| read_metrics(row, 0, MetricSet::Engagement),
                )?;
                metric_json(values)
            },
        )
        .await
    }

    pub async fn quality(&self, project: String, query: Query) -> Result<Value, ApiError> {
        if query.group_by.is_some() {
            return self.breakdown(project, query, MetricSet::Quality).await;
        }
        if query.event_name.is_some()
            || query.sort_by.is_some()
            || query.cursor.is_some()
            || query.limit.is_some()
            || query.order.is_some()
        {
            return Err(ApiError::bad("Unsupported quality query parameter"));
        }
        let filters = query.filters()?;
        self.read_report(
            "quality",
            project.clone(),
            &query,
            query.export.is_some(),
            move |connection| {
                let through = watermark(connection, &project)?;
                let (sql, values) = view_cohort(project, through, &filters, CohortMetrics::Quality);
                let values = connection.query_row(
                    &format!(
                        "{sql}{QUALITY_SAMPLES} SELECT {} FROM quality_samples",
                        metric_columns(MetricSet::Quality)
                    ),
                    params_from_iter(values.iter()),
                    |row| read_metrics(row, 0, MetricSet::Quality),
                )?;
                metric_json(values)
            },
        )
        .await
    }

    pub async fn timeseries(
        &self,
        project: String,
        query: TimeseriesQuery,
    ) -> Result<Value, ApiError> {
        let metrics = query.kind;
        let cache_query = serde_json::to_value(&query).map_err(|_| ApiError::internal())?;
        let export = query.export.clone();
        let window = if export.is_some() {
            query.validate_export()?
        } else {
            query.validate()?
        };
        self.read_report(
            "timeseries",
            project.clone(),
            cache_query,
            export.is_some(),
            move |connection| {
                let through = watermark(connection, &project)?;
                let cohort = match metrics {
                    MetricSet::Summary => CohortMetrics::Basic,
                    MetricSet::Quality => CohortMetrics::Quality,
                    MetricSet::Engagement => CohortMetrics::Engagement,
                };
                let (sql, mut values) = view_cohort(project, through, &window.filters, cohort);
                let width = window.interval.milliseconds();
                values.extend([
                    SqlValue::BigInt(window.first_bucket_ms),
                    SqlValue::BigInt(width),
                ]);
                let (samples, source) = if matches!(metrics, MetricSet::Quality) {
                    (QUALITY_SAMPLES, "quality_samples")
                } else {
                    ("", "view_metrics")
                };
                let mut statement = connection.prepare(&format!(
                    "{sql}{samples} SELECT (occurred_at_ms - ?) // ? AS bucket_index,
                    {} FROM {source} GROUP BY bucket_index ORDER BY bucket_index",
                    metric_columns(metrics)
                ))?;
                let rows = query_rows(&mut statement, &values, export.is_some())?
                    .mapped(|row| Ok((row.get::<_, i64>(0)?, read_metrics(row, 1, metrics)?)));
                let empty = metric_json(match metrics {
                    MetricSet::Summary => MetricValues::Summary {
                        views: 0,
                        viewers: 0,
                        playing: 0.0,
                    },
                    MetricSet::Engagement => MetricValues::Engagement {
                        views: 0,
                        playing: 0.0,
                        reached_end: 0,
                        progress: ProgressMetrics::default(),
                        watch_time: WatchTimeMetrics::default(),
                    },
                    MetricSet::Quality => MetricValues::Quality {
                        views: 0,
                        startup_samples: 0,
                        median: None,
                        p95: None,
                        buffering_samples: 0,
                        playing: 0.0,
                        buffering: 0.0,
                        affected: 0,
                        outcomes: StartupOutcomes::default(),
                        score: None,
                        score_samples: 0,
                    },
                })?;
                if let Some(export) = &export {
                    let mut index = 0usize;
                    let bucket = |mut data: Value, index: usize| {
                        data["bucket_start"] =
                            json!(utc_timestamp(window.first_bucket_ms + index as i64 * width));
                        data
                    };
                    for row in rows {
                        let (next, values) = row?;
                        while index < next as usize {
                            export.push(bucket(empty.clone(), index))?;
                            index += 1;
                        }
                        export.push(bucket(metric_json(values)?, index))?;
                        index += 1;
                    }
                    while index < window.bucket_count {
                        export.push(bucket(empty.clone(), index))?;
                        index += 1;
                    }
                    return Ok(json!({"buckets":[]}));
                }
                let mut buckets: Vec<_> = (0..window.bucket_count)
                    .map(|index| {
                        let mut bucket = empty.clone();
                        bucket["bucket_start"] =
                            json!(utc_timestamp(window.first_bucket_ms + index as i64 * width));
                        bucket
                    })
                    .collect();
                for row in rows {
                    let (index, data) = row?;
                    let bucket = buckets
                        .get_mut(index as usize)
                        .ok_or_else(ApiError::internal)?;
                    let start = bucket["bucket_start"].clone();
                    *bucket = metric_json(data)?;
                    bucket["bucket_start"] = start;
                }
                Ok(json!({"kind":metrics,
                    "from": utc_timestamp(window.from_ms), "to": utc_timestamp(window.to_ms),
                    "interval": window.interval, "timezone": "UTC", "buckets": buckets,
                }))
            },
        )
        .await
    }
}

pub(super) fn utc_timestamp(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .expect("validated RFC3339 time")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

const QUALITY_SAMPLES: &str = ", quality_samples AS (
    SELECT *, measurement.startup_time_ms AS startup_time_ms,
        measurement.rebuffer_duration_ms AS rebuffer_duration_ms,
        COALESCE(measurement.startup_outcome,
            CASE WHEN measurement.startup_time_ms IS NOT NULL OR measurement.playing_time_ms > 0
                THEN 'started' ELSE 'unknown' END) AS startup_outcome,
        measurement.playing_time_ms IS NOT NULL
            AND measurement.rebuffer_duration_ms IS NOT NULL
            AND (measurement.playing_time_ms > 0 OR measurement.rebuffer_duration_ms > 0)
            AS has_buffering_sample
    FROM view_metrics
)";

fn metric_columns(metrics: MetricSet) -> &'static str {
    match metrics {
        MetricSet::Summary => "COUNT(*), COUNT(DISTINCT viewer_id), COALESCE(SUM(playing_time_ms ORDER BY view_id), 0)",
        MetricSet::Engagement => "COUNT(*), COALESCE(SUM(playing_time_ms ORDER BY view_id), 0), COUNT(*) FILTER (WHERE measurement.reached_end),
            COUNT(measurement.progress_ratio),
            COUNT(*) FILTER (WHERE measurement.progress_ratio >= 0.25),
            COUNT(*) FILTER (WHERE measurement.progress_ratio >= 0.5),
            COUNT(*) FILTER (WHERE measurement.progress_ratio >= 0.75),
            COUNT(*) FILTER (WHERE measurement.progress_ratio >= 1),
            COUNT(measurement.playing_time_ms),
            quantile_cont(measurement.playing_time_ms, 0.5),
            quantile_cont(measurement.playing_time_ms, 0.95),
            COUNT(*) FILTER (WHERE measurement.playing_time_ms < 10000),
            COUNT(*) FILTER (WHERE measurement.playing_time_ms >= 10000 AND measurement.playing_time_ms < 30000),
            COUNT(*) FILTER (WHERE measurement.playing_time_ms >= 30000 AND measurement.playing_time_ms < 60000),
            COUNT(*) FILTER (WHERE measurement.playing_time_ms >= 60000 AND measurement.playing_time_ms < 300000),
            COUNT(*) FILTER (WHERE measurement.playing_time_ms >= 300000)",
        MetricSet::Quality => "COUNT(*), COUNT(startup_time_ms),
            quantile_cont(startup_time_ms, 0.5), quantile_cont(startup_time_ms, 0.95),
            COUNT(*) FILTER (WHERE has_buffering_sample),
            COALESCE(SUM(playing_time_ms ORDER BY view_id) FILTER (WHERE has_buffering_sample), 0),
            COALESCE(SUM(rebuffer_duration_ms ORDER BY view_id) FILTER (WHERE has_buffering_sample), 0),
            COUNT(*) FILTER (WHERE has_fatal_error),
            COUNT(*) FILTER (WHERE startup_outcome = 'started'),
            COUNT(*) FILTER (WHERE startup_outcome = 'failed_before_start'),
            COUNT(*) FILTER (WHERE startup_outcome = 'ended_before_start'),
            COUNT(*) FILTER (WHERE startup_outcome = 'unknown'),
            AVG(playback_score ORDER BY view_id), COUNT(playback_score)",
    }
}

fn count_ratio(views: i64, denominator: i64) -> Value {
    json!({"views":views,"ratio":(denominator > 0).then(|| views as f64 / denominator as f64)})
}

#[derive(Default)]
struct ProgressMetrics {
    sample_views: i64,
    milestones: [i64; 4],
}

impl ProgressMetrics {
    fn json(self) -> Value {
        let mut data = json!({"sample_views":self.sample_views});
        for (name, count) in [
            "at_25_percent",
            "at_50_percent",
            "at_75_percent",
            "at_100_percent",
        ]
        .into_iter()
        .zip(self.milestones)
        {
            data[name] = count_ratio(count, self.sample_views);
        }
        data
    }
}

#[derive(Default)]
struct WatchTimeMetrics {
    sample_views: i64,
    median: Option<f64>,
    p95: Option<f64>,
    distribution: [i64; 5],
}

impl WatchTimeMetrics {
    fn is_finite(&self) -> bool {
        [self.median, self.p95]
            .into_iter()
            .flatten()
            .all(f64::is_finite)
    }

    fn json(self) -> Value {
        let names = [
            "under_10_seconds",
            "from_10_to_30_seconds",
            "from_30_to_60_seconds",
            "from_1_to_5_minutes",
            "at_least_5_minutes",
        ];
        let distribution: serde_json::Map<String, Value> = names
            .into_iter()
            .zip(self.distribution)
            .map(|(name, count)| (name.into(), json!(count)))
            .collect();
        json!({"sample_views":self.sample_views,"median":self.median,"p95":self.p95,
            "distribution":distribution})
    }
}

#[derive(Default)]
struct StartupOutcomes {
    started: i64,
    failed: i64,
    exited: i64,
    unknown: i64,
}

impl StartupOutcomes {
    fn json(self, views: i64) -> Value {
        json!({"sample_views":views - self.unknown,
            "started":count_ratio(self.started, views),
            "failed_before_start":count_ratio(self.failed, views),
            "ended_before_start":count_ratio(self.exited, views),
            "unknown":count_ratio(self.unknown, views)})
    }
}

enum MetricValues {
    Summary {
        views: i64,
        viewers: i64,
        playing: f64,
    },
    Engagement {
        views: i64,
        playing: f64,
        reached_end: i64,
        progress: ProgressMetrics,
        watch_time: WatchTimeMetrics,
    },
    Quality {
        views: i64,
        startup_samples: i64,
        median: Option<f64>,
        p95: Option<f64>,
        buffering_samples: i64,
        playing: f64,
        buffering: f64,
        affected: i64,
        outcomes: StartupOutcomes,
        score: Option<f64>,
        score_samples: i64,
    },
}

fn read_metrics(
    row: &duckdb::Row<'_>,
    offset: usize,
    metrics: MetricSet,
) -> duckdb::Result<MetricValues> {
    Ok(match metrics {
        MetricSet::Summary => MetricValues::Summary {
            views: row.get(offset)?,
            viewers: row.get(offset + 1)?,
            playing: row.get(offset + 2)?,
        },
        MetricSet::Engagement => MetricValues::Engagement {
            views: row.get(offset)?,
            playing: row.get(offset + 1)?,
            reached_end: row.get(offset + 2)?,
            progress: ProgressMetrics {
                sample_views: row.get(offset + 3)?,
                milestones: [
                    row.get(offset + 4)?,
                    row.get(offset + 5)?,
                    row.get(offset + 6)?,
                    row.get(offset + 7)?,
                ],
            },
            watch_time: WatchTimeMetrics {
                sample_views: row.get(offset + 8)?,
                median: row.get(offset + 9)?,
                p95: row.get(offset + 10)?,
                distribution: [
                    row.get(offset + 11)?,
                    row.get(offset + 12)?,
                    row.get(offset + 13)?,
                    row.get(offset + 14)?,
                    row.get(offset + 15)?,
                ],
            },
        },
        MetricSet::Quality => MetricValues::Quality {
            views: row.get(offset)?,
            startup_samples: row.get(offset + 1)?,
            median: row.get(offset + 2)?,
            p95: row.get(offset + 3)?,
            buffering_samples: row.get(offset + 4)?,
            playing: row.get(offset + 5)?,
            buffering: row.get(offset + 6)?,
            affected: row.get(offset + 7)?,
            outcomes: StartupOutcomes {
                started: row.get(offset + 8)?,
                failed: row.get(offset + 9)?,
                exited: row.get(offset + 10)?,
                unknown: row.get(offset + 11)?,
            },
            score: row.get(offset + 12)?,
            score_samples: row.get(offset + 13)?,
        },
    })
}

fn metric_json(values: MetricValues) -> Result<Value, ApiError> {
    match values {
        MetricValues::Summary {
            views,
            viewers,
            playing,
        } => {
            if !playing.is_finite() {
                return Err(ApiError::internal());
            }
            Ok(json!({"views":views,"unique_viewers":viewers,"total_playing_time_ms":playing}))
        }
        MetricValues::Engagement {
            views,
            playing,
            reached_end,
            progress,
            watch_time,
        } => {
            if !playing.is_finite() || !watch_time.is_finite() {
                return Err(ApiError::internal());
            }
            Ok(json!({"views":views,"total_playing_time_ms":playing,
                "average_playing_time_ms":(views > 0).then(|| playing / views as f64),
                "reached_end_views":reached_end,"reached_end_ratio":(views > 0).then(|| reached_end as f64 / views as f64),
                "progress_reached":progress.json(), "watch_time_ms":watch_time.json()}))
        }
        MetricValues::Quality {
            views,
            startup_samples,
            median,
            p95,
            buffering_samples,
            playing,
            buffering,
            affected,
            outcomes,
            score,
            score_samples,
        } => {
            if !playing.is_finite()
                || !buffering.is_finite()
                || median.is_some_and(|v| !v.is_finite())
                || p95.is_some_and(|v| !v.is_finite())
            {
                return Err(ApiError::internal());
            }
            // Scale before summing durations so the ratio stays representable.
            let scale = playing.max(buffering);
            let ratio =
                (scale > 0.0).then(|| (buffering / scale) / (playing / scale + buffering / scale));
            Ok(json!({"views":views,
                "startup_time_ms":{"sample_views":startup_samples,"median":median,"p95":p95},
                "buffering":{"sample_views":buffering_samples,"total_playing_time_ms":playing,
                    "total_rebuffer_duration_ms":buffering,"ratio":ratio},
                "fatal_errors":{"affected_views":affected,"ratio":(views > 0).then(|| affected as f64 / views as f64)},
                "startup_outcomes":outcomes.json(views),
                "playback_score":score_json(score, score_samples)}))
        }
    }
}

// A bounded playback heuristic, not a perceptual picture-quality metric.
// Weighted geometric mean: 30% startup (8 / (8 + seconds)), 70% smoothness
// (exp(-10 * buffering ratio)). Applying the smoothness weight gives exp(-7 * ratio).
// Missing inputs remain NULL; observed failures are measured zero scores.
const PLAYBACK_SCORE: &str = ", CASE
    WHEN COALESCE(has_fatal_error, FALSE) OR measurement.startup_outcome = 'failed_before_start' THEN 0.0
    WHEN measurement.playing_time_ms > 0
        AND measurement.startup_time_ms IS NOT NULL
        AND measurement.rebuffer_duration_ms IS NOT NULL
        AND COALESCE(measurement.startup_outcome, 'started') = 'started'
    THEN 100.0 * POWER(8000.0 / (8000.0 + measurement.startup_time_ms), 0.30)
        * EXP(-7.0 *
            (measurement.rebuffer_duration_ms / GREATEST(measurement.playing_time_ms, measurement.rebuffer_duration_ms)) /
            (measurement.playing_time_ms / GREATEST(measurement.playing_time_ms, measurement.rebuffer_duration_ms)
                + measurement.rebuffer_duration_ms / GREATEST(measurement.playing_time_ms, measurement.rebuffer_duration_ms)))
    END AS playback_score";

pub(super) fn score_json(value: Option<f64>, sample_views: i64) -> Value {
    json!({"value":value,"sample_views":sample_views})
}

pub(super) enum CohortMetrics {
    Basic,
    Quality,
    Engagement,
    Complete,
    Renditions,
    Retention,
}

pub(super) fn view_cohort(
    project: String,
    through: i64,
    filters: &crate::query::Filters,
    metrics: CohortMetrics,
) -> (String, Vec<SqlValue>) {
    let labels = matches!(metrics, CohortMetrics::Complete);
    view_cohort_with_labels(project, through, filters, metrics, labels)
}

/// Like `view_cohort`. `labels` carries each start's `user_id` and
/// `video_title` into `view_metrics` for reports that return them. They are
/// also carried whenever a `user_id` filter needs them; otherwise they are NULL
/// so other reports do not hold two extra strings per view.
pub(super) fn view_cohort_with_labels(
    project: String,
    through: i64,
    filters: &crate::query::Filters,
    metrics: CohortMetrics,
    labels: bool,
) -> (String, Vec<SqlValue>) {
    let labels = labels || filters.selects_user_id();
    let (label_fields, label_projection) = if labels {
        (
            ", e_user_id := e_user_id, e_video_title := e_video_title",
            "first_start.e_user_id AS e_user_id, first_start.e_video_title AS e_video_title,",
        )
    } else {
        (
            "",
            "CAST(NULL AS VARCHAR) AS e_user_id, CAST(NULL AS VARCHAR) AS e_video_title,",
        )
    };
    let (conditions, filter_values) = filters.sql();
    let mut values = vec![SqlValue::BigInt(through), SqlValue::Text(project.clone())];
    values.extend(filter_values);
    // Date, identity and context filters select starts. Each selected view contributes its
    // whole cumulative value, even when its final event occurs after the date range.
    // Group starts and measurements by identity in one scan. Then choose the
    // earliest start across identities, before applying any cohort filters.
    // Selecting a struct keeps all measurements from the same winning event,
    // including NULL values. Basic metrics avoid reading extra quality columns.
    let score_projection = if matches!(metrics, CohortMetrics::Quality | CohortMetrics::Complete) {
        PLAYBACK_SCORE
    } else {
        ""
    };
    let (measurement_fields, error_aggregate, measurement_projection, error_events) = match metrics
    {
        CohortMetrics::Basic => (String::new(), "", "", ""),
        CohortMetrics::Retention => (
            format!(", media_duration_seconds := {}, played_ranges := json_extract(event_remainder, '$.played_ranges')", stored_number("media_duration_seconds")),
            "", ", measurement", "",
        ),
        CohortMetrics::Renditions => (
            ", rendition_playing_time := json_extract(event_remainder, '$.rendition_playing_time')".into(),
            "", ", measurement", "",
        ),
        CohortMetrics::Quality | CohortMetrics::Complete => (
            format!(
                ", startup_time_ms := {}, rebuffer_duration_ms := {}, startup_outcome := CASE WHEN event_name = 'chunkify.video.view_ended' THEN e_startup_outcome END, reached_end := (event_name = 'chunkify.video.view_ended' AND e_reason = 'ended'), has_ended_event := (event_name = 'chunkify.video.view_ended')",
                stored_number("startup_time_ms"),
                stored_number("rebuffer_duration_ms")
            ),
            ", bool_or(event_name = 'chunkify.video.playback_error' AND e_error_fatal = TRUE) AS has_fatal_error",
            ", measurement, has_fatal_error",
            ", 'chunkify.video.playback_error'",
        ),
        CohortMetrics::Engagement => (
            format!(", reached_end := (event_name = 'chunkify.video.view_ended' AND e_reason = 'ended'),
                progress_ratio := CASE WHEN {} > 0 AND {} BETWEEN 0 AND 1 THEN {} END",
                stored_number("media_duration_seconds"), stored_number("furthest_progress_ratio"), stored_number("furthest_progress_ratio")),
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
                context_device_type := context_device_type,
                context_remainder := context_remainder{label_fields}
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
            first_start.context_remainder AS context_remainder,
            {label_projection}
            COALESCE(measurement.playing_time_ms, 0) AS playing_time_ms{measurement_projection}
        FROM per_identity WHERE start_order IS NOT NULL
        QUALIFY ROW_NUMBER() OVER (PARTITION BY view_id ORDER BY start_order) = 1
    ), view_metrics AS (
        SELECT view_id, occurred_at_ms, video_id, viewer_id, e_user_id, e_video_title, context_browser_name, context_os_name, context_device_type, context_remainder, playing_time_ms{measurement_projection}{score_projection} FROM started WHERE {conditions}
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

#[cfg(test)]
mod score_tests {
    use super::*;

    #[test]
    fn playback_score_preserves_missing_data_and_bounds_extreme_inputs() {
        let db = Connection::open_in_memory().unwrap();
        let cases = [
            ("10000, 0, 0, 'started', FALSE", Some(100.0_f64)),
            ("10000, 10000, 0, 'started', FALSE", Some(78.40526816831157)),
            ("10000, 20000, 0, 'started', FALSE", Some(68.67198272427282)),
            ("9000, 0, 1000, 'started', FALSE", Some(49.658530379140956)),
            ("8000, 0, 2000, 'started', FALSE", Some(24.659696394160644)),
            (
                "1e308, 0, 1e308, 'started', FALSE",
                Some(3.0197383422318502),
            ),
            (
                "10000, 1e308, 0, 'started', FALSE",
                Some(5.901018770673883e-90),
            ),
            (
                "20719.29999923706, 4830.200000286102, 3.40000057220459, 'started', FALSE",
                Some(86.68783991421262),
            ),
            ("10000, 0, 0, NULL, FALSE", Some(100.0)),
            ("NULL, NULL, NULL, 'failed_before_start', FALSE", Some(0.0)),
            ("NULL, NULL, NULL, NULL, TRUE", Some(0.0)),
            ("10000, 0, 0, 'started', TRUE", Some(0.0)),
            ("10000, 0, 0, 'failed_before_start', FALSE", Some(0.0)),
            ("0, 0, 0, 'started', FALSE", None),
            ("10000, NULL, 0, 'started', FALSE", None),
            ("10000, 0, NULL, 'started', FALSE", None),
            ("10000, 0, 0, 'unknown', FALSE", None),
            ("10000, 0, 0, 'ended_before_start', FALSE", None),
        ];
        for (values, expected) in cases {
            let actual: Option<f64> = db.query_row(&format!(
                "SELECT playback_score FROM (SELECT 1 {PLAYBACK_SCORE} FROM (
                    SELECT struct_pack(playing_time_ms := playing::DOUBLE, startup_time_ms := startup::DOUBLE,
                        rebuffer_duration_ms := rebuffer::DOUBLE, startup_outcome := outcome::VARCHAR) AS measurement,
                        fatal AS has_fatal_error
                    FROM (VALUES ({values})) AS v(playing, startup, rebuffer, outcome, fatal)))"
            ), [], |row| row.get(0)).unwrap();
            match (actual, expected) {
                (Some(actual), Some(expected)) => {
                    assert!(
                        actual.is_finite() && (0.0..=100.0).contains(&actual),
                        "{values}: {actual}"
                    );
                    let tolerance = 1e-12 * expected.abs().max(1e-100);
                    assert!(
                        (actual - expected).abs() <= tolerance,
                        "{values}: {actual} != {expected}"
                    );
                }
                (actual, expected) => assert_eq!(actual, expected, "{values}"),
            }
        }
    }
}
