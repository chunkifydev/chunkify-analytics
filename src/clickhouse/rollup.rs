//! Hourly report rollups.
//!
//! Reports measure each view once, from its first start and its final or latest
//! measurement. Computing that from raw events on every request costs CPU in
//! proportion to the project's views. Rollups store the result per hour of
//! first start, video and first-start context value, as mergeable partial
//! aggregates, so a report over many hours merges a few rows per hour.
//!
//! Results stay exact. A report reads rollup rows only for whole hours inside
//! its date range that are *settled*: no event of a view starting in that hour
//! was stored after the last completed refresh, the cursor snapshot includes
//! all of them, and none of them has expired. Every other hour is computed
//! from events, as without rollups, and both parts merge in one query.
//!
//! Any analytics server can refresh. A run claims the next version with the
//! same deduplicated insert as the project registry, recomputes the hours whose
//! views received events since the previous completed run, and then records its
//! completion. Rows carry the run version as their generation; reads use the
//! newest generation of each hour, so an interrupted run never hides data.
use super::{
    Params, Store,
    reports::{Cohort, Narrow, START_CONTEXT, view_cohort},
};
use crate::{api::ApiError, query::Filters};
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
const PARTIALS: &[(&str, &str, &str)] = &[
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
            Self::Videos => &["views", "viewers", "playing_sum", "score_sum", "score_n"],
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
                sum(score_n) AS score_samples"
            }
        }
    }
}

/// Per-view rows kept by refreshes, with the columns the partials read.
const VIEW_COLUMNS: &[(&str, &str)] = &[
    ("project_id", "LowCardinality(String)"),
    ("generation", "Int64"),
    ("view_id", "String"),
    ("video_id", "String"),
    ("viewer_id", "Nullable(String)"),
    ("hour_ms", "Int64"),
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

/// Statements that create the rollup tables.
pub(super) fn tables() -> Vec<String> {
    let views = VIEW_COLUMNS
        .iter()
        .map(|(name, kind)| format!("{name} {kind}"))
        .collect::<Vec<_>>()
        .join(", ");
    let partials = PARTIALS
        .iter()
        .map(|(name, kind, _)| format!("{name} {kind}"))
        .collect::<Vec<_>>()
        .join(", ");
    vec![
        "CREATE TABLE IF NOT EXISTS rollup_runs (
            version Int64, write_id String, since_ms Int64, through_ms Int64, completed UInt8,
            written_at DateTime64(3) DEFAULT now64(3)
        ) ENGINE = MergeTree ORDER BY (version, completed)
        SETTINGS non_replicated_deduplication_window = 100000"
            .into(),
        format!(
            "CREATE TABLE IF NOT EXISTS rollup_views ({views},
                INDEX rollup_views_view view_id TYPE bloom_filter GRANULARITY 1)
            ENGINE = MergeTree ORDER BY (project_id, generation, view_id)"
        ),
        "CREATE TABLE IF NOT EXISTS rollup_hours (
            project_id LowCardinality(String), hour_ms Int64, generation Int64,
            min_received_ms Int64, max_received_ms Int64
        ) ENGINE = MergeTree ORDER BY (project_id, hour_ms, generation)"
            .into(),
        format!(
            "CREATE TABLE IF NOT EXISTS rollup (
                project_id LowCardinality(String), dim LowCardinality(String), hour_ms Int64,
                generation Int64, value Nullable(String), video_id String, {partials}
            ) ENGINE = MergeTree ORDER BY (project_id, dim, hour_ms, generation, value, video_id)
            SETTINGS allow_nullable_key = 1"
        ),
    ]
}

/// Hours a report reads from rollups, and the date ranges it reads from events.
pub(super) struct Plan {
    pub(super) hours: Vec<i64>,
    generations: Vec<i64>,
    raw: Vec<(i64, i64)>,
}

impl Plan {
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

    /// The condition on rollup rows of the planned generations.
    fn stored(&self, params: &mut Params) -> String {
        let first = *self.hours.first().expect("planned hours");
        let last = *self.hours.last().expect("planned hours");
        format!(
            "hour_ms >= {} AND hour_ms <= {} AND has(arrayZip({}, {}), (hour_ms, generation))",
            params.int(first),
            params.int(last),
            params.ints(&self.hours),
            params.ints(&self.generations)
        )
    }
}

/// Date ranges not covered by the planned hours, within the report's range.
fn uncovered(from: i64, to: i64, hours: &[i64]) -> Vec<(i64, i64)> {
    let mut ranges = Vec::new();
    let mut start = from;
    for hour in hours {
        if *hour > start {
            ranges.push((start, *hour));
        }
        start = hour + HOUR_MS;
    }
    if start < to {
        ranges.push((start, to));
    }
    ranges
}

/// One report's combined query: per-view rows for unsettled hours and rollup
/// rows for settled hours, merged as `parts`. `raw_key` and `stored_key` group
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

    /// Settled hours this report can read from rollups. `None` when rollups
    /// are off, the filters need events, or no whole hour is settled. `group`
    /// is the breakdown column, if any.
    pub(super) async fn rollup_plan(
        &self,
        project: &str,
        filters: &Filters,
        group: Option<&str>,
        cutoff: i64,
        through: i64,
    ) -> Result<Option<Plan>, ApiError> {
        let Some(scope) = filters.rollup_scope().filter(|_| self.rollups_enabled()) else {
            return Ok(None);
        };
        // Rollup rows hold one context value each: a breakdown can only be
        // filtered by its own column.
        if let (Some((column, _)), Some(group)) = (&scope.dimension, group)
            && *column != group
        {
            return Ok(None);
        }
        let from = scope.from_ms.unwrap_or(i64::MIN);
        let to = scope.to_ms.unwrap_or(i64::MAX);
        // Whole hours inside [from, to).
        let first = if from == i64::MIN {
            i64::MIN
        } else {
            from.div_euclid(HOUR_MS)
                .saturating_add(i64::from(from.rem_euclid(HOUR_MS) != 0))
                .saturating_mul(HOUR_MS)
        };
        let end = if to == i64::MAX {
            i64::MAX
        } else {
            to.div_euclid(HOUR_MS).saturating_mul(HOUR_MS)
        };
        if first >= end {
            return Ok(None);
        }
        let mut params = Params::default();
        let project_id = params.text(project);
        let started_hour = hour_of("occurred_at_ms");
        let sql = format!(
            "WITH (SELECT max(through_ms) FROM rollup_runs WHERE completed = 1) AS settled
            SELECT hour_ms, max(generation) FROM rollup_hours
            WHERE project_id = {project_id} AND hour_ms >= {first} AND hour_ms < {end}
            GROUP BY hour_ms
            HAVING argMax(min_received_ms, generation) >= {cutoff}
                AND hour_ms NOT IN (
                    SELECT arrayJoin(hours) FROM rollup_views
                    WHERE project_id = {project_id} AND view_id IN (
                        SELECT view_id FROM events WHERE project_id = {project_id} AND stored_at_ms > settled
                        UNION DISTINCT
                        SELECT view_id FROM events WHERE project_id = {project_id} AND ingest_id > {through})
                    UNION DISTINCT
                    SELECT {started_hour} FROM events WHERE project_id = {project_id}
                        AND event_name = 'chunkify.video.view_started' AND stored_at_ms > settled
                    UNION DISTINCT
                    SELECT {started_hour} FROM events WHERE project_id = {project_id}
                        AND event_name = 'chunkify.video.view_started' AND ingest_id > {through})
            ORDER BY hour_ms",
            first = params.int(first),
            end = params.int(end),
            cutoff = params.int(cutoff),
            through = params.int(through),
        );
        let rows = self.client.all(&sql, &params).await?;
        if rows.is_empty() {
            return Ok(None);
        }
        let mut hours = Vec::with_capacity(rows.len());
        let mut generations = Vec::with_capacity(rows.len());
        for row in rows {
            let read = |index: usize| {
                row.get(index)
                    .and_then(Value::as_i64)
                    .ok_or_else(ApiError::internal)
            };
            hours.push(read(0)?);
            generations.push(read(1)?);
        }
        Ok(Some(Plan {
            raw: uncovered(from, to, &hours),
            hours,
            generations,
        }))
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
        let scope = filters.rollup_scope().ok_or_else(ApiError::internal)?;
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
                        events: None,
                        starts: Some(starts),
                        first_start: true,
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
        let mut stored = vec![
            format!("project_id = {}", params.text(project)),
            plan.stored(params),
        ];
        let dimension = query
            .dimension
            .or(scope.dimension.as_ref().map(|(c, _)| *c));
        match (&scope.dimension, dimension) {
            (Some((column, value)), Some(dimension)) if *column == dimension => {
                stored.push(format!("dim = {}", params.text(dimension)));
                stored.push(format!("value = {}", params.text(value)));
            }
            // `rollup_plan` declines a filter on another column than the breakdown.
            (Some(_), _) => return Err(ApiError::internal()),
            (None, Some(dimension)) => stored.push(format!("dim = {}", params.text(dimension))),
            (None, None) => stored.push("dim = 'all'".into()),
        }
        if let Some(video) = &scope.video_id {
            stored.push(format!("video_id = {}", params.text(video)));
        }
        let stored_key = query
            .stored_key
            .map_or(String::new(), |key| format!("{key} AS key, "));
        let stored = format!(
            "stored_parts AS (SELECT {stored_key}{names} FROM rollup WHERE {})",
            stored.join(" AND ")
        );
        Ok(match raw {
            Some(raw) => format!(
                "{raw}, {stored}, parts AS (SELECT * FROM raw_parts UNION ALL SELECT * FROM stored_parts)"
            ),
            None => format!("WITH {stored}, parts AS (SELECT * FROM stored_parts)"),
        })
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
            .client
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
            .client
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
        let winner = self.client.all(&sql, &params).await?;
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
        self.client
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
        let first = *hours.first().ok_or_else(ApiError::internal)?;
        let last = *hours.last().ok_or_else(ApiError::internal)?;
        let mut params = Params::default();
        let list = params.ints(hours);
        let narrow = Narrow {
            events: Some(format!("stored_at_ms <= {}", params.int(run.through))),
            starts: Some(format!(
                "occurred_at_ms >= {} AND occurred_at_ms < {} AND has({list}, {})",
                params.int(first),
                params.int(last + HOUR_MS),
                hour_of("occurred_at_ms")
            )),
            first_start: false,
        };
        let cohort = view_cohort(
            &mut params,
            project,
            i64::MAX,
            i64::MIN,
            &Filters::default(),
            Cohort::Rollup,
            START_CONTEXT,
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
            self.client.execute(statement, &params).await?;
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
        ] {
            self.client.execute(statement, &params).await?;
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
    fn uncovered_ranges_skip_planned_hours() {
        let h = HOUR_MS;
        assert_eq!(
            uncovered(10, 5 * h + 7, &[h, 2 * h, 4 * h]),
            vec![(10, h), (3 * h, 4 * h), (5 * h, 5 * h + 7)]
        );
        assert_eq!(uncovered(h, 2 * h, &[h]), Vec::<(i64, i64)>::new());
        assert_eq!(
            uncovered(i64::MIN, i64::MAX, &[0]),
            vec![(i64::MIN, 0), (h, i64::MAX)]
        );
    }

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
