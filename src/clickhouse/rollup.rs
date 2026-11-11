//! Hourly report rollups, read when reports may lag stored events.
//!
//! Reports measure each view once, from its first start and its final or latest
//! measurement. Computing that from events on every request costs CPU in
//! proportion to the views in range. Instead, a materialized view keeps each
//! view's aggregation state as events are inserted (`view_states`), and a
//! periodic refresh turns the states of views that changed into per-view
//! results (`rollup_views`) and mergeable partial aggregates per hour of first
//! start, video and first-start context value (`rollup`).
//!
//! With `clickhouse.rollup_max_lag_seconds` above zero, reports read every hour
//! as of the last completed refresh: whole hours from `rollup`, partial hours
//! from `rollup_views`, and hours with expired events from events, so expired
//! events stay hidden. Without it, reports read events and no refresh runs.
//!
//! Any analytics server can refresh. A run claims the next version with the
//! same deduplicated insert as the project registry, recomputes the hours whose
//! views received events since the previous completed run, and then records its
//! completion. Rows carry the run version as their generation; reads use the
//! newest generation of each hour up to their run, so an interrupted run never
//! hides data and later pages read the same run as their first page.
use super::{
    Params, Store,
    reports::{Cohort, Narrow, START_COLUMNS, START_CONTEXT, view_cohort},
};
use crate::{
    api::ApiError,
    query::{Dimension, Filters},
};
use serde_json::{Value, json};

pub(super) const HOUR_MS: i64 = 3_600_000;
/// Time after ClickHouse stamps a row before a refresh relies on it being
/// visible. Parts are written within milliseconds of the stamp.
const SETTLE_MS: i64 = 10_000;
/// A claimed run that has not completed after this many intervals, and at
/// least a minute, is taken over: its server stopped or failed. A slower run
/// that is still working only duplicates work; results stay correct.
const CLAIM_TIMEOUT_INTERVALS: i64 = 4;
const MIN_CLAIM_TIMEOUT_MS: i64 = 60_000;
/// Superseded generations are kept for this many runs, so reads that chose
/// them before a newer generation was written can finish.
const KEEP_RUNS: i64 = 20;
/// Hours recomputed per query during a refresh.
const HOURS_PER_QUERY: usize = 24;

/// The UTC hour containing a millisecond timestamp, also before 1970.
pub(super) fn hour_of(column: &str) -> String {
    format!("({column} - (({column} % {HOUR_MS}) + {HOUR_MS}) % {HOUR_MS})")
}

/// Partial aggregates stored per rollup row: name, column type, and the
/// expression over per-view rows that produces it. Reads compute the same
/// expressions for unsettled hours and merge both.
pub(super) const PARTIALS: &[(&str, &str, &str)] = &[
    ("views", "UInt64", "count()"),
    (
        "viewers",
        "AggregateFunction(uniqExact, Nullable(String))",
        "uniqExactState(viewer_id)",
    ),
    ("playing_sum", "Float64", "sum(playing_time_ms)"),
    ("reached_end", "UInt64", "countIf(m_reached_end)"),
    ("progress_n", "UInt64", "count(m_progress_ratio)"),
    ("p25", "UInt64", "countIf(m_progress_ratio >= 0.25)"),
    ("p50", "UInt64", "countIf(m_progress_ratio >= 0.5)"),
    ("p75", "UInt64", "countIf(m_progress_ratio >= 0.75)"),
    ("p100", "UInt64", "countIf(m_progress_ratio >= 1)"),
    ("playing_n", "UInt64", "count(m_playing_time_ms)"),
    (
        "playing_q",
        "AggregateFunction(quantilesExactInclusive(0.5, 0.95), Nullable(Float64))",
        "quantilesExactInclusiveState(0.5, 0.95)(m_playing_time_ms)",
    ),
    ("b10", "UInt64", "countIf(m_playing_time_ms < 10000)"),
    (
        "b30",
        "UInt64",
        "countIf(m_playing_time_ms >= 10000 AND m_playing_time_ms < 30000)",
    ),
    (
        "b60",
        "UInt64",
        "countIf(m_playing_time_ms >= 30000 AND m_playing_time_ms < 60000)",
    ),
    (
        "b300",
        "UInt64",
        "countIf(m_playing_time_ms >= 60000 AND m_playing_time_ms < 300000)",
    ),
    ("bmax", "UInt64", "countIf(m_playing_time_ms >= 300000)"),
    ("startup_n", "UInt64", "count(startup_time_ms)"),
    (
        "startup_q",
        "AggregateFunction(quantilesExactInclusive(0.5, 0.95), Nullable(Float64))",
        "quantilesExactInclusiveState(0.5, 0.95)(startup_time_ms)",
    ),
    ("buffering_n", "UInt64", "countIf(has_buffering_sample)"),
    (
        "buffering_playing",
        "Float64",
        "sumIf(playing_time_ms, has_buffering_sample)",
    ),
    (
        "rebuffer_sum",
        "Float64",
        "ifNull(sumIf(rebuffer_duration_ms, has_buffering_sample), 0)",
    ),
    ("fatal_n", "UInt64", "countIf(has_fatal_error)"),
    (
        "out_started",
        "UInt64",
        "countIf(startup_outcome = 'started')",
    ),
    (
        "out_failed",
        "UInt64",
        "countIf(startup_outcome = 'failed_before_start')",
    ),
    (
        "out_ended_before",
        "UInt64",
        "countIf(startup_outcome = 'ended_before_start')",
    ),
    (
        "out_unknown",
        "UInt64",
        "countIf(startup_outcome = 'unknown')",
    ),
    ("score_sum", "Float64", "ifNull(sum(playback_score), 0)"),
    ("score_n", "UInt64", "count(playback_score)"),
    // The title of the most recently received first start that has one.
    (
        "title",
        "AggregateFunction(argMaxIf, Nullable(String), Int64, UInt8)",
        "argMaxIfState(e_video_title, start_ingest_id, e_video_title IS NOT NULL)",
    ),
];

/// Which partial aggregates a report needs.
#[derive(Clone, Copy)]
pub(super) enum Partials {
    Summary,
    Engagement,
    Quality,
    Videos,
}

impl Partials {
    fn names(self) -> &'static [&'static str] {
        match self {
            Self::Summary => &["views", "viewers", "playing_sum"],
            Self::Engagement => &[
                "views",
                "playing_sum",
                "reached_end",
                "progress_n",
                "p25",
                "p50",
                "p75",
                "p100",
                "playing_n",
                "playing_q",
                "b10",
                "b30",
                "b60",
                "b300",
                "bmax",
            ],
            Self::Quality => &[
                "views",
                "startup_n",
                "startup_q",
                "buffering_n",
                "buffering_playing",
                "rebuffer_sum",
                "fatal_n",
                "out_started",
                "out_failed",
                "out_ended_before",
                "out_unknown",
                "score_sum",
                "score_n",
            ],
            Self::Videos => &[
                "views",
                "viewers",
                "playing_sum",
                "score_sum",
                "score_n",
                "title",
            ],
        }
    }

    /// Expressions that build these partials from per-view rows.
    fn expressions(self) -> String {
        self.names()
            .iter()
            .map(|name| {
                let (_, _, expression) = PARTIALS
                    .iter()
                    .find(|(partial, _, _)| partial == name)
                    .expect("known partial");
                format!("{expression} AS {name}")
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    /// Report columns from merged partials, in the order of the report's
    /// metric columns over raw views.
    pub(super) fn finals(self) -> &'static str {
        match self {
            Self::Summary => "sum(views), uniqExactMerge(viewers), sum(playing_sum)",
            Self::Engagement => {
                "sum(views), sum(playing_sum), sum(reached_end), sum(progress_n),
                sum(p25), sum(p50), sum(p75), sum(p100), sum(playing_n),
                if(sum(playing_n) = 0, NULL, arrayElement(quantilesExactInclusiveMerge(0.5, 0.95)(playing_q), 1)),
                if(sum(playing_n) = 0, NULL, arrayElement(quantilesExactInclusiveMerge(0.5, 0.95)(playing_q), 2)),
                sum(b10), sum(b30), sum(b60), sum(b300), sum(bmax)"
            }
            Self::Quality => {
                "sum(views), sum(startup_n),
                if(sum(startup_n) = 0, NULL, arrayElement(quantilesExactInclusiveMerge(0.5, 0.95)(startup_q), 1)),
                if(sum(startup_n) = 0, NULL, arrayElement(quantilesExactInclusiveMerge(0.5, 0.95)(startup_q), 2)),
                sum(buffering_n), sum(buffering_playing), sum(rebuffer_sum), sum(fatal_n),
                sum(out_started), sum(out_failed), sum(out_ended_before), sum(out_unknown),
                if(sum(score_n) = 0, NULL, sum(score_sum) / sum(score_n)), sum(score_n)"
            }
            Self::Videos => {
                "sum(views) AS views, uniqExactMerge(viewers) AS unique_viewers,
                sum(playing_sum) AS total_playing_time_ms,
                if(sum(score_n) = 0, NULL, sum(score_sum) / sum(score_n)) AS score,
                sum(score_n) AS score_samples,
                argMaxIfMerge(title) AS video_title"
            }
        }
    }
}

/// Per-view rows kept by refreshes, with the columns the partials read.
pub(super) const VIEW_COLUMNS: &[(&str, &str)] = &[
    ("project_id", "LowCardinality(String)"),
    ("generation", "Int64"),
    ("view_id", "String"),
    ("video_id", "String"),
    ("viewer_id", "Nullable(String)"),
    ("hour_ms", "Int64"),
    ("occurred_at_ms", "Int64"),
    ("start_ingest_id", "Int64"),
    ("hours", "Array(Int64)"),
    ("min_received_ms", "Int64"),
    ("max_received_ms", "Int64"),
    ("context_browser_name", "LowCardinality(Nullable(String))"),
    ("context_os_name", "LowCardinality(Nullable(String))"),
    ("context_device_type", "LowCardinality(Nullable(String))"),
    ("ctx_continent", "Nullable(String)"),
    ("ctx_country", "Nullable(String)"),
    ("ctx_city", "Nullable(String)"),
    ("ctx_page_origin", "Nullable(String)"),
    ("ctx_page_path", "Nullable(String)"),
    ("e_subtitle_language", "LowCardinality(Nullable(String))"),
    ("d_playback_rate", "LowCardinality(Nullable(String))"),
    ("e_user_id", "Nullable(String)"),
    ("e_video_title", "Nullable(String)"),
    ("c_custom", "Map(String, String)"),
    ("playing_time_ms", "Float64"),
    ("m_playing_time_ms", "Nullable(Float64)"),
    ("m_startup_time_ms", "Nullable(Float64)"),
    ("m_rebuffer_duration_ms", "Nullable(Float64)"),
    ("m_startup_outcome", "Nullable(String)"),
    ("m_reached_end", "Nullable(UInt8)"),
    ("m_progress_ratio", "Nullable(Float64)"),
    ("has_fatal_error", "UInt8"),
    ("playback_score", "Nullable(Float64)"),
    ("startup_time_ms", "Nullable(Float64)"),
    ("rebuffer_duration_ms", "Nullable(Float64)"),
    ("startup_outcome", "String"),
    ("has_buffering_sample", "UInt8"),
];

/// Hours a report reads from rollups, and the date ranges it reads from events.
pub(super) struct Plan {
    /// Whole hours read from rollup rows, with the generation of each.
    pub(super) hours: Vec<i64>,
    generations: Vec<i64>,
    /// Partly covered hours read from per-view rows (lagged reads only).
    partial: Vec<(i64, i64)>,
    raw: Vec<(i64, i64)>,
    from: i64,
    to: i64,
    /// Every settled hour reads per-view rows with the full filters, for
    /// values hourly rollups don't hold (user IDs and custom dimensions).
    views: bool,
    /// The refresh run a lagged read uses; page cursors keep it.
    pub(super) run: Option<i64>,
}

impl Plan {
    /// Whether the report needs events at all.
    pub(super) fn reads_events(&self) -> bool {
        !self.raw.is_empty()
    }

    /// The condition on a view's first start for the hours read from events,
    /// or `None` when rollups cover the whole range.
    pub(super) fn raw_starts(&self, params: &mut Params) -> Option<String> {
        if self.raw.is_empty() {
            return None;
        }
        let ranges = self
            .raw
            .iter()
            .map(|(start, end)| {
                let mut bounds = Vec::new();
                if *start > i64::MIN {
                    bounds.push(format!("occurred_at_ms >= {}", params.int(*start)));
                }
                if *end < i64::MAX {
                    bounds.push(format!("occurred_at_ms < {}", params.int(*end)));
                }
                if bounds.is_empty() {
                    "1".to_owned()
                } else {
                    format!("({})", bounds.join(" AND "))
                }
            })
            .collect::<Vec<_>>();
        Some(format!("({})", ranges.join(" OR ")))
    }

    /// The condition on rows of the planned hours and generations.
    fn generations(params: &mut Params, hours: &[i64], generations: &[i64]) -> String {
        let first = *hours.first().expect("planned hours");
        let last = *hours.last().expect("planned hours");
        format!(
            "hour_ms >= {} AND hour_ms <= {} AND has(arrayZip({}, {}), (hour_ms, generation))",
            params.int(first),
            params.int(last),
            params.ints(hours),
            params.ints(generations)
        )
    }
}

/// The date range of a report rollups can answer and whether it reads per-view
/// rows only, or `None` when its filters need events. `group` is the breakdown
/// dimension, if any.
fn scope_range(filters: &Filters, group: Option<&Dimension>) -> Option<((i64, i64), bool)> {
    let range =
        |from: Option<i64>, to: Option<i64>| (from.unwrap_or(i64::MIN), to.unwrap_or(i64::MAX));
    if filters.needs_views() || group.is_some_and(|group| !group.in_rollups()) {
        let (from, to) = filters.views_range()?;
        return Some((range(from, to), true));
    }
    let scope = filters.rollup_scope()?;
    // Rollup rows hold one context value each: a breakdown can only be
    // filtered by its own column.
    if let (Some((column, _)), Some(group)) = (&scope.dimension, group)
        && *column != group.start_column()
    {
        return None;
    }
    Some((range(scope.from_ms, scope.to_ms), false))
}

/// One report's combined query: rollup rows for whole hours, per-view rows for
/// partial hours and events for hours with expired events, merged as `parts`. `raw_key` and `stored_key` group
/// both sides the same way; `None` for totals.
pub(super) struct Combined<'a> {
    pub partials: Partials,
    pub cohort: Cohort,
    pub context: &'a [&'a str],
    pub raw_key: Option<String>,
    pub stored_key: Option<&'a str>,
    /// A first-start context column whose rollup rows to read, or all views.
    pub dimension: Option<&'static str>,
}

impl Store {
    pub(super) fn rollups_enabled(&self) -> bool {
        self.rollup_reads.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The snapshot a report reads (its event order watermark) and its rollup
    /// plan: the last refresh, or the refresh `cursor`'s first page read.
    /// Without a plan the report reads events. `paged` reports need a
    /// watermark for their cursors when they read events.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn report_plan(
        &self,
        project: &str,
        filters: &Filters,
        group: Option<&Dimension>,
        cutoff: i64,
        cursor: Option<(i64, Option<i64>)>,
        paged: bool,
        rollups: bool,
    ) -> Result<(i64, Option<Plan>), ApiError> {
        let lagged = match cursor {
            _ if !rollups => None,
            None => {
                self.lagged_plan(project, filters, group, cutoff, None)
                    .await?
            }
            Some((_, Some(run))) => {
                self.lagged_plan(project, filters, group, cutoff, Some(run))
                    .await?
            }
            Some((_, None)) => None,
        };
        let watermark = |needed: bool| async move {
            match cursor {
                Some((through, _)) => Ok(through),
                None if needed => self.watermark(project, cutoff).await,
                None => Ok(i64::MAX),
            }
        };
        match lagged {
            Some(plan) => Ok((watermark(paged && plan.reads_events()).await?, Some(plan))),
            None => Ok((watermark(paged).await?, None)),
        }
    }

    fn lag_enabled(&self) -> bool {
        self.rollups_enabled() && !self.rollup_max_lag.is_zero()
    }

    /// A read of every hour as of one completed refresh, up to
    /// `rollup_max_lag_seconds` old: hours with no rollup row had no views
    /// then. `run` pins the refresh a previous page used. `None` when lagged
    /// reads are off, the filters need events, or the last refresh is too old.
    pub(super) async fn lagged_plan(
        &self,
        project: &str,
        filters: &Filters,
        group: Option<&Dimension>,
        cutoff: i64,
        run: Option<i64>,
    ) -> Result<Option<Plan>, ApiError> {
        if !self.lag_enabled() {
            return Ok(None);
        }
        let Some(((from, to), views)) = scope_range(filters, group) else {
            return Ok(None);
        };
        let mut params = Params::default();
        let project_id = params.text(project);
        // A pinned run stays readable while its generations are kept.
        let pinned = match run {
            Some(run) => format!(
                "AND version = {} AND version > (SELECT max(version) FROM rollup_runs WHERE completed = 1) - {KEEP_RUNS}",
                params.int(run)
            ),
            None => String::new(),
        };
        let first = from.div_euclid(HOUR_MS).saturating_mul(HOUR_MS);
        let sql = format!(
            "WITH run AS (
                SELECT version, through_ms FROM rollup_runs WHERE completed = 1 {pinned}
                ORDER BY version DESC LIMIT 1)
            SELECT (SELECT version FROM run), (SELECT through_ms FROM run),
                toUnixTimestamp64Milli(now64(3)),
                (SELECT groupArray((hour_ms, latest, low)) FROM (
                    SELECT hour_ms, max(generation) AS latest,
                        argMax(min_received_ms, generation) AS low
                    FROM rollup_hours
                    WHERE project_id = {project_id} AND hour_ms >= {first} AND hour_ms < {to}
                        AND generation <= (SELECT version FROM run)
                    GROUP BY hour_ms ORDER BY hour_ms))",
            first = params.int(first),
            to = params.int(to),
        );
        let row = self.client.one(&sql, &params).await?;
        let int = |value: Option<&Value>| value.and_then(Value::as_i64);
        let (Some(version), Some(through), Some(now)) =
            (int(row.first()), int(row.get(1)), int(row.get(2)))
        else {
            return Ok(None);
        };
        if run.is_none() && now - through > self.rollup_max_lag.as_millis() as i64 {
            return Ok(None);
        }
        let mut plan = Plan {
            hours: Vec::new(),
            generations: Vec::new(),
            partial: Vec::new(),
            raw: Vec::new(),
            from,
            to,
            views,
            run: Some(version),
        };
        for hour in row.get(3).and_then(Value::as_array).into_iter().flatten() {
            let (Some(hour), Some(generation), Some(low)) =
                (int(hour.get(0)), int(hour.get(1)), int(hour.get(2)))
            else {
                return Err(ApiError::internal());
            };
            let end = hour.saturating_add(HOUR_MS);
            if low < cutoff {
                // Expired events must be hidden: read this hour from events.
                plan.raw.push((hour.max(from), end.min(to)));
            } else if hour >= from && end <= to && !views {
                plan.hours.push(hour);
                plan.generations.push(generation);
            } else {
                plan.partial.push((hour, generation));
            }
        }
        Ok(Some(plan))
    }

    /// The `WITH ... parts AS (...)` part of a combined report query.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn combined(
        &self,
        params: &mut Params,
        project: &str,
        through: i64,
        cutoff: i64,
        filters: &Filters,
        plan: &Plan,
        query: Combined<'_>,
    ) -> Result<String, ApiError> {
        // Per-view plans have no hourly rollup rows, so they need no rollup scope.
        let scope = if plan.views {
            None
        } else {
            Some(filters.rollup_scope().ok_or_else(ApiError::internal)?)
        };
        let names = query.partials.names().join(", ");
        let raw_key = query
            .raw_key
            .as_ref()
            .map_or(String::new(), |key| format!("{key} AS key, "));
        let group = if query.raw_key.is_some() {
            " GROUP BY key"
        } else {
            ""
        };
        let raw = match plan.raw_starts(params) {
            None => None,
            Some(starts) => {
                let cohort = view_cohort(
                    params,
                    project,
                    through,
                    cutoff,
                    filters,
                    query.cohort,
                    query.context,
                    &Narrow {
                        starts: Some(starts),
                        first_start: true,
                        states: None,
                    },
                );
                let (samples, source) = match query.partials {
                    Partials::Quality => (super::reports::QUALITY_SAMPLES, "quality_samples"),
                    _ => ("", "view_metrics"),
                };
                Some(format!(
                    "{cohort}{samples}, raw_parts AS (SELECT {raw_key}{} FROM {source}{group})",
                    query.partials.expressions()
                ))
            }
        };
        let mut ctes: Vec<String> = Vec::new();
        let mut sources: Vec<&str> = Vec::new();
        if let Some(raw) = raw {
            ctes.push(raw);
            sources.push("raw_parts");
        }
        let project_id = params.text(project);
        let video = scope
            .as_ref()
            .and_then(|scope| scope.video_id.as_ref())
            .map(|video| format!(" AND video_id = {}", params.text(video)));
        if let Some(scope) = scope.as_ref().filter(|_| !plan.hours.is_empty()) {
            let mut stored = vec![
                format!("project_id = {project_id}"),
                Plan::generations(params, &plan.hours, &plan.generations),
            ];
            let dimension = query
                .dimension
                .or(scope.dimension.as_ref().map(|(c, _)| *c));
            match (&scope.dimension, dimension) {
                (Some((column, value)), Some(dimension)) if *column == dimension => {
                    stored.push(format!("dim = {}", params.text(dimension)));
                    stored.push(format!("value = {}", params.text(value)));
                }
                // Plans decline a filter on another column than the breakdown.
                (Some(_), _) => return Err(ApiError::internal()),
                (None, Some(dimension)) => stored.push(format!("dim = {}", params.text(dimension))),
                (None, None) => stored.push("dim = 'all'".into()),
            }
            let stored_key = query
                .stored_key
                .map_or(String::new(), |key| format!("{key} AS key, "));
            ctes.push(format!(
                "stored_parts AS (SELECT {stored_key}{names} FROM rollup WHERE {}{})",
                stored.join(" AND "),
                video.clone().unwrap_or_default()
            ));
            sources.push("stored_parts");
        }
        if !plan.partial.is_empty() {
            // Per-view rows carry the same columns as the cohort's view rows,
            // so the same keys and partials apply, filtered by start time.
            let (hours, generations): (Vec<i64>, Vec<i64>) = plan.partial.iter().copied().unzip();
            let mut conditions = vec![
                format!("project_id = {project_id}"),
                Plan::generations(params, &hours, &generations),
            ];
            if plan.from > i64::MIN {
                conditions.push(format!("occurred_at_ms >= {}", params.int(plan.from)));
            }
            if plan.to < i64::MAX {
                conditions.push(format!("occurred_at_ms < {}", params.int(plan.to)));
            }
            match &scope {
                Some(scope) => {
                    if let Some((column, value)) = &scope.dimension {
                        conditions.push(format!("{column} = {}", params.text(value)));
                    }
                }
                // Per-view rows carry every first-start column the filters
                // can use once viewer, view and event filters are excluded.
                None => conditions.push(filters.clickhouse(params)),
            }
            ctes.push(format!(
                "view_parts AS (SELECT {raw_key}{} FROM rollup_views WHERE {}{}{group})",
                query.partials.expressions(),
                conditions.join(" AND "),
                video.unwrap_or_default()
            ));
            sources.push("view_parts");
        }
        if sources.is_empty() {
            // Nothing had views in range: an empty source with the same columns.
            ctes.push(format!(
                "view_parts AS (SELECT {raw_key}{} FROM rollup_views WHERE 0{group})",
                query.partials.expressions()
            ));
            sources.push("view_parts");
        }
        let parts = sources
            .iter()
            .map(|source| format!("SELECT * FROM {source}"))
            .collect::<Vec<_>>()
            .join(" UNION ALL ");
        // The events part already opens with its own WITH clause.
        let opening = if ctes[0].starts_with("WITH ") {
            ""
        } else {
            "WITH "
        };
        Ok(format!("{opening}{}, parts AS ({parts})", ctes.join(", ")))
    }

    /// Refresh rollups if no other server refreshed recently. Returns the
    /// number of hours recomputed, or `None` when another run is current.
    pub async fn refresh_rollups(&self) -> Result<Option<usize>, ApiError> {
        let gap = self.rollup_interval.as_millis() as i64 * 9 / 10;
        self.refresh_rollups_with(SETTLE_MS, gap).await
    }

    pub(super) async fn refresh_rollups_with(
        &self,
        settle_ms: i64,
        min_gap_ms: i64,
    ) -> Result<Option<usize>, ApiError> {
        let started = std::time::Instant::now();
        let Some(run) = self.claim_run(settle_ms, min_gap_ms).await? else {
            return Ok(None);
        };
        let mut hours = 0;
        for (project, dirty) in self.dirty_hours(&run).await? {
            for chunk in dirty.chunks(HOURS_PER_QUERY) {
                self.recompute(&run, &project, chunk).await?;
                hours += chunk.len();
            }
        }
        self.record_run(&run, true).await?;
        if run.version % KEEP_RUNS == 0 {
            self.collect_generations(run.version - KEEP_RUNS).await?;
        }
        tracing::info!(
            backend = "clickhouse",
            version = run.version,
            hours,
            duration_ms = started.elapsed().as_millis(),
            "rollup_refreshed"
        );
        Ok(Some(hours))
    }

    async fn claim_run(&self, settle_ms: i64, min_gap_ms: i64) -> Result<Option<Run>, ApiError> {
        let none = Params::default();
        let clock = self
            .control
            .one(
                "SELECT toUnixTimestamp64Milli(now64(3)),
                    (SELECT max(through_ms) FROM rollup_runs WHERE completed = 1)",
                &none,
            )
            .await?;
        let read = |row: &[Value], index: usize| {
            row.get(index)
                .and_then(Value::as_i64)
                .ok_or_else(ApiError::internal)
        };
        let now = read(&clock, 0)?;
        let since = read(&clock, 1)?;
        let latest = self
            .control
            .all(
                "SELECT version, completed, toUnixTimestamp64Milli(written_at) FROM rollup_runs
                ORDER BY version DESC, completed DESC LIMIT 1",
                &none,
            )
            .await?;
        let claim_timeout = (self.rollup_interval.as_millis() as i64 * CLAIM_TIMEOUT_INTERVALS)
            .max(MIN_CLAIM_TIMEOUT_MS);
        let mut version = 1;
        if let Some(row) = latest.first() {
            let (last, completed, written) = (read(row, 0)?, read(row, 1)?, read(row, 2)?);
            let age = now - written;
            if (completed == 1 && age < min_gap_ms) || (completed == 0 && age < claim_timeout) {
                return Ok(None);
            }
            version = last + 1;
        }
        let run = Run {
            version,
            since,
            through: now - settle_ms,
            write_id: uuid::Uuid::new_v4().to_string(),
        };
        if run.through <= run.since {
            return Ok(None);
        }
        self.record_run(&run, false).await?;
        let mut params = Params::default();
        let sql = format!(
            "SELECT write_id FROM rollup_runs WHERE version = {} AND completed = 0",
            params.int(version)
        );
        let winner = self.control.all(&sql, &params).await?;
        Ok((winner.len() == 1
            && winner[0].first().and_then(Value::as_str) == Some(run.write_id.as_str()))
        .then_some(run))
    }

    async fn record_run(&self, run: &Run, completed: bool) -> Result<(), ApiError> {
        let row = json!({"version": run.version, "write_id": run.write_id,
            "since_ms": run.since, "through_ms": run.through, "completed": u8::from(completed)});
        // Deduplication keeps one claim per version, as the registry does.
        let token = format!(
            "chunkify-rollup-{}-{}",
            run.version,
            if completed { "done" } else { "claim" }
        );
        self.control
            .insert(
                "rollup_runs",
                serde_json::to_vec(&row).map_err(|_| ApiError::internal())?,
                &[
                    ("async_insert", "0"),
                    ("insert_deduplicate", "1"),
                    ("insert_deduplication_token", &token),
                ],
            )
            .await
    }

    /// Hours, per project, with a view whose events were stored during the run's
    /// window, including the hours of every start of such a view.
    async fn dirty_hours(&self, run: &Run) -> Result<Vec<(String, Vec<i64>)>, ApiError> {
        let mut params = Params::default();
        let window = format!(
            "stored_at_ms > {} AND stored_at_ms <= {}",
            params.int(run.since),
            params.int(run.through)
        );
        let sql = format!(
            "SELECT project_id, arraySort(groupUniqArray(hour)) FROM (
                SELECT project_id, arrayJoin(hours) AS hour FROM rollup_views
                WHERE (project_id, view_id) IN (SELECT project_id, view_id FROM events WHERE {window})
                UNION ALL
                SELECT project_id, {} AS hour FROM events
                WHERE {window} AND event_name = 'chunkify.video.view_started'
            ) GROUP BY project_id ORDER BY project_id",
            hour_of("occurred_at_ms")
        );
        self.client
            .all(&sql, &params)
            .await?
            .into_iter()
            .map(|row| {
                let project = row
                    .first()
                    .and_then(Value::as_str)
                    .ok_or_else(ApiError::internal)?;
                let hours = row
                    .get(1)
                    .and_then(Value::as_array)
                    .ok_or_else(ApiError::internal)?
                    .iter()
                    .map(|hour| hour.as_i64().ok_or_else(ApiError::internal))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((project.to_owned(), hours))
            })
            .collect()
    }

    /// Recompute the views with a start in `hours`, then those hours' rollup
    /// rows and receipt ranges, as of the run's window.
    async fn recompute(&self, run: &Run, project: &str, hours: &[i64]) -> Result<(), ApiError> {
        let mut params = Params::default();
        let list = params.ints(hours);
        let owner = params.text(project);
        // Every view with a start in these hours: views known from earlier runs
        // and views with events stored in this run's window. Their states are
        // read by primary key, so the cost follows views, not events.
        let views = format!(
            "SELECT view_id FROM rollup_views WHERE project_id = {owner} AND hasAny(hours, {list})
            UNION DISTINCT
            SELECT view_id FROM events WHERE project_id = {owner}
                AND stored_at_ms > {} AND stored_at_ms <= {}",
            params.int(run.since),
            params.int(run.through)
        );
        let narrow = Narrow {
            states: Some(views),
            ..Narrow::default()
        };
        let cohort = view_cohort(
            &mut params,
            project,
            i64::MAX,
            i64::MIN,
            &Filters::default(),
            Cohort::Rollup,
            START_COLUMNS,
            &narrow,
        );
        let project_id = params.text(project);
        let generation = params.int(run.version);
        let columns = VIEW_COLUMNS
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(", ");
        let values = VIEW_COLUMNS
            .iter()
            .map(|(name, _)| match *name {
                "project_id" => project_id.clone(),
                "generation" => generation.clone(),
                "hour_ms" => hour_of("occurred_at_ms"),
                name => name.to_owned(),
            })
            .collect::<Vec<_>>()
            .join(", ");
        self.client
            .execute(
                format!(
                    "INSERT INTO rollup_views ({columns}) {cohort}{} SELECT {values} FROM quality_samples",
                    super::reports::QUALITY_SAMPLES
                ),
                &params,
            )
            .await?;
        let mut params = Params::default();
        let list = params.ints(hours);
        let project_id = params.text(project);
        let generation = params.int(run.version);
        let current = format!(
            "project_id = {project_id} AND generation = {generation} AND has({list}, hour_ms)"
        );
        let dimensions = std::iter::once("('all', CAST(NULL AS Nullable(String)))".to_owned())
            .chain(
                START_CONTEXT
                    .iter()
                    .map(|column| format!("('{column}', CAST({column} AS Nullable(String)))")),
            )
            .collect::<Vec<_>>()
            .join(", ");
        let names = PARTIALS
            .iter()
            .map(|(name, _, _)| *name)
            .collect::<Vec<_>>()
            .join(", ");
        let expressions = PARTIALS
            .iter()
            .map(|(_, _, expression)| *expression)
            .collect::<Vec<_>>()
            .join(", ");
        self.client
            .execute(
                format!(
                    "INSERT INTO rollup (project_id, hour_ms, generation, dim, value, video_id, {names})
                    SELECT project_id, hour_ms, generation, dimension.1 AS dim, dimension.2 AS value,
                        video_id, {expressions}
                    FROM rollup_views ARRAY JOIN [{dimensions}] AS dimension
                    WHERE {current}
                    GROUP BY project_id, hour_ms, generation, dim, value, video_id"
                ),
                &params,
            )
            .await?;
        // Receipt range per hour over every view with a start in it, so reads
        // skip hours with expired events. Hours without views get a row too.
        self.client
            .execute(
                format!(
                    "INSERT INTO rollup_hours (project_id, hour_ms, generation, min_received_ms, max_received_ms)
                    SELECT {project_id}, hour, {generation}, min(low), max(high) FROM (
                        SELECT arrayJoin({list}) AS hour,
                            toInt64(9223372036854775807) AS low, toInt64(-9223372036854775808) AS high
                        UNION ALL
                        SELECT arrayJoin(hours) AS hour, min_received_ms AS low, max_received_ms AS high
                        FROM rollup_views WHERE project_id = {project_id} AND generation = {generation}
                    ) WHERE has({list}, hour) GROUP BY hour"
                ),
                &params,
            )
            .await
    }

    /// Delete generations superseded at least `KEEP_RUNS` runs ago.
    async fn collect_generations(&self, before: i64) -> Result<(), ApiError> {
        if before <= 0 {
            return Ok(());
        }
        let mut params = Params::default();
        let before = params.int(before);
        let current_hours = format!(
            "SELECT project_id, hour_ms, max(generation) FROM rollup_hours
            WHERE generation <= {before} GROUP BY project_id, hour_ms"
        );
        for statement in [
            format!(
                "DELETE FROM rollup WHERE generation < {before}
                AND (project_id, hour_ms, generation) NOT IN ({current_hours})"
            ),
            format!(
                "DELETE FROM rollup_hours WHERE generation < {before}
                AND (project_id, hour_ms, generation) NOT IN ({current_hours})"
            ),
            format!(
                "DELETE FROM rollup_views WHERE generation < {before}
                AND (project_id, view_id, generation) NOT IN (
                    SELECT project_id, view_id, max(generation) FROM rollup_views
                    WHERE generation <= {before} GROUP BY project_id, view_id)"
            ),
        ] {
            // Replicated tables reject deletes whose condition reads other
            // rows unless allowed; each replica applies the same generations.
            self.client
                .execute_with(
                    statement,
                    &params,
                    &[("allow_nondeterministic_mutations", "1")],
                )
                .await?;
        }
        Ok(())
    }

    /// Delete rollup rows whose events have all expired. `retained` keeps rows
    /// with a receipt time at or after each project's cutoff.
    pub(super) async fn purge_rollups(
        &self,
        retained: impl Fn(&mut Params, &str) -> Result<String, ApiError>,
    ) -> Result<(), ApiError> {
        let mut params = Params::default();
        let hours = retained(&mut params, "max(max_received_ms)")?;
        let views = retained(&mut params, "max(max_received_ms)")?;
        let states = retained(&mut params, "maxMerge(max_received)")?;
        let expired_hours = format!(
            "SELECT project_id, hour_ms FROM rollup_hours GROUP BY project_id, hour_ms HAVING NOT ({hours})"
        );
        for statement in [
            format!("DELETE FROM rollup WHERE (project_id, hour_ms) IN ({expired_hours})"),
            format!("DELETE FROM rollup_hours WHERE (project_id, hour_ms) IN ({expired_hours})"),
            format!(
                "DELETE FROM rollup_views WHERE (project_id, view_id) IN (
                    SELECT project_id, view_id FROM rollup_views
                    GROUP BY project_id, view_id HAVING NOT ({views}))"
            ),
            format!(
                "DELETE FROM view_states WHERE (project_id, view_id) IN (
                    SELECT project_id, view_id FROM view_states
                    GROUP BY project_id, view_id HAVING NOT ({states}))"
            ),
        ] {
            // Replicated tables reject deletes whose condition reads other
            // rows unless allowed; each replica applies the same generations.
            self.client
                .execute_with(
                    statement,
                    &params,
                    &[("allow_nondeterministic_mutations", "1")],
                )
                .await?;
        }
        Ok(())
    }
}

struct Run {
    version: i64,
    since: i64,
    through: i64,
    write_id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hours_floor_before_and_after_1970() {
        let floor = |ms: i64| ms - ((ms % HOUR_MS) + HOUR_MS) % HOUR_MS;
        assert_eq!(floor(HOUR_MS + 5), HOUR_MS);
        assert_eq!(floor(-5), -HOUR_MS);
        assert_eq!(floor(-HOUR_MS), -HOUR_MS);
        assert_eq!(
            hour_of("t"),
            format!("(t - ((t % {HOUR_MS}) + {HOUR_MS}) % {HOUR_MS})")
        );
    }
}
