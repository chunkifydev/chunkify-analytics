//! Analytics reports in ClickHouse SQL. Each report mirrors the local storage
//! report in `crate::store`, including its cohort rules, ordering, cursors and
//! response shape, so HTTP, MCP, exports and dashboards see the same API.
use super::{
    Params, Store,
    rollup::{Combined, Partials},
};
use crate::{
    api::ApiError,
    exports::emit_async,
    query::{
        Cursor, Dimension, Filters, GroupPosition, MetricKind as MetricSet, Query, SortOrder,
        TimeseriesQuery, VideoSort, ViewsQuery,
    },
};
use serde_json::{Value, json};

const STARTED: &str = "'chunkify.video.view_started'";
const SNAPSHOT: &str = "'chunkify.video.view_snapshot'";
const ENDED: &str = "'chunkify.video.view_ended'";
const ERROR: &str = "'chunkify.video.playback_error'";

#[derive(Clone, Copy)]
pub(super) enum Cohort {
    Basic,
    Quality,
    Engagement,
    Complete,
    Renditions,
    Subtitles,
    PlaybackRates,
    Retention,
    /// Every measurement the rollups store, plus each view's start hours and
    /// receipt range across all of its identities.
    Rollup,
}

/// Further limits on a cohort, used by rollup refreshes and combined reads.
#[derive(Default)]
pub(super) struct Narrow {
    /// A condition on view starts. Candidates need a matching start.
    pub starts: Option<String>,
    /// Also require the condition of each view's first start.
    pub first_start: bool,
    /// Read the identities of these views (a `view_id` subquery) from their
    /// states in `view_states` instead of aggregating events.
    pub states: Option<String>,
}

/// The measurement each cohort reads from a view's final or latest snapshot or
/// end event, after playing time: names and expressions over events.
fn measurement(cohort: Cohort) -> Vec<(&'static str, String)> {
    let reached_end = format!("event_name = {ENDED} AND e_reason = 'ended'");
    let progress_ratio = || {
        let duration = stored_number("media_duration_seconds");
        let progress = stored_number("furthest_progress_ratio");
        format!("if({duration} > 0 AND {progress} BETWEEN 0 AND 1, {progress}, NULL)")
    };
    match cohort {
        Cohort::Basic => Vec::new(),
        Cohort::Retention => vec![
            (
                "media_duration_seconds",
                stored_number("media_duration_seconds"),
            ),
            ("played_ranges", json_array("played_ranges")),
        ],
        Cohort::Renditions => vec![(
            "rendition_playing_time",
            json_array("rendition_playing_time"),
        )],
        Cohort::Subtitles => vec![("subtitle_playing_time", json_array("subtitle_playing_time"))],
        Cohort::PlaybackRates => vec![(
            "playback_rate_playing_time",
            json_array("playback_rate_playing_time"),
        )],
        Cohort::Quality | Cohort::Complete => vec![
            ("startup_time_ms", stored_number("startup_time_ms")),
            (
                "rebuffer_duration_ms",
                stored_number("rebuffer_duration_ms"),
            ),
            (
                "startup_outcome",
                format!("if(event_name = {ENDED}, e_startup_outcome, NULL)"),
            ),
            ("reached_end", reached_end),
            ("has_ended_event", format!("event_name = {ENDED}")),
        ],
        Cohort::Engagement => vec![
            ("reached_end", reached_end),
            ("progress_ratio", progress_ratio()),
        ],
        Cohort::Rollup => vec![
            ("startup_time_ms", stored_number("startup_time_ms")),
            (
                "rebuffer_duration_ms",
                stored_number("rebuffer_duration_ms"),
            ),
            (
                "startup_outcome",
                format!("if(event_name = {ENDED}, e_startup_outcome, NULL)"),
            ),
            ("reached_end", reached_end),
            ("progress_ratio", progress_ratio()),
        ],
    }
}

fn reads_errors(cohort: Cohort) -> bool {
    matches!(cohort, Cohort::Quality | Cohort::Complete | Cohort::Rollup)
}

/// Event names a cohort reads.
pub(super) fn cohort_events(cohort: Cohort) -> String {
    let errors = if reads_errors(cohort) {
        format!(", {ERROR}")
    } else {
        String::new()
    };
    format!("{STARTED}, {SNAPSHOT}, {ENDED}{errors}")
}

/// Aggregates over one identity's events (view, video and viewer): function,
/// arguments and name. Reports apply them to events; the `view_states`
/// materialized view keeps their states as events arrive, and rollup refreshes
/// merge those states with the same functions.
pub(super) fn identity_aggregates(
    cohort: Cohort,
    starts: &[&str],
) -> Vec<(&'static str, String, &'static str)> {
    let order = "tuple(sequence, occurred_at_ms, ingest_id)";
    let mut aggregates = Vec::new();
    if !starts.is_empty() {
        aggregates.push((
            "argMinIf",
            format!(
                "tuple({}), {order}, event_name = {STARTED}",
                starts.join(", ")
            ),
            "first_start",
        ));
    }
    aggregates.push((
        "minIf",
        format!("{order}, event_name = {STARTED}"),
        "start_order",
    ));
    aggregates.push(("countIf", format!("event_name = {STARTED}"), "start_count"));
    let values = std::iter::once("playing_time_ms".to_owned())
        .chain(
            measurement(cohort)
                .into_iter()
                .map(|(_, expression)| expression),
        )
        .map(|expression| format!("toNullable({expression})"))
        .collect::<Vec<_>>()
        .join(", ");
    aggregates.push((
        "argMaxIf",
        format!(
            "tuple({values}), tuple(event_name = {ENDED}, sequence, occurred_at_ms, ingest_id),
            event_name IN ({SNAPSHOT}, {ENDED})"
        ),
        "measurement",
    ));
    if reads_errors(cohort) {
        aggregates.push((
            "countIf",
            format!("event_name = {ERROR} AND e_error_fatal = true"),
            "fatal_events",
        ));
    }
    // Rollups record every hour in which a view started and the receipt range
    // of its events, across identities, to know which hours a view affects.
    if matches!(cohort, Cohort::Rollup) {
        aggregates.push((
            "groupUniqArrayIf",
            format!(
                "{}, event_name = {STARTED}",
                super::rollup::hour_of("occurred_at_ms")
            ),
            "start_hours",
        ));
        aggregates.push(("min", "received_at_ms".into(), "min_received"));
        aggregates.push(("max", "received_at_ms".into(), "max_received"));
    }
    aggregates
}

// Numeric report fields materialized at insert time; see `REPORT_NUMBERS`.
// These field names are fixed by this module, never supplied by callers.
fn stored_number(field: &str) -> String {
    debug_assert!(super::REPORT_NUMBERS.contains(&field));
    format!("n_{field}")
}

fn json_array(field: &str) -> String {
    format!(
        "if(JSONType(event_remainder, '{field}') = 'Array', JSONExtractRaw(event_remainder, '{field}'), NULL)"
    )
}

/// Each view's selected cumulative array `m_{field}` expanded to one row per
/// object entry, with `columns` computed from `entry`. Only the final event,
/// or latest snapshot, contributes, so time is never counted twice.
fn expanded_entries(field: &str, columns: &str) -> String {
    format!(
        "SELECT view_id, {columns}
        FROM view_metrics ARRAY JOIN JSONExtractArrayRaw(ifNull(m_{field}, '[]')) AS entry
        WHERE JSONType(entry) = 'Object'"
    )
}

/// Ranked reports over cumulative arrays have no pages, groups or sorting.
fn reject_ranking_parameters(query: &Query, message: &'static str) -> Result<(), ApiError> {
    if query.event_name.is_some()
        || query.group_by.is_some()
        || query.sort_by.is_some()
        || query.order.is_some()
        || query.cursor.is_some()
    {
        return Err(ApiError::bad(message));
    }
    Ok(())
}

fn json_number(source: &str, field: &str) -> String {
    format!(
        "if(JSONType({source}, '{field}') IN ('Int64', 'UInt64', 'Double'), JSONExtractFloat({source}, '{field}'), NULL)"
    )
}

// A bounded playback heuristic, not a perceptual picture-quality metric.
// Weighted geometric mean: 30% startup (8 / (8 + seconds)), 70% smoothness
// (exp(-10 * buffering ratio)). Applying the smoothness weight gives exp(-7 * ratio).
// Missing inputs remain NULL; observed failures are measured zero scores.
pub(super) const PLAYBACK_SCORE: &str = ", multiIf(
    has_fatal_error OR m_startup_outcome = 'failed_before_start', 0.0,
    m_playing_time_ms > 0
        AND m_startup_time_ms IS NOT NULL
        AND m_rebuffer_duration_ms IS NOT NULL
        AND ifNull(m_startup_outcome, 'started') = 'started',
    100.0 * pow(8000.0 / (8000.0 + m_startup_time_ms), 0.30)
        * exp(-7.0 *
            (m_rebuffer_duration_ms / greatest(m_playing_time_ms, m_rebuffer_duration_ms)) /
            (m_playing_time_ms / greatest(m_playing_time_ms, m_rebuffer_duration_ms)
                + m_rebuffer_duration_ms / greatest(m_playing_time_ms, m_rebuffer_duration_ms))),
    NULL) AS playback_score";

pub(super) const QUALITY_SAMPLES: &str = ", quality_samples AS (
    SELECT *, m_startup_time_ms AS startup_time_ms,
        m_rebuffer_duration_ms AS rebuffer_duration_ms,
        ifNull(m_startup_outcome,
            if(m_startup_time_ms IS NOT NULL OR m_playing_time_ms > 0, 'started', 'unknown')) AS startup_outcome,
        m_playing_time_ms IS NOT NULL
            AND m_rebuffer_duration_ms IS NOT NULL
            AND (m_playing_time_ms > 0 OR m_rebuffer_duration_ms > 0) AS has_buffering_sample
    FROM view_metrics
)";

/// Values recorded on a view's first start, available to filters and callers:
/// the context, the subtitle language and playback rate, then the
/// integrator's user ID, video title and custom dimensions.
pub(super) const START_COLUMNS: &[&str] = &[
    "context_browser_name",
    "context_os_name",
    "context_device_type",
    "ctx_continent",
    "ctx_country",
    "ctx_city",
    "ctx_page_origin",
    "ctx_page_path",
    "e_subtitle_language",
    "d_playback_rate",
    "e_user_id",
    "e_video_title",
    "c_custom",
];
/// First-start values with few distinct values, which rollups break down by.
pub(super) const START_CONTEXT: &[&str] = &[
    "context_browser_name",
    "context_os_name",
    "context_device_type",
    "ctx_continent",
    "ctx_country",
    "ctx_city",
    "ctx_page_origin",
    "ctx_page_path",
    "e_subtitle_language",
    "d_playback_rate",
];
const VIEW_CONTEXT: &[&str] = &[
    "context_browser_name",
    "context_os_name",
    "context_device_type",
    "e_user_id",
    "e_video_title",
];

/// Select each view's earliest retained start and its final or latest cumulative
/// measurement, exactly as local storage does, then apply cohort filters.
#[allow(clippy::too_many_arguments)]
pub(super) fn view_cohort(
    params: &mut Params,
    project: &str,
    through: i64,
    cutoff: i64,
    filters: &Filters,
    cohort: Cohort,
    context: &[&str],
    narrow: &Narrow,
) -> String {
    let project_id = params.text(project);
    let scope = format!(
        "project_id = {project_id} AND ingest_id <= {} AND received_at_ms >= {}",
        params.int(through),
        params.int(cutoff)
    );
    let filtered = filters.clickhouse(params);
    let (candidate_conditions, conditions) = match &narrow.starts {
        None => (filtered.clone(), filtered),
        Some(starts) if narrow.first_start => {
            let both = format!("{filtered} AND {starts}");
            (both.clone(), both)
        }
        Some(starts) => (format!("{filtered} AND {starts}"), filtered),
    };
    // Carry only the first-start values that filters or the caller read: each
    // extra column makes the per-view aggregation noticeably more expensive.
    let starts: Vec<&str> = START_COLUMNS
        .iter()
        .copied()
        .filter(|column| context.contains(column) || conditions.contains(column))
        .collect();
    let start_columns: String = starts
        .iter()
        .enumerate()
        .map(|(index, column)| format!(", first_start.{} AS {column}", index + 1))
        .collect();
    let projections: String = measurement(cohort)
        .iter()
        .enumerate()
        .map(|(index, (name, _))| format!(", measurement.{} AS m_{name}", index + 2))
        .collect();
    let (error_projection, score) = if reads_errors(cohort) {
        (", fatal_events > 0 AS has_fatal_error", PLAYBACK_SCORE)
    } else {
        ("", "")
    };
    let meta_projection = if matches!(cohort, Cohort::Rollup) {
        ", arrayDistinct(arrayFlatten(groupArray(start_hours) OVER (PARTITION BY view_id))) AS hours,
            min(min_received) OVER (PARTITION BY view_id) AS min_received_ms,
            max(max_received) OVER (PARTITION BY view_id) AS max_received_ms"
    } else {
        ""
    };
    let aggregates = identity_aggregates(cohort, &starts);
    let identities = match &narrow.states {
        // Merged states of the given views, with the same results per identity.
        Some(views) => format!(
            "SELECT view_id, video_id, viewer_id, {} FROM view_states
            WHERE project_id = {project_id} AND view_id IN ({views})
            GROUP BY view_id, video_id, viewer_id",
            aggregates
                .iter()
                .map(|(function, _, name)| format!("{function}Merge({name}) AS {name}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        None => {
            // A view qualifies only through a start with matching values, so
            // first narrowing to views with such a start keeps every result.
            let candidates = if filters.is_empty() && narrow.starts.is_none() {
                String::new()
            } else {
                format!(
                    " AND view_id IN (SELECT view_id FROM events WHERE {scope} AND event_name = {STARTED} AND {candidate_conditions})"
                )
            };
            format!(
                "SELECT view_id, video_id, viewer_id, {} FROM events
                WHERE {scope} AND event_name IN ({}){candidates}
                GROUP BY view_id, video_id, viewer_id",
                aggregates
                    .iter()
                    .map(|(function, arguments, name)| format!("{function}({arguments}) AS {name}"))
                    .collect::<Vec<_>>()
                    .join(", "),
                cohort_events(cohort)
            )
        }
    };
    format!(
        "WITH per_identity AS (
        {identities}
    ), started AS (
        SELECT view_id, video_id, viewer_id,
            start_order.2 AS occurred_at_ms, start_order.3 AS start_ingest_id{start_columns},
            ifNull(measurement.1, 0) AS playing_time_ms,
            measurement.1 AS m_playing_time_ms{projections}{error_projection}{meta_projection}
        FROM per_identity WHERE start_count > 0
        ORDER BY view_id, start_order LIMIT 1 BY view_id
    ), view_metrics AS (
        SELECT *{score} FROM started WHERE {conditions}
    )"
    )
}

fn metric_columns(metrics: MetricSet) -> &'static str {
    match metrics {
        MetricSet::Summary => "count(), uniqExact(viewer_id), sum(playing_time_ms)",
        MetricSet::Engagement => {
            "count(), sum(playing_time_ms), countIf(m_reached_end),
            count(m_progress_ratio),
            countIf(m_progress_ratio >= 0.25),
            countIf(m_progress_ratio >= 0.5),
            countIf(m_progress_ratio >= 0.75),
            countIf(m_progress_ratio >= 1),
            count(m_playing_time_ms),
            quantileExactInclusive(0.5)(m_playing_time_ms),
            quantileExactInclusive(0.95)(m_playing_time_ms),
            countIf(m_playing_time_ms < 10000),
            countIf(m_playing_time_ms >= 10000 AND m_playing_time_ms < 30000),
            countIf(m_playing_time_ms >= 30000 AND m_playing_time_ms < 60000),
            countIf(m_playing_time_ms >= 60000 AND m_playing_time_ms < 300000),
            countIf(m_playing_time_ms >= 300000)"
        }
        MetricSet::Quality => {
            "count(), count(startup_time_ms),
            quantileExactInclusive(0.5)(startup_time_ms), quantileExactInclusive(0.95)(startup_time_ms),
            countIf(has_buffering_sample),
            sumIf(playing_time_ms, has_buffering_sample),
            ifNull(sumIf(rebuffer_duration_ms, has_buffering_sample), 0),
            countIf(has_fatal_error),
            countIf(startup_outcome = 'started'),
            countIf(startup_outcome = 'failed_before_start'),
            countIf(startup_outcome = 'ended_before_start'),
            countIf(startup_outcome = 'unknown'),
            if(count(playback_score) = 0, NULL,
                arraySum(arraySort(groupArray(playback_score))) / count(playback_score)),
            count(playback_score)"
        }
    }
}

fn cohort_for(metrics: MetricSet) -> Cohort {
    match metrics {
        MetricSet::Summary => Cohort::Basic,
        MetricSet::Quality => Cohort::Quality,
        MetricSet::Engagement => Cohort::Engagement,
    }
}

fn partials_for(metrics: MetricSet) -> Partials {
    match metrics {
        MetricSet::Summary => Partials::Summary,
        MetricSet::Engagement => Partials::Engagement,
        MetricSet::Quality => Partials::Quality,
    }
}

fn source_for(metrics: MetricSet) -> (&'static str, &'static str) {
    match metrics {
        MetricSet::Quality => (QUALITY_SAMPLES, "quality_samples"),
        _ => ("", "view_metrics"),
    }
}

// Row accessors for JSONCompactEachRow results.
fn int(row: &[Value], index: usize) -> Result<i64, ApiError> {
    row.get(index)
        .and_then(Value::as_i64)
        .ok_or_else(ApiError::internal)
}
fn float(row: &[Value], index: usize) -> Result<f64, ApiError> {
    row.get(index)
        .and_then(Value::as_f64)
        .ok_or_else(ApiError::internal)
}
fn optional_float(row: &[Value], index: usize) -> Result<Option<f64>, ApiError> {
    match row.get(index) {
        Some(Value::Null) => Ok(None),
        Some(value) => value.as_f64().map(Some).ok_or_else(ApiError::internal),
        None => Err(ApiError::internal()),
    }
}
fn optional_text(row: &[Value], index: usize) -> Result<Option<String>, ApiError> {
    match row.get(index) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(value)) => Ok(Some(value.clone())),
        _ => Err(ApiError::internal()),
    }
}
fn text(row: &[Value], index: usize) -> Result<String, ApiError> {
    optional_text(row, index)?.ok_or_else(ApiError::internal)
}
fn boolean(row: &[Value], index: usize) -> Result<bool, ApiError> {
    match row.get(index) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(value) => match value.as_u64() {
            Some(0) => Ok(false),
            Some(1) => Ok(true),
            _ => Err(ApiError::internal()),
        },
        None => Err(ApiError::internal()),
    }
}

#[derive(Default)]
struct ProgressMetrics {
    sample_views: i64,
    milestones: [i64; 4],
}

fn count_ratio(views: i64, denominator: i64) -> Value {
    json!({"views":views,"ratio":(denominator > 0).then(|| views as f64 / denominator as f64)})
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

/// Convert one metric row, in the order of `metric_columns`, into the shared
/// response shape used by local storage.
fn metric_json(row: &[Value], offset: usize, metrics: MetricSet) -> Result<Value, ApiError> {
    let n = |i| int(row, offset + i);
    let f = |i| float(row, offset + i);
    let o = |i| optional_float(row, offset + i);
    match metrics {
        MetricSet::Summary => {
            let playing = f(2)?;
            if !playing.is_finite() {
                return Err(ApiError::internal());
            }
            Ok(json!({"views":n(0)?,"unique_viewers":n(1)?,"total_playing_time_ms":playing}))
        }
        MetricSet::Engagement => {
            let views = n(0)?;
            let playing = f(1)?;
            let reached_end = n(2)?;
            let progress = ProgressMetrics {
                sample_views: n(3)?,
                milestones: [n(4)?, n(5)?, n(6)?, n(7)?],
            };
            let (median, p95) = (o(9)?, o(10)?);
            if !playing.is_finite() || [median, p95].into_iter().flatten().any(|v| !v.is_finite()) {
                return Err(ApiError::internal());
            }
            let names = [
                "under_10_seconds",
                "from_10_to_30_seconds",
                "from_30_to_60_seconds",
                "from_1_to_5_minutes",
                "at_least_5_minutes",
            ];
            let mut distribution = serde_json::Map::new();
            for (index, name) in names.into_iter().enumerate() {
                distribution.insert(name.into(), json!(n(11 + index)?));
            }
            Ok(json!({"views":views,"total_playing_time_ms":playing,
                "average_playing_time_ms":(views > 0).then(|| playing / views as f64),
                "reached_end_views":reached_end,"reached_end_ratio":(views > 0).then(|| reached_end as f64 / views as f64),
                "progress_reached":progress.json(),
                "watch_time_ms":{"sample_views":n(8)?,"median":median,"p95":p95,"distribution":distribution}}))
        }
        MetricSet::Quality => {
            let views = n(0)?;
            let (median, p95) = (o(2)?, o(3)?);
            let playing = f(5)?;
            let buffering = f(6)?;
            if !playing.is_finite()
                || !buffering.is_finite()
                || median.is_some_and(|v| !v.is_finite())
                || p95.is_some_and(|v| !v.is_finite())
            {
                return Err(ApiError::internal());
            }
            let affected = n(7)?;
            let unknown = n(11)?;
            // Scale before summing durations so the ratio stays representable.
            let scale = playing.max(buffering);
            let ratio =
                (scale > 0.0).then(|| (buffering / scale) / (playing / scale + buffering / scale));
            Ok(json!({"views":views,
                "startup_time_ms":{"sample_views":n(1)?,"median":median,"p95":p95},
                "buffering":{"sample_views":n(4)?,"total_playing_time_ms":playing,
                    "total_rebuffer_duration_ms":buffering,"ratio":ratio},
                "fatal_errors":{"affected_views":affected,"ratio":(views > 0).then(|| affected as f64 / views as f64)},
                "startup_outcomes":{"sample_views":views - unknown,
                    "started":count_ratio(n(8)?, views),
                    "failed_before_start":count_ratio(n(9)?, views),
                    "ended_before_start":count_ratio(n(10)?, views),
                    "unknown":count_ratio(unknown, views)},
                "playback_score":score_json(o(12)?, n(13)?)}))
        }
    }
}

fn empty_metrics(metrics: MetricSet) -> Result<Value, ApiError> {
    let width = match metrics {
        MetricSet::Summary => 3,
        MetricSet::Engagement => 16,
        MetricSet::Quality => 14,
    };
    let mut row = vec![json!(0); width];
    match metrics {
        MetricSet::Engagement => {
            row[9] = Value::Null;
            row[10] = Value::Null;
        }
        MetricSet::Quality => {
            row[2] = Value::Null;
            row[3] = Value::Null;
            row[12] = Value::Null;
        }
        MetricSet::Summary => {}
    }
    metric_json(&row, 0, metrics)
}

fn score_json(value: Option<f64>, sample_views: i64) -> Value {
    json!({"value":value,"sample_views":sample_views})
}

fn utc_timestamp(milliseconds: i64) -> String {
    chrono::DateTime::from_timestamp_millis(milliseconds)
        .expect("validated RFC3339 time")
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct VideoPosition {
    video_id: String,
    views: i64,
    total_playing_time_ms: f64,
    unique_viewers: i64,
    score: Option<f64>,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewPosition {
    id: String,
    value: Option<ViewSortValue>,
}
#[derive(serde::Serialize, serde::Deserialize)]
enum ViewSortValue {
    Text(String),
    Number(f64),
}

const VIEW_COLUMNS: &str = "view_id, video_id, viewer_id, occurred_at_ms,
    context_browser_name, context_os_name, context_device_type, playing_time_ms,
    m_startup_time_ms, m_rebuffer_duration_ms,
    ifNull(m_reached_end, 0), ifNull(m_has_ended_event, 0),
    has_fatal_error, playback_score, e_user_id, e_video_title";
/// Columns after `VIEW_COLUMNS`, such as a sort key, start here.
const VIEW_COLUMN_COUNT: usize = 16;

fn view_row(row: &[Value]) -> Result<Value, ApiError> {
    let score = optional_float(row, 13)?;
    Ok(json!({"view_id":text(row, 0)?, "video_id":text(row, 1)?,
        "video_title":optional_text(row, 15)?,
        "viewer_id":optional_text(row, 2)?, "user_id":optional_text(row, 14)?,
        "started_at":utc_timestamp(int(row, 3)?),
        "browser_name":optional_text(row, 4)?, "os_name":optional_text(row, 5)?,
        "device_type":optional_text(row, 6)?, "playing_time_ms":float(row, 7)?,
        "startup_time_ms":optional_float(row, 8)?, "rebuffer_duration_ms":optional_float(row, 9)?,
        "reached_end":boolean(row, 10)?, "has_ended_event":boolean(row, 11)?,
        "has_fatal_error":boolean(row, 12)?,
        "playback_score":score_json(score, i64::from(score.is_some()))}))
}

fn event_columns() -> String {
    format!("ingest_id, {}, otlp_payload", *crate::storage::COLUMN_NAMES)
}

impl Store {
    /// The latest retained event order, which bounds every page of one listing.
    pub(super) async fn watermark(&self, project: &str, cutoff: i64) -> Result<i64, ApiError> {
        let mut params = Params::default();
        let sql = format!(
            "SELECT ingest_id FROM events WHERE project_id = {} AND received_at_ms >= {}
            ORDER BY ingest_id DESC LIMIT 1",
            params.text(project),
            params.int(cutoff)
        );
        let rows = self.client.all(&sql, &params).await?;
        rows.first().map_or(Ok(0), |row| int(row, 0))
    }

    async fn admit_read(
        &self,
        project: &str,
        export: bool,
    ) -> Result<Option<crate::limits::Admission>, ApiError> {
        // Export downloads have their own budget, as with local storage.
        if export {
            Ok(None)
        } else {
            self.admit(project).await.map(Some)
        }
    }

    pub async fn events(&self, project: String, query: Query) -> Result<Value, ApiError> {
        let export = query.export.clone();
        if query.sort_by.is_some() || query.group_by.is_some() {
            return Err(ApiError::bad(
                "sort_by and group_by are not supported by events",
            ));
        }
        // Custom values exist only on view starts; filtering events by them
        // would return starts alone, so event lists reject them.
        if !query.custom.is_empty() {
            return Err(ApiError::bad(
                "Custom filters select views; use a view report instead of events",
            ));
        }
        let order = query.order.unwrap_or_default();
        let kind = match order {
            SortOrder::Asc => "events",
            SortOrder::Desc => "events_desc",
        };
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<i64>::decode(value, kind, &filters, &project))
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|c| c.after < 0 || c.after > c.through)
        {
            return Err(ApiError::bad("Invalid cursor position"));
        }
        let _admission = self.admit_read(&project, export.is_some()).await?;
        let cutoff = self.cutoff(&project)?;
        let through = match &cursor {
            Some(c) => c.through,
            None => self.watermark(&project, cutoff).await?,
        };
        let after = cursor.as_ref().map_or(0, |c| c.after);
        let (lower, upper) = match order {
            SortOrder::Asc => (after, through),
            SortOrder::Desc => (
                0,
                cursor
                    .as_ref()
                    .map_or(through, |c| c.after.saturating_sub(1)),
            ),
        };
        let direction = match order {
            SortOrder::Asc => "ASC",
            SortOrder::Desc => "DESC",
        };
        let mut params = Params::default();
        let conditions = filters.clickhouse(&mut params);
        let sql = format!(
            "SELECT {} FROM events WHERE ({conditions}) AND project_id = {} AND received_at_ms >= {}
            AND ingest_id > {} AND ingest_id <= {} ORDER BY ingest_id {direction} LIMIT {}",
            event_columns(),
            params.text(&project),
            params.int(cutoff),
            params.int(lower),
            params.int(upper),
            limit + 1
        );
        let mut rows = self.client.rows(&sql, &params).await?;
        let mut events = Vec::new();
        let mut last = after;
        let mut next_cursor = None;
        while let Some(row) = rows.next().await? {
            if events.len() == limit {
                next_cursor = Some(
                    Cursor {
                        version: 1,
                        project_id: project.clone(),
                        kind: kind.into(),
                        after: last,
                        through,
                        filters: filters.clone(),
                        rollup: None,
                    }
                    .encode()?,
                );
                break;
            }
            let (id, record) = crate::storage::decode(&row[..])?;
            last = id;
            emit_async(&export, &mut events, record).await?;
        }
        Ok(json!({ "events": events, "next_cursor": next_cursor }))
    }

    async fn breakdown(
        &self,
        project: String,
        query: Query,
        metrics: MetricSet,
    ) -> Result<Value, ApiError> {
        let group_by = query
            .group_by
            .clone()
            .ok_or_else(|| ApiError::bad("group_by is required"))?;
        if query.event_name.is_some() || query.sort_by.is_some() || query.order.is_some() {
            return Err(ApiError::bad("Unsupported breakdown query parameter"));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let dimension = &group_by.0;
        // Cursor kinds match local storage, so cursors keep the same format.
        let kind = format!(
            "breakdown_{}_{}_views_desc",
            dimension.name(),
            metrics.name()
        );
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<GroupPosition>::decode(value, &kind, &filters, &project))
            .transpose()?;
        let work = async {
            let cutoff = self.cutoff(&project)?;
            let mut params = Params::default();
            // Groups are named `group_value`: a custom key's value is an expression.
            let column = "group_value";
            let expression = dimension.expression();
            let (through, plan) = self
                .report_plan(
                    &project,
                    &filters,
                    Some(dimension),
                    cutoff,
                    cursor
                        .as_ref()
                        .map(|cursor| (cursor.through, cursor.rollup)),
                    true,
                    true,
                )
                .await?;
            let run = plan.as_ref().and_then(|plan| plan.run);
            let sql = if let Some(plan) = plan {
                let parts = self.combined(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &filters,
                    &plan,
                    Combined {
                        partials: partials_for(metrics),
                        cohort: cohort_for(metrics),
                        context: &[dimension.start_column()],
                        raw_key: Some(expression),
                        stored_key: Some("value"),
                        dimension: dimension.in_rollups().then(|| dimension.start_column()),
                    },
                )?;
                // Each merged row counts views as `sum(views)`.
                let continuation = match &cursor {
                    None => "1".to_owned(),
                    Some(cursor) => cursor
                        .after
                        .continuation(column, &mut params)
                        .replace("count()", "sum(views)"),
                };
                format!(
                    "{parts} SELECT key AS {column}, {} FROM parts
                    GROUP BY {column} HAVING {continuation}
                    ORDER BY sum(views) DESC, {column} ASC NULLS FIRST LIMIT {}",
                    partials_for(metrics).finals(),
                    limit + 1
                )
            } else {
                let cohort = view_cohort(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &filters,
                    cohort_for(metrics),
                    &[dimension.start_column()],
                    &Narrow::default(),
                );
                let continuation = match &cursor {
                    None => "1".to_owned(),
                    Some(cursor) => cursor.after.continuation(column, &mut params),
                };
                let (samples, source) = source_for(metrics);
                format!(
                    "{cohort}{samples} SELECT {expression} AS {column}, {} FROM {source}
                    GROUP BY {column} HAVING {continuation}
                    ORDER BY count() DESC, {column} ASC NULLS FIRST LIMIT {}",
                    metric_columns(metrics),
                    limit + 1
                )
            };
            let mut rows = self.client.rows(&sql, &params).await?;
            let mut groups = Vec::new();
            let mut last = None;
            let mut next_cursor = None;
            while let Some(row) = rows.next().await? {
                if groups.len() == limit {
                    next_cursor = Some(
                        Cursor {
                            version: 1,
                            project_id: project.clone(),
                            kind: kind.clone(),
                            after: last.expect("nonempty page"),
                            through,
                            filters: filters.clone(),
                            rollup: run,
                        }
                        .encode()?,
                    );
                    break;
                }
                let value = optional_text(&row, 0)?;
                last = Some(GroupPosition {
                    views: int(&row, 1)?,
                    value: value.clone(),
                });
                emit_async(
                    &export,
                    &mut groups,
                    json!({"value":value,"data":metric_json(&row, 1, metrics)?}),
                )
                .await?;
            }
            Ok(json!({"group_by":group_by,"groups":groups,"next_cursor":next_cursor}))
        };
        self.cached(
            "breakdown",
            &project,
            (&query, metrics.name()),
            export.is_some(),
            work,
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
        let work = async {
            let cutoff = self.cutoff(&project)?;
            let mut params = Params::default();
            let cohort = if per_video {
                Cohort::Quality
            } else {
                Cohort::Basic
            };
            // Floating-point sort keys must be identical on every page, which
            // only one aggregation path guarantees; those orders read events.
            let rollups = !(per_video
                && matches!(
                    sort,
                    VideoSort::TotalPlayingTimeMs | VideoSort::PlaybackScore
                ));
            let (through, plan) = self
                .report_plan(
                    &project,
                    &filters,
                    None,
                    cutoff,
                    cursor
                        .as_ref()
                        .map(|cursor| (cursor.through, cursor.rollup)),
                    per_video,
                    rollups,
                )
                .await?;
            let run = plan.as_ref().and_then(|plan| plan.run);
            let partials = if per_video {
                Partials::Videos
            } else {
                Partials::Summary
            };
            let combined = match &plan {
                Some(plan) => Some(self.combined(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &filters,
                    plan,
                    Combined {
                        partials,
                        cohort,
                        context: if per_video { &["e_video_title"] } else { &[] },
                        raw_key: per_video.then(|| "video_id".to_owned()),
                        stored_key: per_video.then_some("video_id"),
                        dimension: None,
                    },
                )?),
                None => None,
            };
            let cohort = match &combined {
                Some(_) => String::new(),
                None => view_cohort(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &filters,
                    cohort,
                    if per_video { &["e_video_title"] } else { &[] },
                    &Narrow::default(),
                ),
            };
            if !per_video {
                let sql = match &combined {
                    Some(parts) => format!("{parts} SELECT {} FROM parts", partials.finals()),
                    None => format!(
                        "{cohort} SELECT {} FROM view_metrics",
                        metric_columns(MetricSet::Summary)
                    ),
                };
                let row = self.client.one(&sql, &params).await?;
                return metric_json(&row, 0, MetricSet::Summary);
            }
            let (direction, comparison) = match order {
                SortOrder::Asc => ("ASC", ">"),
                SortOrder::Desc => ("DESC", "<"),
            };
            let column = sort.column();
            let tie_breaker = if matches!(sort, VideoSort::VideoId) {
                ""
            } else {
                ", video_id ASC"
            };
            let mut continuation = "1".to_owned();
            if let Some(cursor) = &cursor {
                let after = &cursor.after;
                if matches!(sort, VideoSort::VideoId) {
                    continuation =
                        format!("video_id {comparison} {}", params.text(&after.video_id));
                } else if matches!(sort, VideoSort::PlaybackScore) {
                    continuation = match after.score {
                        Some(score) => {
                            let score = params.float(score);
                            format!(
                                "(score {comparison} {score} OR (score = {score} AND video_id > {}) OR score IS NULL)",
                                params.text(&after.video_id)
                            )
                        }
                        None => format!(
                            "(score IS NULL AND video_id > {})",
                            params.text(&after.video_id)
                        ),
                    };
                } else {
                    let metric = match sort {
                        VideoSort::Views => params.int(after.views),
                        VideoSort::TotalPlayingTimeMs => params.float(after.total_playing_time_ms),
                        VideoSort::UniqueViewers => params.int(after.unique_viewers),
                        VideoSort::VideoId | VideoSort::PlaybackScore => unreachable!(),
                    };
                    continuation = format!(
                        "({column} {comparison} {metric} OR ({column} = {metric} AND video_id > {}))",
                        params.text(&after.video_id)
                    );
                }
            }
            // Aggregate the whole filtered cohort before ranking or applying the
            // cursor. Summing sorted values keeps floating-point sort keys
            // identical when the next page recomputes the same cohort.
            let video_metrics = match &combined {
                Some(parts) => format!(
                    "{parts}, video_metrics AS (
                        SELECT key AS video_id, {} FROM parts GROUP BY video_id
                    )",
                    Partials::Videos.finals()
                ),
                None => format!(
                    "{cohort}, video_metrics AS (
                        SELECT video_id, count() AS views, uniqExact(viewer_id) AS unique_viewers,
                            arraySum(arraySort(groupArray(playing_time_ms))) AS total_playing_time_ms,
                            if(count(playback_score) = 0, NULL,
                                arraySum(arraySort(groupArray(playback_score))) / count(playback_score)) AS score,
                            count(playback_score) AS score_samples,
                            argMaxIf(e_video_title, start_ingest_id, e_video_title IS NOT NULL) AS video_title
                        FROM view_metrics GROUP BY video_id
                    )"
                ),
            };
            let sql = format!(
                "{video_metrics}
                SELECT video_id, views, unique_viewers, total_playing_time_ms, score, score_samples, video_title
                FROM video_metrics
                WHERE {continuation} ORDER BY {column} {direction} NULLS LAST{tie_breaker} LIMIT {}",
                limit + 1
            );
            let mut rows = self.client.rows(&sql, &params).await?;
            let mut videos = Vec::new();
            let mut last = None;
            let mut next_cursor = None;
            while let Some(row) = rows.next().await? {
                let position = VideoPosition {
                    video_id: text(&row, 0)?,
                    views: int(&row, 1)?,
                    unique_viewers: int(&row, 2)?,
                    total_playing_time_ms: float(&row, 3)?,
                    score: optional_float(&row, 4)?,
                };
                if !position.total_playing_time_ms.is_finite() {
                    return Err(ApiError::internal());
                }
                if videos.len() == limit {
                    next_cursor = Some(
                        Cursor {
                            version: 1,
                            project_id: project.clone(),
                            kind: kind.clone(),
                            after: last.expect("nonempty video page"),
                            through,
                            filters: filters.clone(),
                            rollup: run,
                        }
                        .encode()?,
                    );
                    break;
                }
                let video = json!({
                    "video_id": position.video_id, "video_title": optional_text(&row, 6)?,
                    "views": position.views,
                    "unique_viewers": position.unique_viewers,
                    "total_playing_time_ms": position.total_playing_time_ms,
                    "playback_score": score_json(position.score, int(&row, 5)?)
                });
                last = Some(position);
                emit_async(&export, &mut videos, video).await?;
            }
            Ok(json!({ "videos": videos, "next_cursor": next_cursor }))
        };
        self.cached(
            if per_video { "videos" } else { "summary" },
            &project,
            (&query, per_video),
            export.is_some(),
            work,
        )
        .await
    }

    async fn totals(
        &self,
        name: &'static str,
        project: String,
        query: Query,
        metrics: MetricSet,
    ) -> Result<Value, ApiError> {
        if query.event_name.is_some()
            || query.sort_by.is_some()
            || query.cursor.is_some()
            || query.limit.is_some()
            || query.order.is_some()
        {
            return Err(ApiError::bad(format!("Unsupported {name} query parameter")));
        }
        let filters = query.filters()?;
        let work = async {
            let cutoff = self.cutoff(&project)?;
            // One query reads one consistent snapshot; no watermark round trip.
            let mut params = Params::default();
            let (through, plan) = self
                .report_plan(&project, &filters, None, cutoff, None, false, true)
                .await?;
            let sql = if let Some(plan) = plan {
                let parts = self.combined(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &filters,
                    &plan,
                    Combined {
                        partials: partials_for(metrics),
                        cohort: cohort_for(metrics),
                        context: &[],
                        raw_key: None,
                        stored_key: None,
                        dimension: None,
                    },
                )?;
                format!(
                    "{parts} SELECT {} FROM parts",
                    partials_for(metrics).finals()
                )
            } else {
                let cohort = view_cohort(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &filters,
                    cohort_for(metrics),
                    &[],
                    &Narrow::default(),
                );
                let (samples, source) = source_for(metrics);
                format!(
                    "{cohort}{samples} SELECT {} FROM {source}",
                    metric_columns(metrics)
                )
            };
            let row = self.client.one(&sql, &params).await?;
            metric_json(&row, 0, metrics)
        };
        self.cached(name, &project, &query, query.export.is_some(), work)
            .await
    }

    pub async fn engagement(&self, project: String, query: Query) -> Result<Value, ApiError> {
        if query.group_by.is_some() {
            return self.breakdown(project, query, MetricSet::Engagement).await;
        }
        self.totals("engagement", project, query, MetricSet::Engagement)
            .await
    }

    pub async fn quality(&self, project: String, query: Query) -> Result<Value, ApiError> {
        if query.group_by.is_some() {
            return self.breakdown(project, query, MetricSet::Quality).await;
        }
        self.totals("quality", project, query, MetricSet::Quality)
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
        let work = async {
            let cutoff = self.cutoff(&project)?;
            // One query reads one consistent snapshot; no watermark round trip.
            let mut params = Params::default();
            let width = window.interval.milliseconds();
            // Buckets start on UTC hours, so each rollup hour falls in one bucket.
            let (through, plan) = self
                .report_plan(&project, &window.filters, None, cutoff, None, false, true)
                .await?;
            let sql = if let Some(plan) = plan {
                let first = params.int(window.first_bucket_ms);
                let size = params.int(width);
                let parts = self.combined(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &window.filters,
                    &plan,
                    Combined {
                        partials: partials_for(metrics),
                        cohort: cohort_for(metrics),
                        context: &[],
                        raw_key: Some(format!("intDiv(occurred_at_ms - {first}, {size})")),
                        stored_key: Some(&format!("intDiv(hour_ms - {first}, {size})")),
                        dimension: None,
                    },
                )?;
                format!(
                    "{parts} SELECT key AS bucket_index, {} FROM parts
                    GROUP BY bucket_index ORDER BY bucket_index",
                    partials_for(metrics).finals()
                )
            } else {
                let cohort = view_cohort(
                    &mut params,
                    &project,
                    through,
                    cutoff,
                    &window.filters,
                    cohort_for(metrics),
                    &[],
                    &Narrow::default(),
                );
                let (samples, source) = source_for(metrics);
                format!(
                    "{cohort}{samples} SELECT intDiv(occurred_at_ms - {}, {}) AS bucket_index,
                    {} FROM {source} GROUP BY bucket_index ORDER BY bucket_index",
                    params.int(window.first_bucket_ms),
                    params.int(width),
                    metric_columns(metrics)
                )
            };
            let mut rows = self.client.rows(&sql, &params).await?;
            let empty = empty_metrics(metrics)?;
            let bucket = |mut data: Value, index: usize| {
                data["bucket_start"] =
                    json!(utc_timestamp(window.first_bucket_ms + index as i64 * width));
                data
            };
            if let Some(sink) = &export {
                let mut index = 0usize;
                while let Some(row) = rows.next().await? {
                    let next = int(&row, 0)? as usize;
                    while index < next {
                        sink.push_async(bucket(empty.clone(), index)).await?;
                        index += 1;
                    }
                    sink.push_async(bucket(metric_json(&row, 1, metrics)?, index))
                        .await?;
                    index += 1;
                }
                while index < window.bucket_count {
                    sink.push_async(bucket(empty.clone(), index)).await?;
                    index += 1;
                }
                return Ok(json!({"buckets":[]}));
            }
            let mut buckets: Vec<_> = (0..window.bucket_count)
                .map(|index| bucket(empty.clone(), index))
                .collect();
            while let Some(row) = rows.next().await? {
                let index = int(&row, 0)? as usize;
                let slot = buckets.get_mut(index).ok_or_else(ApiError::internal)?;
                let start = slot["bucket_start"].clone();
                *slot = metric_json(&row, 1, metrics)?;
                slot["bucket_start"] = start;
            }
            Ok(json!({"kind":metrics,
                "from": utc_timestamp(window.from_ms), "to": utc_timestamp(window.to_ms),
                "interval": window.interval, "timezone": "UTC", "buckets": buckets,
            }))
        };
        self.cached("timeseries", &project, cache_query, export.is_some(), work)
            .await
    }

    pub async fn renditions(&self, project: String, query: Query) -> Result<Value, ApiError> {
        reject_ranking_parameters(&query, "Unsupported rendition query parameter")?;
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let work = async {
            let cutoff = self.cutoff(&project)?;
            let through = self.watermark(&project, cutoff).await?;
            let mut params = Params::default();
            let cohort = view_cohort(
                &mut params,
                &project,
                through,
                cutoff,
                &filters,
                Cohort::Renditions,
                &[],
                &Narrow::default(),
            );
            let entries = expanded_entries(
                "rendition_playing_time",
                &format!(
                    "{} AS r_width, {} AS r_height,
                    if(JSONType(entry, 'codec') = 'String', JSONExtractString(entry, 'codec'), NULL) AS r_codec,
                    {} AS r_bitrate_bps, {} AS r_frame_rate, {} AS r_playing_time_ms",
                    json_number("entry", "width"),
                    json_number("entry", "height"),
                    json_number("entry", "bitrate_bps"),
                    json_number("entry", "frame_rate"),
                    json_number("entry", "playing_time_ms"),
                ),
            );
            let sql = format!(
                "{cohort}, rendition_entries AS ({entries}), measured_renditions AS (
                    SELECT * FROM rendition_entries WHERE r_playing_time_ms > 0
                ), ranked_renditions AS (
                    SELECT r_width, r_height, r_codec, r_bitrate_bps, r_frame_rate,
                        uniqExact(view_id) AS r_views,
                        arraySum(arraySort(groupArray(r_playing_time_ms))) AS r_total
                    FROM measured_renditions GROUP BY r_width, r_height, r_codec, r_bitrate_bps, r_frame_rate
                )"
            );
            let totals = self
                .client
                .one(
                    &format!(
                        "{sql} SELECT
                        (SELECT count() FROM view_metrics), uniqExact(view_id),
                        ifNull(sum(r_playing_time_ms), 0),
                        ifNull(sumIf(r_playing_time_ms, r_bitrate_bps IS NOT NULL), 0),
                        sum(r_bitrate_bps * r_playing_time_ms) / nullIf(sumIf(r_playing_time_ms, r_bitrate_bps IS NOT NULL), 0),
                        (SELECT count() FROM ranked_renditions)
                        FROM measured_renditions"
                    ),
                    &params,
                )
                .await?;
            let (views, samples, total, bitrate_time, average, count) = (
                int(&totals, 0)?,
                int(&totals, 1)?,
                float(&totals, 2)?,
                float(&totals, 3)?,
                optional_float(&totals, 4)?,
                int(&totals, 5)?,
            );
            if !total.is_finite()
                || !bitrate_time.is_finite()
                || average.is_some_and(|n| !n.is_finite())
            {
                return Err(ApiError::internal());
            }
            let mut rows = self
                .client
                .rows(
                    &format!(
                        "{sql} SELECT r_width, r_height, r_codec, r_bitrate_bps, r_frame_rate, r_views, r_total
                        FROM ranked_renditions ORDER BY r_total DESC,
                            r_width ASC NULLS FIRST, r_height ASC NULLS FIRST, r_codec ASC NULLS FIRST,
                            r_bitrate_bps ASC NULLS FIRST, r_frame_rate ASC NULLS FIRST LIMIT {limit}"
                    ),
                    &params,
                )
                .await?;
            let mut renditions = Vec::new();
            while let Some(row) = rows.next().await? {
                let time = float(&row, 6)?;
                emit_async(
                    &export,
                    &mut renditions,
                    json!({"width":optional_float(&row, 0)?, "height":optional_float(&row, 1)?,
                        "codec":optional_text(&row, 2)?, "bitrate_bps":optional_float(&row, 3)?,
                        "frame_rate":optional_float(&row, 4)?, "views":int(&row, 5)?,
                        "playing_time_ms":time, "playing_time_ratio": time / total}),
                )
                .await?;
            }
            Ok(
                json!({"views":views, "sample_views":samples, "total_playing_time_ms":total,
                "bitrate_sample_playing_time_ms":bitrate_time, "average_bitrate_bps":average,
                "total_renditions":count, "renditions":renditions}),
            )
        };
        self.cached("renditions", &project, &query, export.is_some(), work)
            .await
    }

    /// Watch time per subtitle language, `null` meaning subtitles off.
    pub async fn subtitles(&self, project: String, query: Query) -> Result<Value, ApiError> {
        self.state_times(project, query, &SUBTITLES).await
    }

    /// Watch time per playback rate.
    pub async fn playback_rates(&self, project: String, query: Query) -> Result<Value, ApiError> {
        self.state_times(project, query, &PLAYBACK_RATES).await
    }

    /// Expand each view's selected cumulative per-state playing time once, like
    /// renditions, and rank states by playing time.
    async fn state_times(
        &self,
        project: String,
        query: Query,
        report: &'static StateTimes,
    ) -> Result<Value, ApiError> {
        reject_ranking_parameters(&query, report.unsupported)?;
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let work = async {
            let cutoff = self.cutoff(&project)?;
            let through = self.watermark(&project, cutoff).await?;
            let mut params = Params::default();
            let cohort = view_cohort(
                &mut params,
                &project,
                through,
                cutoff,
                &filters,
                report.cohort,
                &[],
                &Narrow::default(),
            );
            let entries = expanded_entries(
                report.field,
                &format!(
                    "{} AS s_value, {} AS s_playing_time_ms",
                    report.value,
                    json_number("entry", "playing_time_ms")
                ),
            );
            let sql = format!(
                "{cohort}, state_entries AS ({entries}), measured_states AS (
                    SELECT * FROM state_entries WHERE s_playing_time_ms > 0
                ), ranked_states AS (
                    SELECT s_value, uniqExact(view_id) AS s_views,
                        arraySum(arraySort(groupArray(s_playing_time_ms))) AS s_total
                    FROM measured_states GROUP BY s_value
                )"
            );
            let totals = self
                .client
                .one(
                    &format!(
                        "{sql} SELECT (SELECT count() FROM view_metrics), uniqExact(view_id),
                        ifNull(sum(s_playing_time_ms), 0), (SELECT count() FROM ranked_states)
                        FROM measured_states"
                    ),
                    &params,
                )
                .await?;
            let (views, samples, total, count) = (
                int(&totals, 0)?,
                int(&totals, 1)?,
                float(&totals, 2)?,
                int(&totals, 3)?,
            );
            if !total.is_finite() {
                return Err(ApiError::internal());
            }
            let mut rows = self
                .client
                .rows(
                    &format!(
                        "{sql} SELECT s_value, s_views, s_total FROM ranked_states
                        ORDER BY s_total DESC, s_value ASC NULLS FIRST LIMIT {limit}"
                    ),
                    &params,
                )
                .await?;
            let mut states = Vec::new();
            while let Some(row) = rows.next().await? {
                let value = if report.numeric {
                    json!(optional_float(&row, 0)?)
                } else {
                    json!(optional_text(&row, 0)?)
                };
                let time = float(&row, 2)?;
                emit_async(
                    &export,
                    &mut states,
                    json!({report.key:value, "views":int(&row, 1)?,
                        "playing_time_ms":time, "playing_time_ratio": time / total}),
                )
                .await?;
            }
            Ok(
                json!({"views":views, "sample_views":samples, "total_playing_time_ms":total,
                report.total:count, report.list:states}),
            )
        };
        self.cached(report.cache, &project, &query, export.is_some(), work)
            .await
    }

    /// Count each attempt once per media-position bucket, independent of replays.
    pub async fn retention(&self, project: String, query: Query) -> Result<Value, ApiError> {
        crate::store::validate_retention_query(&query)?;
        let export = query.export.clone();
        let filters = query.filters()?;
        let work = async {
            let cutoff = self.cutoff(&project)?;
            let through = self.watermark(&project, cutoff).await?;
            let mut params = Params::default();
            let cohort = view_cohort(
                &mut params,
                &project,
                through,
                cutoff,
                &filters,
                Cohort::Retention,
                &[],
                &Narrow::default(),
            );
            let sql = format!(
                "{cohort}, measured AS (
                    SELECT * FROM view_metrics
                    WHERE m_media_duration_seconds > 0
                        AND isFinite(m_media_duration_seconds)
                        AND m_played_ranges IS NOT NULL
                )"
            );
            let totals = self
                .client
                .one(
                    &format!("{sql} SELECT (SELECT count() FROM view_metrics), count(), max(m_media_duration_seconds) FROM measured"),
                    &params,
                )
                .await?;
            let (views, samples) = (int(&totals, 0)?, int(&totals, 1)?);
            let Some(duration) = optional_float(&totals, 2)? else {
                return Ok(
                    json!({"views":views,"sample_views":0,"duration_seconds":null,"interval_seconds":null,"buckets":[]}),
                );
            };
            // At most 200 buckets, with one-second resolution for short videos.
            let interval = (duration / 200.0).ceil().max(1.0);
            let count = (duration / interval).ceil() as i64;
            let step = params.float(interval);
            let limit = params.float(duration);
            let buckets = params.float(count as f64);
            // Enumerate only the buckets near each range, then apply the exact
            // overlap test used by local storage to those candidates.
            let mut rows = self
                .client
                .rows(
                    &format!(
                        "{sql}, ranges AS (
                            SELECT view_id, start_seconds,
                                if(isNull(end_value), m_media_duration_seconds, least(end_value, m_media_duration_seconds)) AS end_seconds
                            FROM (
                                SELECT view_id, m_media_duration_seconds, {} AS start_seconds, {} AS end_value
                                FROM measured ARRAY JOIN JSONExtractArrayRaw(ifNull(m_played_ranges, '[]')) AS r
                            ) WHERE start_seconds IS NOT NULL
                        ), overlaps AS (
                            SELECT view_id, i FROM ranges
                            ARRAY JOIN range(
                                toUInt64(greatest(0, floor(start_seconds / {step}) - 1)),
                                toUInt64(greatest(0, least({buckets}, ceil(end_seconds / {step}) + 2)))) AS i
                            WHERE start_seconds < least((i + 1) * {step}, {limit}) AND end_seconds > i * {step}
                        )
                        SELECT i, uniqExact(view_id) FROM overlaps GROUP BY i ORDER BY i",
                        json_number("r", "start_seconds"),
                        json_number("r", "end_seconds"),
                    ),
                    &params,
                )
                .await?;
            let mut watched = vec![0_i64; count as usize];
            while let Some(row) = rows.next().await? {
                let index = int(&row, 0)? as usize;
                *watched.get_mut(index).ok_or_else(ApiError::internal)? = int(&row, 1)?;
            }
            let mut output = Vec::new();
            for (index, views) in watched.into_iter().enumerate() {
                let start = index as f64 * interval;
                let end = ((index + 1) as f64 * interval).min(duration);
                emit_async(
                    &export,
                    &mut output,
                    json!({"start_seconds":start, "end_seconds":end,
                        "views":views,"ratio":views as f64 / samples as f64}),
                )
                .await?;
            }
            Ok(
                json!({"views":views,"sample_views":samples,"duration_seconds":duration,
                "interval_seconds":interval,"buckets":output}),
            )
        };
        self.cached("retention", &project, &query, export.is_some(), work)
            .await
    }

    pub async fn views(&self, project: String, input: ViewsQuery) -> Result<Value, ApiError> {
        let query = input.query;
        validate_exploration(&query)?;
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let order = query.order.unwrap_or(SortOrder::Desc);
        let sort = input.sort_by;
        let kind = format!(
            "views_{}_{}_{:?}",
            sort.column(),
            order.as_str(),
            input.has_fatal_error
        );
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| Cursor::<ViewPosition>::decode(c, &kind, &filters, &project))
            .transpose()?;
        if cursor.as_ref().is_some_and(|c| match &c.after.value {
            Some(ViewSortValue::Text(value)) => !sort.is_text() || value.len() > 16384,
            Some(ViewSortValue::Number(value)) => sort.is_text() || !value.is_finite(),
            None => false,
        }) {
            return Err(ApiError::bad("Invalid view cursor position"));
        }
        let _admission = self.admit_read(&project, export.is_some()).await?;
        let cutoff = self.cutoff(&project)?;
        let through = match &cursor {
            Some(c) => c.through,
            None => self.watermark(&project, cutoff).await?,
        };
        let mut params = Params::default();
        let cohort = view_cohort(
            &mut params,
            &project,
            through,
            cutoff,
            &filters,
            Cohort::Complete,
            VIEW_CONTEXT,
            &Narrow::default(),
        );
        let column = sort.column();
        let (direction, comparison) = match order {
            SortOrder::Asc => ("ASC", ">"),
            SortOrder::Desc => ("DESC", "<"),
        };
        let mut conditions = Vec::new();
        if let Some(c) = &cursor {
            let id = params.text(&c.after.id);
            match &c.after.value {
                Some(value) => {
                    let value = match value {
                        ViewSortValue::Text(text) => params.text(text),
                        ViewSortValue::Number(number) => params.float(*number),
                    };
                    conditions.push(format!(
                        "({column} {comparison} {value} OR ({column} = {value} AND view_id {comparison} {id}) OR {column} IS NULL)"
                    ));
                }
                None => {
                    conditions.push(format!("({column} IS NULL AND view_id {comparison} {id})"))
                }
            }
        }
        if let Some(fatal) = input.has_fatal_error {
            conditions.push(format!("has_fatal_error = {}", params.boolean(fatal)));
        }
        let conditions = if conditions.is_empty() {
            "1".into()
        } else {
            conditions.join(" AND ")
        };
        let projection = if sort.is_text() {
            column.to_owned()
        } else {
            format!("toFloat64({column})")
        };
        let sql = format!(
            "{cohort} SELECT {VIEW_COLUMNS}, {projection} FROM view_metrics WHERE {conditions}
            ORDER BY {column} {direction} NULLS LAST, view_id {direction} LIMIT {}",
            limit + 1
        );
        let mut rows = self.client.rows(&sql, &params).await?;
        let mut views = Vec::new();
        let mut next_cursor = None;
        let mut last = None;
        while let Some(row) = rows.next().await? {
            if views.len() == limit {
                next_cursor = Some(
                    Cursor {
                        version: 1,
                        project_id: project.clone(),
                        kind: kind.clone(),
                        after: last.expect("nonempty page"),
                        through,
                        filters: filters.clone(),
                        rollup: None,
                    }
                    .encode()?,
                );
                break;
            }
            let value = if sort.is_text() {
                optional_text(&row, VIEW_COLUMN_COUNT)?.map(ViewSortValue::Text)
            } else {
                optional_float(&row, VIEW_COLUMN_COUNT)?.map(ViewSortValue::Number)
            };
            let view = view_row(&row)?;
            last = Some(ViewPosition {
                id: text(&row, 0)?,
                value,
            });
            emit_async(&export, &mut views, view).await?;
        }
        Ok(json!({"views":views,"next_cursor":next_cursor}))
    }

    pub async fn view(
        &self,
        project: String,
        view_id: String,
        mut query: Query,
    ) -> Result<Value, ApiError> {
        // A detail page accepts only timeline pagination. Its identity comes from the path.
        if query.from.is_some()
            || query.to.is_some()
            || query.video_id.is_some()
            || query.viewer_id.is_some()
            || query.user_id.is_some()
            || query.video_title.is_some()
            || query.view_id.is_some()
            || query.browser_name.is_some()
            || query.os_name.is_some()
            || query.device_type.is_some()
            || query.continent.is_some()
            || query.country.is_some()
            || query.city.is_some()
            || query.page_origin.is_some()
            || query.page_path.is_some()
            || query.subtitle_language.is_some()
            || query.playback_rate.is_some()
            || !query.custom.is_empty()
            || query.order.is_some()
        {
            return Err(ApiError::bad("View detail accepts only limit and cursor"));
        }
        validate_exploration(&query)?;
        query.view_id = Some(view_id);
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| Cursor::<i64>::decode(c, "view_events", &filters, &project))
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|c| c.after < 0 || c.after > c.through)
        {
            return Err(ApiError::bad("Invalid cursor position"));
        }
        let _admission = self.admit(&project).await?;
        let cutoff = self.cutoff(&project)?;
        let through = match &cursor {
            Some(c) => c.through,
            None => self.watermark(&project, cutoff).await?,
        };
        let mut params = Params::default();
        let cohort = view_cohort(
            &mut params,
            &project,
            through,
            cutoff,
            &filters,
            Cohort::Complete,
            VIEW_CONTEXT,
            &Narrow::default(),
        );
        let view = self
            .client
            .all(
                &format!("{cohort} SELECT {VIEW_COLUMNS} FROM view_metrics"),
                &params,
            )
            .await?
            .first()
            .map(|row| view_row(row))
            .transpose()?
            .ok_or_else(|| {
                ApiError::new(
                    axum::http::StatusCode::NOT_FOUND,
                    "View has no retained start".into(),
                )
            })?;
        let after = cursor.as_ref().map_or(0, |c| c.after);
        let mut params = Params::default();
        let conditions = filters.clickhouse(&mut params);
        let sql = format!(
            "SELECT {} FROM events WHERE ({conditions}) AND project_id = {} AND received_at_ms >= {}
            AND ingest_id > {} AND ingest_id <= {} ORDER BY ingest_id ASC LIMIT {}",
            event_columns(),
            params.text(&project),
            params.int(cutoff),
            params.int(after),
            params.int(through),
            limit + 1
        );
        let mut rows = self.client.rows(&sql, &params).await?;
        let mut events = Vec::new();
        let mut last = after;
        let mut next_cursor = None;
        while let Some(row) = rows.next().await? {
            if events.len() == limit {
                next_cursor = Some(
                    Cursor {
                        version: 1,
                        project_id: project.clone(),
                        kind: "view_events".into(),
                        after: last,
                        through,
                        filters: filters.clone(),
                        rollup: None,
                    }
                    .encode()?,
                );
                break;
            }
            let (id, record) = crate::storage::decode(&row[..])?;
            last = id;
            events.push(record);
        }
        Ok(json!({"view":view,"events":events,"next_cursor":next_cursor}))
    }

    pub async fn dimension(
        &self,
        project: String,
        dimension: Dimension,
        query: Query,
    ) -> Result<Value, ApiError> {
        validate_exploration(&query)?;
        if query.order.is_some() {
            return Err(ApiError::bad("Dimension values sort by views descending"));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let kind = format!("dimension_{}_views_desc", dimension.name());
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| Cursor::<GroupPosition>::decode(c, &kind, &filters, &project))
            .transpose()?;
        let work = async {
            let cutoff = self.cutoff(&project)?;
            let through = match &cursor {
                Some(c) => c.through,
                None => self.watermark(&project, cutoff).await?,
            };
            let mut params = Params::default();
            let cohort = view_cohort(
                &mut params,
                &project,
                through,
                cutoff,
                &filters,
                Cohort::Basic,
                &[dimension.start_column()],
                &Narrow::default(),
            );
            let column = "dimension_value";
            let expression = dimension.expression();
            let continuation = match &cursor {
                None => "1".into(),
                Some(c) => c.after.continuation(column, &mut params),
            };
            let sql = format!(
                "{cohort} SELECT {expression} AS {column}, count() FROM view_metrics GROUP BY {column} HAVING {continuation}
                ORDER BY count() DESC, {column} ASC NULLS FIRST LIMIT {}",
                limit + 1
            );
            let mut rows = self.client.rows(&sql, &params).await?;
            let mut values = Vec::new();
            let mut last = None;
            let mut next_cursor = None;
            while let Some(row) = rows.next().await? {
                if values.len() == limit {
                    next_cursor = Some(
                        Cursor {
                            version: 1,
                            project_id: project.clone(),
                            kind: kind.clone(),
                            after: last.expect("nonempty page"),
                            through,
                            filters: filters.clone(),
                            rollup: None,
                        }
                        .encode()?,
                    );
                    break;
                }
                let (value, views) = (optional_text(&row, 0)?, int(&row, 1)?);
                last = Some(GroupPosition {
                    views,
                    value: value.clone(),
                });
                emit_async(&export, &mut values, json!({"value":value,"views":views})).await?;
            }
            Ok(json!({"dimension":dimension,"values":values,"next_cursor":next_cursor}))
        };
        self.cached(
            "dimension",
            &project,
            (&query, dimension.name()),
            export.is_some(),
            work,
        )
        .await
    }

    pub async fn errors(&self, project: String, query: Query) -> Result<Value, ApiError> {
        validate_exploration(&query)?;
        if query.order.is_some() {
            return Err(ApiError::bad(
                "Error groups sort by affected views descending",
            ));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| {
                Cursor::<(i64, String, String)>::decode(c, "errors_views_desc", &filters, &project)
            })
            .transpose()?;
        let work = async {
            let cutoff = self.cutoff(&project)?;
            let through = match &cursor {
                Some(c) => c.through,
                None => self.watermark(&project, cutoff).await?,
            };
            let mut params = Params::default();
            let cohort = view_cohort(
                &mut params,
                &project,
                through,
                cutoff,
                &filters,
                Cohort::Basic,
                &[],
                &Narrow::default(),
            );
            let scope = format!(
                "project_id = {} AND ingest_id <= {} AND received_at_ms >= {}",
                params.text(&project),
                params.int(through),
                params.int(cutoff)
            );
            let continuation = match &cursor {
                None => "1".to_owned(),
                Some(c) => {
                    let views = params.int(c.after.0);
                    let code = params.text(&c.after.1);
                    let fatal = params.text(&c.after.2);
                    format!(
                        "(affected_views < {views} OR (affected_views = {views} AND (code > {code} OR (code = {code} AND fatal > {fatal}))))"
                    )
                }
            };
            // Group errors of views in the cohort by their JSON-encoded code.
            let sql = format!(
                "{cohort}, error_groups AS (
                    SELECT if(JSONType(event_remainder, 'error_code') IN ('String', 'Int64', 'UInt64', 'Double'),
                            JSONExtractRaw(event_remainder, 'error_code'), 'null') AS code,
                        ifNull(toString(e_error_fatal), 'null') AS fatal,
                        uniqExact(event_id) AS error_events, uniqExact(view_id) AS affected_views
                    FROM events
                    WHERE {scope} AND event_name = {ERROR}
                        AND (view_id, video_id, viewer_id) IN (SELECT view_id, video_id, viewer_id FROM view_metrics)
                    GROUP BY code, fatal)
                SELECT code, fatal, error_events, affected_views FROM error_groups WHERE {continuation}
                ORDER BY affected_views DESC, code ASC, fatal ASC LIMIT {}
                SETTINGS transform_null_in = 1",
                limit + 1
            );
            let mut rows = self.client.rows(&sql, &params).await?;
            let mut errors = Vec::new();
            let mut last = None;
            let mut next_cursor = None;
            while let Some(row) = rows.next().await? {
                if errors.len() == limit {
                    next_cursor = Some(
                        Cursor {
                            version: 1,
                            project_id: project.clone(),
                            kind: "errors_views_desc".into(),
                            after: last.expect("nonempty page"),
                            through,
                            filters: filters.clone(),
                            rollup: None,
                        }
                        .encode()?,
                    );
                    break;
                }
                let (code, fatal, count, views) =
                    (text(&row, 0)?, text(&row, 1)?, int(&row, 2)?, int(&row, 3)?);
                let code_value: Value =
                    serde_json::from_str(&code).map_err(|_| ApiError::internal())?;
                let fatal_value: Value =
                    serde_json::from_str(&fatal).map_err(|_| ApiError::internal())?;
                last = Some((views, code, fatal));
                emit_async(
                    &export,
                    &mut errors,
                    json!({"error_code":code_value,"fatal":fatal_value,"error_events":count,"affected_views":views}),
                )
                .await?;
            }
            Ok(json!({"errors":errors,"next_cursor":next_cursor}))
        };
        self.cached("errors", &project, &query, export.is_some(), work)
            .await
    }
}

/// A report of watch time per playback state, read from each view's cumulative
/// per-state array.
struct StateTimes {
    cohort: Cohort,
    /// The cumulative array the cohort measures.
    field: &'static str,
    /// The state of one array `entry`, as SQL.
    value: &'static str,
    /// Whether the state is a number rather than text.
    numeric: bool,
    /// Response names: row key, row list and distinct state count.
    key: &'static str,
    list: &'static str,
    total: &'static str,
    cache: &'static str,
    unsupported: &'static str,
}

/// A language tag, or `NULL` when subtitles are off.
const SUBTITLES: StateTimes = StateTimes {
    cohort: Cohort::Subtitles,
    field: "subtitle_playing_time",
    value: "if(JSONType(entry, 'language') = 'String', JSONExtractString(entry, 'language'), NULL)",
    numeric: false,
    key: "language",
    list: "languages",
    total: "total_languages",
    cache: "subtitles",
    unsupported: "Unsupported subtitle query parameter",
};

/// A rate rounded like the `playback_rate` dimension.
const PLAYBACK_RATES: StateTimes = StateTimes {
    cohort: Cohort::PlaybackRates,
    field: "playback_rate_playing_time",
    value: "if(JSONType(entry, 'rate') IN ('Int64', 'UInt64', 'Double'), round(JSONExtractFloat(entry, 'rate'), 2), NULL)",
    numeric: true,
    key: "rate",
    list: "rates",
    total: "total_rates",
    cache: "playback_rates",
    unsupported: "Unsupported playback rate query parameter",
};

fn validate_exploration(query: &Query) -> Result<(), ApiError> {
    if query.event_name.is_some() || query.group_by.is_some() || query.sort_by.is_some() {
        return Err(ApiError::bad("Unsupported exploration query parameter"));
    }
    Ok(())
}
