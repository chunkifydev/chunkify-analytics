//! Report and ingestion semantics on a disposable ClickHouse database
//! (CHUNKIFY_TEST_CLICKHOUSE_URL, default `http://127.0.0.1:8123`).
use super::{Params, Store, testing};
use crate::{
    api::ApiError,
    event::IncomingEvent,
    query::{BreakdownBy, Dimension, Filters, MetricKind, Query},
};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicI64, Ordering},
};

struct Fixture {
    store: Arc<Store>,
    registry: Arc<crate::projects::Registry>,
    config: crate::config::Config,
    next: AtomicI64,
}

impl Fixture {
    async fn new(projects: &[&str]) -> Self {
        Self::with_settings(projects, "").await
    }

    async fn with_settings(projects: &[&str], extra: &str) -> Self {
        let config = testing::config(&format!(
            "retention.seconds = 0\nrate_limits.read_per_minute = 1000000\nrate_limits.read_burst = 1000000\n{extra}"
        ));
        let mut list = vec![crate::projects::primary_project(
            "test-public",
            "test-secret",
        )];
        for (index, id) in projects.iter().filter(|id| **id != "default").enumerate() {
            let mut project = crate::projects::primary_project(
                &format!("public-{index}"),
                &format!("secret-{index}"),
            );
            project.id = (*id).into();
            project.name = (*id).into();
            list.push(project);
        }
        let keys = crate::config::Keys { projects: list };
        let registry = Arc::new(crate::projects::Registry::new(&keys, &config).unwrap());
        let store = Store::open(&config, registry.clone()).await.unwrap();
        Self {
            store,
            registry,
            config,
            next: AtomicI64::new(1),
        }
    }

    /// Store rows in order, each with the next event order.
    async fn insert(&self, rows: Vec<Value>) {
        let mut body = Vec::new();
        for mut row in rows {
            row["ingest_id"] = json!(self.next.fetch_add(1, Ordering::Relaxed));
            body.extend(serde_json::to_vec(&row).unwrap());
            body.push(b'\n');
        }
        self.store
            .client
            .insert("events", body, &[("async_insert", "0")])
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
    }

    async fn sql(&self, sql: &str) {
        self.store
            .client
            .execute_with(sql, &Params::default(), &[("mutations_sync", "2")])
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        testing::drop_database(&self.config.clickhouse);
    }
}

#[allow(clippy::too_many_arguments)]
fn event(
    id: &str,
    project: &str,
    view: &str,
    video: &str,
    viewer: Option<&str>,
    kind: &str,
    sequence: i64,
    time: i64,
) -> Value {
    json!({"event_id": id, "project_id": project, "view_id": view, "video_id": video,
        "viewer_id": viewer, "event_name": format!("chunkify.video.{kind}"), "sequence": sequence,
        "occurred_at_ms": time, "received_at_ms": time, "event_remainder": "{}", "context_format": 0})
}

fn with(mut row: Value, fields: Value) -> Value {
    for (key, value) in fields.as_object().unwrap() {
        row[key] = value.clone();
    }
    row
}

fn ok(result: Result<Value, ApiError>) -> Value {
    result.unwrap_or_else(|e| panic!("{}", e.message))
}

#[tokio::test]
async fn summaries_preserve_start_identity_ties_null_measurements_and_page_watermarks() {
    let fixture = Fixture::new(&["default", "other"]).await;
    let store = &fixture.store;
    let rows = [
        (
            "a-later",
            "default",
            "a",
            "video-a",
            None,
            "view_started",
            2,
            1000,
            Some("Firefox"),
            None,
        ),
        (
            "a-first",
            "default",
            "a",
            "video-a",
            None,
            "view_started",
            1,
            2000,
            Some("Chrome"),
            None,
        ),
        (
            "a-tie",
            "default",
            "a",
            "video-a",
            None,
            "view_started",
            1,
            2000,
            Some("Safari"),
            None,
        ),
        (
            "a-other-start",
            "default",
            "a",
            "video-z",
            Some("other"),
            "view_started",
            1,
            3000,
            Some("Safari"),
            None,
        ),
        (
            "a-snapshot",
            "default",
            "a",
            "video-a",
            None,
            "view_snapshot",
            90,
            8000,
            Some("Chrome"),
            Some(1000.0),
        ),
        (
            "a-end",
            "default",
            "a",
            "video-a",
            None,
            "view_ended",
            3,
            4000,
            Some("Chrome"),
            Some(200.0),
        ),
        (
            "a-end-tie",
            "default",
            "a",
            "video-a",
            None,
            "view_ended",
            3,
            4000,
            Some("Chrome"),
            None,
        ),
        (
            "a-other-end",
            "default",
            "a",
            "video-z",
            Some("other"),
            "view_ended",
            100,
            9000,
            Some("Safari"),
            Some(99999.0),
        ),
        (
            "b-start",
            "default",
            "b",
            "video-b",
            Some("b-viewer"),
            "view_started",
            1,
            1000,
            Some("Firefox"),
            None,
        ),
        (
            "b-snapshot",
            "default",
            "b",
            "video-b",
            Some("b-viewer"),
            "view_snapshot",
            5,
            5000,
            Some("Firefox"),
            Some(1500.0),
        ),
        (
            "b-tie",
            "default",
            "b",
            "video-b",
            Some("b-viewer"),
            "view_snapshot",
            5,
            5000,
            Some("Firefox"),
            Some(2500.0),
        ),
        (
            "b-lower",
            "default",
            "b",
            "video-b",
            Some("b-viewer"),
            "view_snapshot",
            4,
            6000,
            Some("Firefox"),
            Some(5000.0),
        ),
        (
            "c-start",
            "default",
            "c",
            "video-c",
            Some("c-viewer"),
            "view_started",
            1,
            1000,
            Some("Chrome"),
            None,
        ),
        (
            "orphan",
            "default",
            "orphan",
            "video-orphan",
            Some("orphan"),
            "view_ended",
            1,
            1000,
            Some("Chrome"),
            Some(99999.0),
        ),
        (
            "a-first",
            "other",
            "a",
            "video-a",
            Some("other-viewer"),
            "view_started",
            1,
            1000,
            Some("Chrome"),
            None,
        ),
        (
            "a-end",
            "other",
            "a",
            "video-a",
            Some("other-viewer"),
            "view_ended",
            2,
            2000,
            Some("Chrome"),
            Some(900.0),
        ),
    ];
    fixture
        .insert(
            rows.iter()
                .map(
                    |(id, project, view, video, viewer, kind, seq, time, browser, playing)| {
                        with(
                            event(id, project, view, video, *viewer, kind, *seq, *time),
                            json!({"context_browser_name": browser, "playing_time_ms": playing}),
                        )
                    },
                )
                .collect(),
        )
        .await;
    let totals = |views, viewers, time| json!({"views":views,"unique_viewers":viewers,"total_playing_time_ms":time});
    let filtered = |fields: Value| -> Query { serde_json::from_value(fields).unwrap() };
    for (query, expected) in [
        (Query::default(), totals(3, 2, 2500.0)),
        (
            filtered(json!({"browser_name": "Chrome"})),
            totals(2, 1, 0.0),
        ),
        (
            filtered(json!({"browser_name": "Firefox"})),
            totals(1, 1, 2500.0),
        ),
        (
            filtered(json!({"browser_name": "Safari"})),
            totals(0, 0, 0.0),
        ),
        (filtered(json!({"video_id": "video-z"})), totals(0, 0, 0.0)),
        (filtered(json!({"viewer_id": "other"})), totals(0, 0, 0.0)),
        (
            filtered(json!({"from": "1970-01-01T00:00:02Z", "to": "1970-01-01T00:00:03Z"})),
            totals(1, 0, 0.0),
        ),
        (
            filtered(json!({"from": "1970-01-01T00:00:01Z", "to": "1970-01-01T00:00:02Z"})),
            totals(2, 2, 2500.0),
        ),
    ] {
        assert_eq!(
            ok(store.summary("default".into(), query, false).await),
            expected
        );
    }
    assert_eq!(
        ok(store.summary("other".into(), Query::default(), false).await),
        totals(1, 1, 900.0)
    );
    // Historical records can lack durations. Available playing time alone
    // must not turn an unknown buffering measurement into a zero sample.
    assert_eq!(
        ok(store.quality("default".into(), Query::default()).await),
        json!({
            "playback_score":{"value":null,"sample_views":0},
            "views":3, "startup_time_ms":{"sample_views":0,"median":null,"p95":null},
            "buffering":{"sample_views":0,"total_playing_time_ms":0.0,"total_rebuffer_duration_ms":0.0,"ratio":null},
            "fatal_errors":{"affected_views":0,"ratio":0.0},
            "startup_outcomes":{"sample_views":1,
                "started":{"views":1,"ratio":1.0/3.0},
                "failed_before_start":{"views":0,"ratio":0.0},
                "ended_before_start":{"views":0,"ratio":0.0},
                "unknown":{"views":2,"ratio":2.0/3.0}}
        })
    );
    // A winning historical NULL measurement still contributes a zero
    // attempt to engagement, with the same total and denominator as summary.
    assert_eq!(
        ok(store.engagement("default".into(), Query::default()).await),
        json!({
            "views":3,"total_playing_time_ms":2500.0,"average_playing_time_ms":2500.0/3.0,
            "reached_end_views":0,"reached_end_ratio":0.0,
            "progress_reached":{"sample_views":0,
                "at_25_percent":{"views":0,"ratio":null},"at_50_percent":{"views":0,"ratio":null},
                "at_75_percent":{"views":0,"ratio":null},"at_100_percent":{"views":0,"ratio":null}},
            "watch_time_ms":{"sample_views":1,"median":2500.0,"p95":2500.0,
                "distribution":{"under_10_seconds":1,"from_10_to_30_seconds":0,
                    "from_30_to_60_seconds":0,"from_1_to_5_minutes":0,"at_least_5_minutes":0}}
        })
    );
    let page = |cursor: Option<&Value>| Query {
        limit: Some(1),
        cursor: cursor.and_then(Value::as_str).map(str::to_owned),
        ..Query::default()
    };
    let first = ok(store.summary("default".into(), page(None), true).await);
    assert_eq!(
        first["videos"],
        json!([{"playback_score":{"value":null,"sample_views":0},"video_id":"video-a","views":1,"unique_viewers":0,"total_playing_time_ms":0.0}])
    );
    // An earlier start arriving after page one must not alter its frozen cohort.
    fixture
        .insert(vec![with(
            event(
                "b-new-first",
                "default",
                "b",
                "video-x",
                Some("new-viewer"),
                "view_started",
                0,
                500,
            ),
            json!({"received_at_ms": 10000}),
        )])
        .await;
    let second = ok(store
        .summary("default".into(), page(Some(&first["next_cursor"])), true)
        .await);
    assert_eq!(
        second["videos"],
        json!([{"playback_score":{"value":null,"sample_views":0},"video_id":"video-b","views":1,"unique_viewers":1,"total_playing_time_ms":2500.0}])
    );
    let third = ok(store
        .summary("default".into(), page(Some(&second["next_cursor"])), true)
        .await);
    assert_eq!(
        third["videos"],
        json!([{"playback_score":{"value":null,"sample_views":0},"video_id":"video-c","views":1,"unique_viewers":1,"total_playing_time_ms":0.0}])
    );
    assert!(third["next_cursor"].is_null());
    assert_eq!(
        ok(store
            .summary("default".into(), Query::default(), false)
            .await),
        totals(3, 2, 0.0)
    );
}

#[tokio::test]
async fn playback_scores_agree_across_sessions_videos_quality_and_buckets() {
    let fixture = Fixture::new(&["default"]).await;
    let store = &fixture.store;
    let ids = [
        "perfect",
        "slow",
        "buffer",
        "combined",
        "session",
        "long-startup",
        "heavy-buffer",
        "fatal",
        "missing",
        "pending",
        "exit",
    ];
    let mut rows: Vec<Value> = ids
        .iter()
        .map(|id| {
            with(
                event(
                    &format!("{id}-start"),
                    "default",
                    id,
                    "video",
                    None,
                    "view_started",
                    1,
                    1000,
                ),
                json!({"context_browser_name": "Chrome"}),
            )
        })
        .collect();
    for (id, playing, startup, rebuffer, outcome) in [
        ("perfect", 10000.0, Some(0.0), Some(0.0), "started"),
        ("slow", 10000.0, Some(2000.0), Some(0.0), "started"),
        ("buffer", 9800.0, Some(0.0), Some(200.0), "started"),
        ("combined", 9900.0, Some(2000.0), Some(100.0), "started"),
        (
            "session",
            20719.29999923706,
            Some(4830.200000286102),
            Some(3.40000057220459),
            "started",
        ),
        ("long-startup", 10000.0, Some(10000.0), Some(0.0), "started"),
        ("heavy-buffer", 9000.0, Some(0.0), Some(1000.0), "started"),
        ("missing", 10000.0, Some(100.0), None, "started"),
        ("exit", 0.0, None, Some(0.0), "ended_before_start"),
    ] {
        rows.push(with(
            event(
                &format!("{id}-end"),
                "default",
                id,
                "video",
                None,
                "view_ended",
                3,
                5000,
            ),
            json!({"playing_time_ms": playing, "e_startup_time_ms": startup,
                "e_rebuffer_duration_ms": rebuffer, "e_startup_outcome": outcome}),
        ));
    }
    rows.push(with(
        event(
            "fatal-error",
            "default",
            "fatal",
            "video",
            None,
            "playback_error",
            2,
            2000,
        ),
        json!({"e_error_fatal": true}),
    ));
    rows.push(with(
        event(
            "old-snapshot",
            "default",
            "slow",
            "video",
            None,
            "view_snapshot",
            99,
            9000,
        ),
        json!({"playing_time_ms": 9000.0, "e_startup_time_ms": 0.0, "e_rebuffer_duration_ms": 0.0}),
    ));
    fixture.insert(rows).await;
    let expected = [
        ("perfect", Some(100.0)),
        ("slow", Some(93.52484478226214)),
        ("buffer", Some(86.93582353988059)),
        ("combined", Some(87.20198728264428)),
        ("session", Some(86.68783991421262)),
        ("long-startup", Some(78.40526816831157)),
        ("heavy-buffer", Some(49.658530379140956)),
        ("fatal", Some(0.0)),
        ("missing", None),
        ("pending", None),
        ("exit", None),
    ];
    let page = ok(store
        .views("default".into(), serde_json::from_value(json!({})).unwrap())
        .await);
    for (id, score) in expected {
        let row = page["views"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["view_id"] == id)
            .unwrap();
        let actual = row["playback_score"]["value"].as_f64();
        assert_eq!(actual.is_some(), score.is_some(), "{id}");
        if let Some(value) = score {
            assert!((actual.unwrap() - value).abs() < 1e-9, "{id}: {actual:?}");
        }
        assert_eq!(
            row["playback_score"]["sample_views"],
            i64::from(score.is_some())
        );
        let detail = ok(store
            .view("default".into(), id.into(), Query::default())
            .await);
        assert_eq!(detail["view"]["playback_score"], row["playback_score"]);
    }
    let quality = ok(store.quality("default".into(), Query::default()).await);
    assert_eq!(quality["playback_score"]["sample_views"], 8);
    let mean = expected.iter().filter_map(|(_, score)| *score).sum::<f64>() / 8.0;
    assert!((quality["playback_score"]["value"].as_f64().unwrap() - mean).abs() < 1e-9);
    let videos = ok(store
        .summary("default".into(), Query::default(), true)
        .await);
    assert_eq!(
        videos["videos"][0]["playback_score"],
        quality["playback_score"]
    );
    let groups = ok(store
        .quality(
            "default".into(),
            serde_json::from_value(json!({"group_by":"browser_name"})).unwrap(),
        )
        .await);
    assert_eq!(
        groups["groups"][0]["data"]["playback_score"],
        quality["playback_score"]
    );
    let series = ok(store
        .timeseries(
            "default".into(),
            serde_json::from_value(json!({"kind":"quality",
        "from":"1970-01-01T00:00:00Z","to":"1970-01-03T00:00:00Z","interval":"day"}))
            .unwrap(),
        )
        .await);
    assert_eq!(
        series["buckets"][0]["playback_score"],
        quality["playback_score"]
    );
    assert_eq!(
        series["buckets"][1]["playback_score"],
        json!({"value":null,"sample_views":0})
    );
}

#[tokio::test]
async fn playback_score_preserves_missing_data_and_bounds_extreme_inputs() {
    let fixture = Fixture::new(&["default"]).await;
    let cases = [
        ("10000, 0, 0, 'started', 0", Some(100.0_f64)),
        ("10000, 10000, 0, 'started', 0", Some(78.40526816831157)),
        ("10000, 20000, 0, 'started', 0", Some(68.67198272427282)),
        ("9000, 0, 1000, 'started', 0", Some(49.658530379140956)),
        ("8000, 0, 2000, 'started', 0", Some(24.659696394160644)),
        ("1e308, 0, 1e308, 'started', 0", Some(3.0197383422318502)),
        ("10000, 1e308, 0, 'started', 0", Some(5.901018770673883e-90)),
        (
            "20719.29999923706, 4830.200000286102, 3.40000057220459, 'started', 0",
            Some(86.68783991421262),
        ),
        ("10000, 0, 0, NULL, 0", Some(100.0)),
        ("NULL, NULL, NULL, 'failed_before_start', 0", Some(0.0)),
        ("NULL, NULL, NULL, NULL, 1", Some(0.0)),
        ("10000, 0, 0, 'started', 1", Some(0.0)),
        ("10000, 0, 0, 'failed_before_start', 0", Some(0.0)),
        ("0, 0, 0, 'started', 0", None),
        ("10000, NULL, 0, 'started', 0", None),
        ("10000, 0, NULL, 'started', 0", None),
        ("10000, 0, 0, 'unknown', 0", None),
        ("10000, 0, 0, 'ended_before_start', 0", None),
    ];
    for (values, expected) in cases {
        let row = fixture
            .store
            .client
            .one(
                &format!(
                    "SELECT playback_score FROM (SELECT 1 {} FROM (
                        SELECT CAST(v.1 AS Nullable(Float64)) AS m_playing_time_ms,
                            CAST(v.2 AS Nullable(Float64)) AS m_startup_time_ms,
                            CAST(v.3 AS Nullable(Float64)) AS m_rebuffer_duration_ms,
                            CAST(v.4 AS Nullable(String)) AS m_startup_outcome,
                            CAST(v.5 AS UInt8) AS has_fatal_error
                        FROM (SELECT tuple({values}) AS v)))",
                    super::reports::PLAYBACK_SCORE
                ),
                &Params::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("{values}: {}", e.message));
        let actual = row[0].as_f64();
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

#[tokio::test]
async fn counted_groups_rank_full_cohorts_and_preserve_pagination_snapshots() {
    let fixture = Fixture::new(&["default", "other"]).await;
    let store = &fixture.store;
    let labels = [
        ("z1", "default", Some("Z")),
        ("z2", "default", Some("Z")),
        ("z3", "default", Some("Z")),
        ("z4", "default", Some("Z")),
        ("n1", "default", None),
        ("n2", "default", None),
        ("a1", "default", Some("A")),
        ("a2", "default", Some("A")),
        ("b1", "default", Some("B")),
        ("b2", "default", Some("B")),
        ("c1", "default", Some("C")),
        ("other", "other", Some("A")),
    ];
    let starts = |id: &str, project: &str, label: Option<&str>| {
        with(
            event(id, project, id, "video", Some(id), "view_started", 1, 1000),
            json!({"context_browser_name": label, "context_os_name": label, "context_device_type": label,
                "context_remainder": json!({"continent": label, "country": label, "city": label,
                    "page_origin": label, "page_path": label}).to_string()}),
        )
    };
    fixture
        .insert(
            labels
                .iter()
                .map(|(id, project, label)| starts(id, project, *label))
                .collect(),
        )
        .await;
    let expected = vec![
        json!(["Z", 4]),
        json!([null, 2]),
        json!(["A", 2]),
        json!(["B", 2]),
        json!(["C", 1]),
    ];
    for (group_by, dimension) in [
        (BreakdownBy::BrowserName, Dimension::BrowserName),
        (BreakdownBy::OsName, Dimension::OsName),
        (BreakdownBy::DeviceType, Dimension::DeviceType),
        (BreakdownBy::Continent, Dimension::Continent),
        (BreakdownBy::Country, Dimension::Country),
        (BreakdownBy::City, Dimension::City),
        (BreakdownBy::PageOrigin, Dimension::PageOrigin),
        (BreakdownBy::PagePath, Dimension::PagePath),
    ] {
        for metrics in [
            MetricKind::Summary,
            MetricKind::Quality,
            MetricKind::Engagement,
        ] {
            let mut cursor = None;
            let mut actual = Vec::new();
            loop {
                let query = Query {
                    group_by: Some(group_by),
                    limit: Some(1),
                    cursor,
                    ..Query::default()
                };
                let page = ok(match metrics {
                    MetricKind::Summary => store.summary("default".into(), query, false).await,
                    MetricKind::Quality => store.quality("default".into(), query).await,
                    MetricKind::Engagement => store.engagement("default".into(), query).await,
                });
                actual.extend(
                    page["groups"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|row| json!([row["value"], row["data"]["views"]])),
                );
                cursor = page["next_cursor"].as_str().map(str::to_owned);
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(actual, expected);
        }
        let mut cursor = None;
        let mut actual = Vec::new();
        loop {
            let page = ok(store
                .dimension(
                    "default".into(),
                    dimension,
                    Query {
                        limit: Some(1),
                        cursor,
                        ..Query::default()
                    },
                )
                .await);
            actual.extend(
                page["values"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|row| json!([row["value"], row["views"]])),
            );
            cursor = page["next_cursor"].as_str().map(str::to_owned);
            if cursor.is_none() {
                break;
            }
        }
        assert_eq!(actual, expected);
    }
    let mut errors = Vec::new();
    for (id, project, label) in labels
        .iter()
        .filter(|(_, project, _)| *project == "default")
    {
        errors.push(with(
            event(&format!("{id}-error"), project, id, "video", Some(id), "playback_error", 2, 2000),
            json!({"event_remainder": json!({"error_code": label}).to_string(), "e_error_fatal": false}),
        ));
    }
    for (id, project, _) in labels
        .iter()
        .filter(|(_, project, label)| *project == "default" && *label == Some("A"))
    {
        errors.push(with(
            event(
                &format!("{id}-fatal"),
                project,
                id,
                "video",
                Some(id),
                "playback_error",
                3,
                3000,
            ),
            json!({"event_remainder": "{\"error_code\":\"A\"}", "e_error_fatal": true}),
        ));
    }
    fixture.insert(errors).await;
    let mut cursor = None;
    let mut errors = Vec::new();
    loop {
        let page = ok(store
            .errors(
                "default".into(),
                Query {
                    limit: Some(1),
                    cursor,
                    ..Query::default()
                },
            )
            .await);
        errors.extend(
            page["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| json!([row["error_code"], row["fatal"], row["affected_views"]])),
        );
        cursor = page["next_cursor"].as_str().map(str::to_owned);
        if cursor.is_none() {
            break;
        }
    }
    assert_eq!(
        errors,
        vec![
            json!(["Z", false, 4]),
            json!(["A", false, 2]),
            json!(["A", true, 2]),
            json!(["B", false, 2]),
            json!([null, false, 2]),
            json!(["C", false, 1])
        ]
    );
    let first_page = || Query {
        limit: Some(1),
        ..Query::default()
    };
    let first = ok(store
        .summary(
            "default".into(),
            Query {
                group_by: Some(BreakdownBy::OsName),
                ..first_page()
            },
            false,
        )
        .await);
    let dimensions = ok(store
        .dimension("default".into(), Dimension::OsName, first_page())
        .await);
    let error_page = ok(store.errors("default".into(), first_page()).await);
    let mut late = Vec::new();
    for index in 0..8 {
        let id = format!("late-{index}");
        late.push(with(
            event(
                &id,
                "default",
                &id,
                "video",
                Some(&id),
                "view_started",
                1,
                1000,
            ),
            json!({"received_at_ms": 4000, "context_os_name": "A"}),
        ));
        late.push(with(
            event(&format!("{id}-error"), "default", &id, "video", Some(&id), "playback_error", 2, 2000),
            json!({"received_at_ms": 4000, "event_remainder": "{\"error_code\":\"A\"}", "e_error_fatal": false}),
        ));
    }
    fixture.insert(late).await;
    let rest = |cursor: &Value| Query {
        cursor: cursor.as_str().map(str::to_owned),
        ..Query::default()
    };
    let remaining = ok(store
        .summary(
            "default".into(),
            Query {
                group_by: Some(BreakdownBy::OsName),
                ..rest(&first["next_cursor"])
            },
            false,
        )
        .await);
    assert_eq!(
        remaining["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| json!([r["value"], r["data"]["views"]]))
            .collect::<Vec<_>>(),
        expected[1..]
    );
    let remaining = ok(store
        .dimension(
            "default".into(),
            Dimension::OsName,
            rest(&dimensions["next_cursor"]),
        )
        .await);
    assert_eq!(
        remaining["values"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| json!([r["value"], r["views"]]))
            .collect::<Vec<_>>(),
        expected[1..]
    );
    let remaining = ok(store
        .errors("default".into(), rest(&error_page["next_cursor"]))
        .await);
    assert_eq!(
        remaining["errors"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| json!([r["error_code"], r["fatal"], r["affected_views"]]))
            .collect::<Vec<_>>(),
        errors[1..]
    );
    let refreshed = ok(store
        .summary(
            "default".into(),
            Query {
                group_by: Some(BreakdownBy::OsName),
                ..Query::default()
            },
            false,
        )
        .await);
    assert_eq!(refreshed["groups"][0]["value"], "A");
    assert_eq!(refreshed["groups"][0]["data"]["views"], 10);
}

#[tokio::test]
async fn views_sort_by_start_with_stable_ties_and_watermark() {
    let fixture = Fixture::new(&["default", "other"]).await;
    let store = &fixture.store;
    // UUID order, arrival order and occurrence order differ. All attempts share one viewer.
    fixture
        .insert(
            [
                ("a", "default", 2000),
                ("b", "default", 3000),
                ("z", "default", 1000),
                ("m", "default", 2000),
                ("other", "other", 4000),
            ]
            .iter()
            .map(|(id, project, started)| {
                event(
                    id,
                    project,
                    id,
                    "video",
                    Some("same-viewer"),
                    "view_started",
                    1,
                    *started,
                )
            })
            .collect(),
        )
        .await;
    let ids = |page: &Value| {
        page["views"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["view_id"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    for (order, expected) in [
        (None, vec!["b", "m", "a", "z"]),
        (Some("asc"), vec!["z", "a", "m", "b"]),
        (Some("desc"), vec!["b", "m", "a", "z"]),
    ] {
        let mut query = json!({"limit":2,"viewer_id":"same-viewer","order":order});
        let first = ok(store
            .views(
                "default".into(),
                serde_json::from_value(query.clone()).unwrap(),
            )
            .await);
        assert_eq!(ids(&first), expected[..2]);
        let cursor = first["next_cursor"].clone();
        assert!(cursor.is_string());
        query["cursor"] = cursor;
        let second = ok(store
            .views(
                "default".into(),
                serde_json::from_value(query.clone()).unwrap(),
            )
            .await);
        assert_eq!(ids(&second), expected[2..]);
        assert!(second["next_cursor"].is_null());
        query["viewer_id"] = json!("another-viewer");
        assert!(
            store
                .views("default".into(), serde_json::from_value(query).unwrap())
                .await
                .is_err()
        );
    }
    let first = ok(store
        .views(
            "default".into(),
            serde_json::from_value(json!({"limit":2})).unwrap(),
        )
        .await);
    fixture
        .insert(
            [("late", 1500), ("newest", 4000)]
                .iter()
                .map(|(id, started)| {
                    with(
                        event(
                            id,
                            "default",
                            id,
                            "video",
                            Some("same-viewer"),
                            "view_started",
                            1,
                            *started,
                        ),
                        json!({"received_at_ms": 4000}),
                    )
                })
                .collect(),
        )
        .await;
    let second = ok(store
        .views(
            "default".into(),
            serde_json::from_value(json!({"limit":2,"cursor":first["next_cursor"]})).unwrap(),
        )
        .await);
    assert_eq!(ids(&second), ["a", "z"]);
    assert!(second["next_cursor"].is_null());
    let refreshed = ok(store
        .views("default".into(), serde_json::from_value(json!({})).unwrap())
        .await);
    assert_eq!(ids(&refreshed), ["newest", "b", "m", "a", "late", "z"]);
}

#[tokio::test]
async fn pages_keep_otlp_project_scope_order_and_watermark() {
    let fixture = Fixture::new(&["a", "b"]).await;
    let store = &fixture.store;
    let start = |id: &str, project: &str, sequence, time, otlp: Option<Value>| {
        with(
            event(
                id,
                project,
                "v",
                "video",
                None,
                "view_started",
                sequence,
                time,
            ),
            json!({"otlp_payload": otlp.map(|v| v.to_string())}),
        )
    };
    fixture
        .insert(vec![
            start("shared", "a", 1, 0, Some(json!({"project":"a"}))),
            start("shared", "b", 1, 0, Some(json!({"project":"b"}))),
            start("native", "a", 2, 1, None),
            start(
                "native",
                "b",
                2,
                1,
                Some(json!({"project":"b", "native": true})),
            ),
            start("last", "a", 3, 2, Some(json!({"last":true}))),
        ])
        .await;
    let first = ok(store
        .events(
            "a".into(),
            Query {
                limit: Some(2),
                ..Query::default()
            },
        )
        .await);
    assert_eq!(first["events"].as_array().unwrap().len(), 2);
    assert_eq!(first["events"][0]["event"]["event_id"], "shared");
    assert_eq!(first["events"][0]["otlp"], json!({"project":"a"}));
    assert_eq!(first["events"][1]["event"]["event_id"], "native");
    assert!(first["events"][1].get("otlp").is_none());
    let newest = ok(store
        .events(
            "a".into(),
            serde_json::from_value(json!({"limit": 2, "order": "desc", "video_id": "video"}))
                .unwrap(),
        )
        .await);
    assert_eq!(newest["events"][0]["event"]["event_id"], "last");
    assert_eq!(newest["events"][0]["otlp"], json!({"last":true}));
    assert_eq!(newest["events"][1]["event"]["event_id"], "native");
    assert!(newest["events"][1].get("otlp").is_none());
    let newest_cursor = newest["next_cursor"].as_str().unwrap();
    for (project, order, video) in [
        ("b", "desc", "video"),
        ("a", "asc", "video"),
        ("a", "desc", "other"),
    ] {
        assert!(
            store
                .events(
                    project.into(),
                    serde_json::from_value(
                        json!({"order": order, "video_id": video, "cursor": newest_cursor})
                    )
                    .unwrap()
                )
                .await
                .is_err()
        );
    }
    fixture.insert(vec![start("late", "a", 4, 3, None)]).await;
    let second = ok(store
        .events(
            "a".into(),
            Query {
                limit: Some(2),
                cursor: Some(first["next_cursor"].as_str().unwrap().into()),
                ..Query::default()
            },
        )
        .await);
    assert_eq!(second["events"].as_array().unwrap().len(), 1);
    assert_eq!(second["events"][0]["otlp"], json!({"last":true}));
    assert!(second["next_cursor"].is_null());
    let older = ok(store
        .events(
            "a".into(),
            serde_json::from_value(
                json!({"limit": 1, "order": "desc", "video_id": "video", "cursor": newest_cursor}),
            )
            .unwrap(),
        )
        .await);
    assert_eq!(older["events"].as_array().unwrap().len(), 1);
    assert_eq!(older["events"][0]["event"]["event_id"], "shared");
    assert_eq!(older["events"][0]["otlp"], json!({"project":"a"}));
    assert!(older["next_cursor"].is_null());
    let refreshed = ok(store
        .events(
            "a".into(),
            serde_json::from_value(json!({"order": "desc", "limit": 1})).unwrap(),
        )
        .await);
    assert_eq!(refreshed["events"][0]["event"]["event_id"], "late");
    let filtered = ok(store
        .events(
            "a".into(),
            serde_json::from_value(json!({"order": "desc", "from": "1970-01-01T00:00:00.001Z",
                "to": "1970-01-01T00:00:00.003Z"}))
            .unwrap(),
        )
        .await);
    assert_eq!(filtered["events"].as_array().unwrap().len(), 2);
    assert_eq!(filtered["events"][0]["event"]["event_id"], "last");
    assert_eq!(filtered["events"][1]["event"]["event_id"], "native");
    let other = ok(store
        .events(
            "b".into(),
            Query {
                limit: Some(1),
                ..Query::default()
            },
        )
        .await);
    assert_eq!(other["events"][0]["otlp"], json!({"project":"b"}));
}

#[tokio::test]
async fn renditions_select_one_measurement_per_view_and_weight_known_bitrates() {
    let fixture = Fixture::new(&["default", "other"]).await;
    let store = &fixture.store;
    let rendition = |width, height, bitrate: Option<i64>, time| json!({"width":width,"height":height,"codec":"avc1","bitrate_bps":bitrate,"frame_rate":30,"playing_time_ms":time});
    let a = json!({"rendition_playing_time":[rendition(1280,720,Some(2000000),6000),rendition(640,360,Some(1000000),4000)]});
    let latest = json!({"rendition_playing_time":[rendition(1280,720,Some(2000000),2000),rendition(640,360,None,1000)]});
    let rows = [
        (
            "a-start",
            "default",
            "a",
            "video",
            None,
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "a-snapshot",
            "default",
            "a",
            "video",
            None,
            "view_snapshot",
            90,
            "Chrome",
            latest.clone(),
        ),
        (
            "a-end",
            "default",
            "a",
            "video",
            None,
            "view_ended",
            2,
            "Chrome",
            a.clone(),
        ),
        (
            "a-wrong-identity",
            "default",
            "a",
            "other-video",
            Some("other"),
            "view_ended",
            100,
            "Chrome",
            a.clone(),
        ),
        (
            "b-start",
            "default",
            "b",
            "video",
            None,
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "b-snapshot",
            "default",
            "b",
            "video",
            None,
            "view_snapshot",
            2,
            "Chrome",
            a.clone(),
        ),
        (
            "b-end",
            "default",
            "b",
            "video",
            None,
            "view_ended",
            3,
            "Chrome",
            json!({"rendition_playing_time":null}),
        ),
        (
            "c-start",
            "default",
            "c",
            "video",
            None,
            "view_started",
            1,
            "Firefox",
            json!({}),
        ),
        (
            "c-old",
            "default",
            "c",
            "video",
            None,
            "view_snapshot",
            2,
            "Firefox",
            a.clone(),
        ),
        (
            "c-latest",
            "default",
            "c",
            "video",
            None,
            "view_snapshot",
            3,
            "Firefox",
            latest,
        ),
        (
            "d-start",
            "default",
            "d",
            "video",
            None,
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "orphan",
            "default",
            "orphan",
            "video",
            None,
            "view_ended",
            1,
            "Chrome",
            a.clone(),
        ),
        (
            "foreign-start",
            "other",
            "x",
            "video",
            None,
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "foreign-end",
            "other",
            "x",
            "video",
            None,
            "view_ended",
            2,
            "Chrome",
            a,
        ),
    ];
    fixture
        .insert(
            rows.iter()
                .map(
                    |(id, project, view, video, viewer, kind, seq, browser, payload)| {
                        let time = if *kind == "view_started" { 1000 } else { 2000 };
                        with(
                            event(id, project, view, video, *viewer, kind, *seq, time),
                            json!({"received_at_ms": 2000, "event_remainder": payload.to_string(),
                            "context_browser_name": browser}),
                        )
                    },
                )
                .collect(),
        )
        .await;
    let result = ok(store
        .renditions(
            "default".into(),
            Query {
                limit: Some(1),
                video_id: Some("video".into()),
                from: Some("1970-01-01T00:00:01Z".into()),
                to: Some("1970-01-01T00:00:01.500Z".into()),
                ..Query::default()
            },
        )
        .await);
    assert_eq!(result["views"], 4);
    assert_eq!(result["sample_views"], 2);
    assert_eq!(result["total_renditions"], 3);
    assert_eq!(result["total_playing_time_ms"], 13000.0);
    assert_eq!(result["bitrate_sample_playing_time_ms"], 12000.0);
    assert!(
        (result["average_bitrate_bps"].as_f64().unwrap() - 20000000000.0 / 12000.0).abs() < 0.001
    );
    assert_eq!(result["renditions"].as_array().unwrap().len(), 1);
    assert_eq!(result["renditions"][0]["height"], 720.0);
    assert_eq!(result["renditions"][0]["views"], 2);
    assert!(
        (result["renditions"][0]["playing_time_ratio"]
            .as_f64()
            .unwrap()
            - 8.0 / 13.0)
            .abs()
            < 1e-10
    );
    let chrome = ok(store
        .renditions(
            "default".into(),
            Query {
                browser_name: Some("Chrome".into()),
                ..Query::default()
            },
        )
        .await);
    assert_eq!(chrome["average_bitrate_bps"], 1600000.0);
    assert_eq!(chrome["views"], 3);
    for view in ["b", "d", "missing"] {
        let empty = ok(store
            .renditions(
                "default".into(),
                Query {
                    view_id: Some(view.into()),
                    ..Query::default()
                },
            )
            .await);
        assert_eq!(empty["sample_views"], 0);
        assert_eq!(empty["average_bitrate_bps"], Value::Null);
        assert_eq!(empty["renditions"], json!([]));
    }
    assert!(
        store
            .renditions(
                "default".into(),
                Query {
                    cursor: Some("invalid".into()),
                    ..Query::default()
                }
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn retention_uses_one_measurement_per_identity_and_preserves_unwatched_gaps() {
    let fixture = Fixture::new(&["default", "foreign"]).await;
    let store = &fixture.store;
    let payload = |ranges: Value| json!({"media_duration_seconds":5,"played_ranges":ranges});
    let ranges = json!([{"start_seconds":0,"end_seconds":1},{"start_seconds":3,"end_seconds":4}]);
    let rows = [
        (
            "a-start",
            "default",
            "a",
            "video",
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "a-snapshot",
            "default",
            "a",
            "video",
            "view_snapshot",
            99,
            "Chrome",
            payload(json!([{"start_seconds":0,"end_seconds":5}])),
        ),
        (
            "a-end",
            "default",
            "a",
            "video",
            "view_ended",
            2,
            "Chrome",
            payload(ranges.clone()),
        ),
        (
            "a-duplicate",
            "default",
            "a",
            "video",
            "view_ended",
            2,
            "Chrome",
            payload(ranges.clone()),
        ),
        (
            "a-other-identity",
            "default",
            "a",
            "other",
            "view_ended",
            999,
            "Chrome",
            payload(json!([{"start_seconds":0,"end_seconds":5}])),
        ),
        (
            "b-start",
            "default",
            "b",
            "video",
            "view_started",
            1,
            "Firefox",
            json!({}),
        ),
        (
            "b-snapshot",
            "default",
            "b",
            "video",
            "view_snapshot",
            2,
            "Firefox",
            payload(json!([{"start_seconds":0.5,"end_seconds":2}])),
        ),
        (
            "c-start",
            "default",
            "c",
            "video",
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "c-end",
            "default",
            "c",
            "video",
            "view_ended",
            2,
            "Chrome",
            payload(json!([])),
        ),
        (
            "old-start",
            "default",
            "old",
            "video",
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "old-end",
            "default",
            "old",
            "video",
            "view_ended",
            2,
            "Chrome",
            json!({"media_duration_seconds":5}),
        ),
        (
            "foreign-start",
            "foreign",
            "a",
            "video",
            "view_started",
            1,
            "Chrome",
            json!({}),
        ),
        (
            "foreign-end",
            "foreign",
            "a",
            "video",
            "view_ended",
            2,
            "Chrome",
            payload(ranges.clone()),
        ),
        (
            "orphan",
            "default",
            "orphan",
            "video",
            "view_ended",
            2,
            "Chrome",
            payload(ranges),
        ),
    ];
    fixture
        .insert(
            rows.iter()
                .map(|(id, project, view, video, kind, seq, browser, body)| {
                    let time = if *kind == "view_started" { 1000 } else { 2000 };
                    with(
                        event(id, project, view, video, None, kind, *seq, time),
                        json!({"received_at_ms": 2000, "event_remainder": body.to_string(),
                            "context_browser_name": browser}),
                    )
                })
                .collect(),
        )
        .await;
    let query = || Query {
        video_id: Some("video".into()),
        from: Some("1970-01-01T00:00:01Z".into()),
        to: Some("1970-01-01T00:00:01.500Z".into()),
        ..Query::default()
    };
    let report = ok(store.retention("default".into(), query()).await);
    assert_eq!(report["views"], 4);
    assert_eq!(report["sample_views"], 3);
    assert_eq!(report["duration_seconds"], 5.0);
    assert_eq!(report["interval_seconds"], 1.0);
    let counts: Vec<_> = report["buckets"]
        .as_array()
        .unwrap()
        .iter()
        .map(|b| b["views"].as_i64().unwrap())
        .collect();
    assert_eq!(counts, [2, 1, 0, 1, 0]);
    assert_eq!(report["buckets"][0]["ratio"], 2.0 / 3.0);
    let filtered = ok(store
        .retention(
            "default".into(),
            Query {
                browser_name: Some("Firefox".into()),
                ..query()
            },
        )
        .await);
    assert_eq!(filtered["views"], 1);
    assert_eq!(filtered["buckets"][3]["views"], 0);
    let empty = ok(store
        .retention(
            "default".into(),
            Query {
                view_id: Some("old".into()),
                ..query()
            },
        )
        .await);
    assert_eq!(empty["views"], 1);
    assert_eq!(empty["sample_views"], 0);
    assert_eq!(empty["buckets"], json!([]));
    assert_eq!(empty["duration_seconds"], Value::Null);
    for q in [
        Query::default(),
        Query {
            limit: Some(10),
            ..query()
        },
        Query {
            cursor: Some("x".into()),
            ..query()
        },
    ] {
        assert!(store.retention("default".into(), q).await.is_err());
    }
    // Long media remains bounded and the last bucket ends at the real duration.
    fixture
        .sql(&format!(
            "ALTER TABLE events UPDATE event_remainder = '{}' WHERE event_id = 'b-snapshot'",
            json!({"media_duration_seconds":1001,"played_ranges":[]})
        ))
        .await;
    let long = ok(store.retention("default".into(), query()).await);
    assert_eq!(long["interval_seconds"], 6.0);
    assert_eq!(long["buckets"].as_array().unwrap().len(), 167);
    assert_eq!(
        long["buckets"].as_array().unwrap().last().unwrap()["end_seconds"],
        1001.0
    );
}

#[test]
fn played_ranges_require_ordered_finite_disjoint_intervals() {
    for value in [
        json!([]),
        json!(null),
        json!([{"start_seconds":0,"end_seconds":1},{"start_seconds":2,"end_seconds":3}]),
    ] {
        assert!(crate::event::valid_played_ranges(&value));
    }
    for value in [
        json!({}),
        json!([{"start_seconds":1,"end_seconds":1}]),
        json!([{"start_seconds":-1,"end_seconds":1}]),
        json!([{"start_seconds":0,"end_seconds":2},{"start_seconds":1,"end_seconds":3}]),
        json!([{"start_seconds":2,"end_seconds":3},{"start_seconds":0,"end_seconds":1}]),
        json!([{"start_seconds":"0","end_seconds":1}]),
    ] {
        assert!(!crate::event::valid_played_ranges(&value));
    }
}

#[tokio::test]
async fn ingestion_keeps_first_deliveries_per_project_and_parameterizes_ids() {
    let fixture = Fixture::new(&["a", "b"]).await;
    let store = &fixture.store;
    let record = |id: &str, marker: &str, otlp: bool| IncomingEvent {
        event: json!({"event_id":id,"view_id":"view","video_id":"video","viewer_id":null,
            "event_name":"chunkify.video.view_started","sequence":1,
            "occurred_at":"2026-10-20T00:00:00Z","marker":marker}),
        otlp: otlp.then(|| json!({"marker":marker})),
    };
    let a = fixture.registry.access("a").unwrap();
    let b = fixture.registry.access("b").unwrap();
    let id = "same'); DELETE FROM events; --";
    async fn insert(
        store: &Store,
        project: &crate::projects::Access,
        records: Vec<IncomingEvent>,
    ) -> Result<usize, ApiError> {
        store.insert(project, "test-key", records, None).await
    }
    assert_eq!(
        insert(
            store,
            &a,
            vec![record(id, "first", true), record(id, "duplicate", true)]
        )
        .await
        .unwrap(),
        1
    );
    assert_eq!(
        insert(store, &b, vec![record(id, "other-project", true)])
            .await
            .unwrap(),
        1
    );
    assert_eq!(
        insert(store, &a, vec![record("native", "native", false)])
            .await
            .unwrap(),
        1
    );
    for (project, records, inserted) in [
        (
            &a,
            vec![
                record(id, "replacement", true),
                record("native", "late-otlp", true),
            ],
            0,
        ),
        (&b, vec![record(id, "replacement", true)], 0),
        (&a, vec![record("new", "new", true)], 1),
    ] {
        assert_eq!(insert(store, project, records).await.unwrap(), inserted);
    }
    let listed = |project: &'static str| async move {
        ok(store.events(project.into(), Query::default()).await)["events"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| {
                (
                    row["event"]["event_id"].as_str().unwrap().to_owned(),
                    row["event"]["marker"].as_str().unwrap().to_owned(),
                    row["otlp"]["marker"].as_str().map(str::to_owned),
                    row["ingest_key_id"].as_str().unwrap().to_owned(),
                )
            })
            .collect::<Vec<_>>()
    };
    let owned = |id: &str, marker: &str, otlp: Option<&str>| {
        (
            id.to_owned(),
            marker.to_owned(),
            otlp.map(str::to_owned),
            "test-key".to_owned(),
        )
    };
    assert_eq!(
        listed("a").await,
        [
            owned(id, "first", Some("first")),
            owned("native", "native", None),
            owned("new", "new", Some("new")),
        ]
    );
    assert_eq!(
        listed("b").await,
        [owned(id, "other-project", Some("other-project"))]
    );
    // A failed insert acknowledges and keeps nothing; a retry then succeeds.
    fixture
        .sql("ALTER TABLE events ADD CONSTRAINT block CHECK 0")
        .await;
    assert!(
        insert(store, &a, vec![record("blocked", "blocked", true)])
            .await
            .is_err()
    );
    fixture
        .sql("ALTER TABLE events DROP CONSTRAINT block")
        .await;
    assert_eq!(listed("a").await.len(), 3);
    assert_eq!(
        insert(store, &a, vec![record("blocked", "retry", true)])
            .await
            .unwrap(),
        1
    );
    assert_eq!(listed("a").await.last().unwrap().1, "retry");
}

/// Compare report JSON, allowing floating-point sums to differ in the last
/// bits: rollups add partial sums in another order than one pass over views.
fn assert_same(rollup: &Value, raw: &Value, path: &str) {
    match (rollup, raw) {
        (Value::Number(a), Value::Number(b)) if a.is_f64() || b.is_f64() => {
            let (a, b) = (a.as_f64().unwrap(), b.as_f64().unwrap());
            assert!(
                (a - b).abs() <= 1e-9 * a.abs().max(b.abs()).max(1.0),
                "{path}: {a} != {b}"
            );
        }
        (Value::Array(a), Value::Array(b)) => {
            assert_eq!(a.len(), b.len(), "{path}: {rollup} != {raw}");
            for (index, (a, b)) in a.iter().zip(b).enumerate() {
                assert_same(a, b, &format!("{path}[{index}]"));
            }
        }
        (Value::Object(a), Value::Object(b)) => {
            let keys = |o: &serde_json::Map<String, Value>| o.keys().cloned().collect::<Vec<_>>();
            assert_eq!(keys(a), keys(b), "{path}");
            for (key, a) in a {
                // Cursors encode the same position; sums inside may differ in the last bits.
                if key == "next_cursor" {
                    assert_eq!(a.is_null(), b[key].is_null(), "{path}.{key}");
                    continue;
                }
                assert_same(a, &b[key], &format!("{path}.{key}"));
            }
        }
        _ => assert_eq!(rollup, raw, "{path}"),
    }
}

const HOUR: i64 = 3_600_000;

/// Views over four hours with contexts, two videos, errors, progress, a view
/// with two identities and a view with starts in two hours.
fn rollup_rows(base: i64) -> Vec<Value> {
    let mut rows = Vec::new();
    let browsers = ["Chrome", "Firefox", "Safari"];
    let countries = ["FR", "US"];
    for index in 0..24i64 {
        let view = format!("v{index}");
        let hour = base + (index % 4) * HOUR;
        let start = hour + 60_000 * (index + 1);
        let video = if index % 3 == 0 { "video-b" } else { "video-a" };
        let viewer = format!("viewer-{}", index % 5);
        let context = json!({
            "context_browser_name": browsers[(index % 3) as usize],
            "context_os_name": "Linux", "context_device_type": "desktop",
            "context_remainder": json!({"continent": "EU", "country": countries[(index % 2) as usize],
                "city": format!("city-{}", index % 4), "page_origin": "https://a.example",
                "page_path": "/watch"}).to_string(),
        });
        let event = |kind: &str, sequence: i64, at: i64| {
            event(
                &format!("{view}-{kind}-{sequence}"),
                "default",
                &view,
                video,
                Some(&viewer),
                kind,
                sequence,
                at,
            )
        };
        rows.push(with(event("view_started", 1, start), context.clone()));
        rows.push(with(
            event("view_snapshot", 2, start + 10_000),
            json!({"playing_time_ms": (1000 * (index + 1)) as f64,
                "e_startup_time_ms": (100 * (index % 7)) as f64, "e_rebuffer_duration_ms": 0.0,
                "e_media_duration_seconds": 600.0, "e_furthest_progress_ratio": 0.1}),
        ));
        if index % 2 == 0 {
            rows.push(with(
                event("view_ended", 3, start + 20_000),
                json!({"playing_time_ms": (2000 * (index + 1)) as f64,
                    "e_startup_time_ms": (100 * (index % 7)) as f64,
                    "e_rebuffer_duration_ms": (50 * (index % 3)) as f64,
                    "e_startup_outcome": if index % 8 == 0 { "failed_before_start" } else { "started" },
                    "e_reason": if index % 4 == 0 { "ended" } else { "exit" },
                    "e_media_duration_seconds": 600.0,
                    "e_furthest_progress_ratio": (index % 5) as f64 / 4.0}),
            ));
        }
        if index % 5 == 0 {
            rows.push(with(
                event("playback_error", 4, start + 5_000),
                json!({"e_error_fatal": true}),
            ));
        }
    }
    // A second identity of v1 and a second start of v2 in a later hour.
    rows.push(event(
        "v1-other",
        "default",
        "v1",
        "video-z",
        Some("other"),
        "view_started",
        1,
        base + HOUR + 1_800_000,
    ));
    rows.push(event(
        "v2-again",
        "default",
        "v2",
        "video-a",
        Some("viewer-2"),
        "view_started",
        9,
        base + 3 * HOUR + 1_000,
    ));
    rows
}

/// Every report that can read rollups, with all pages of paginated ones.
/// `events_only` adds video orders that always read events.
async fn all_reports(store: &Store, base: i64, events_only: bool) -> Value {
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let query = |fields: Value| -> Query { serde_json::from_value(fields).unwrap() };
    let ranges = [
        json!({}),
        json!({"from": iso(base), "to": iso(base + 4 * HOUR)}),
        // Partial first and last hours read events; the middle reads rollups.
        json!({"from": iso(base + 1_200_000), "to": iso(base + 3 * HOUR + 600_000)}),
    ];
    // Every report that can read rollups, with all pages of paginated ones.
    let mut all = Vec::new();
    for range in ranges {
        let mut filtered = vec![range.clone()];
        for extra in [
            json!({"browser_name": "Chrome"}),
            json!({"video_id": "video-a"}),
            json!({"country": "FR", "video_id": "video-b"}),
        ] {
            let mut fields = range.clone();
            fields
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            filtered.push(fields);
        }
        for fields in &filtered {
            all.push(ok(store
                .summary("default".into(), query(fields.clone()), false)
                .await));
            all.push(ok(store
                .engagement("default".into(), query(fields.clone()))
                .await));
            all.push(ok(store
                .quality("default".into(), query(fields.clone()))
                .await));
            let sorts: &[&str] = if events_only {
                &[
                    "views",
                    "video_id",
                    "unique_viewers",
                    "total_playing_time_ms",
                ]
            } else {
                &["views", "video_id", "unique_viewers"]
            };
            for sort in sorts {
                let mut cursor = Value::Null;
                loop {
                    let mut page = fields.clone();
                    page["sort_by"] = json!(sort);
                    page["limit"] = json!(1);
                    if let Some(cursor) = cursor.as_str() {
                        page["cursor"] = json!(cursor);
                    }
                    let result = ok(store.summary("default".into(), query(page), true).await);
                    cursor = result["next_cursor"].clone();
                    all.push(result);
                    if cursor.is_null() {
                        break;
                    }
                }
            }
        }
        for group in ["browser_name", "country"] {
            for kind in [
                MetricKind::Summary,
                MetricKind::Engagement,
                MetricKind::Quality,
            ] {
                let values = if group == "country" {
                    [None, Some("FR"), Some("US")]
                } else {
                    [None, Some("Chrome"), Some("Edge")]
                };
                for value in values {
                    let mut filter = json!({});
                    if let Some(value) = value {
                        filter[group] = json!(value);
                    }
                    let mut cursor = Value::Null;
                    loop {
                        let mut page = range.clone();
                        page.as_object_mut()
                            .unwrap()
                            .extend(filter.as_object().unwrap().clone());
                        page["group_by"] = json!(group);
                        page["limit"] = json!(1);
                        if let Some(cursor) = cursor.as_str() {
                            page["cursor"] = json!(cursor);
                        }
                        let result = ok(match kind {
                            MetricKind::Summary => {
                                store.summary("default".into(), query(page), false).await
                            }
                            MetricKind::Engagement => {
                                store.engagement("default".into(), query(page)).await
                            }
                            MetricKind::Quality => {
                                store.quality("default".into(), query(page)).await
                            }
                        });
                        cursor = result["next_cursor"].clone();
                        all.push(result);
                        if cursor.is_null() {
                            break;
                        }
                    }
                }
            }
        }
    }
    for (interval, kind) in [
        ("hour", "summary"),
        ("hour", "quality"),
        ("day", "engagement"),
    ] {
        let series: crate::query::TimeseriesQuery = serde_json::from_value(json!({
                "from": iso(base + 1_200_000), "to": iso(base + 5 * HOUR),
                "interval": interval, "kind": kind}))
        .unwrap();
        all.push(ok(store.timeseries("default".into(), series).await));
    }
    Value::Array(all)
}

#[tokio::test]
async fn rollups_match_event_reports_through_refreshes_late_events_and_pages() {
    let fixture = Fixture::with_settings(&["default"], "cache.report_ttl_seconds = 0").await;
    let store = &fixture.store;
    let base = 1_000 * HOUR;
    fixture.insert(rollup_rows(base)).await;
    let iso = |ms: i64| {
        chrono::DateTime::from_timestamp_millis(ms)
            .unwrap()
            .to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
    };
    let query = |fields: Value| -> Query { serde_json::from_value(fields).unwrap() };
    let reports = move || all_reports(store, base, true);
    let compare = move |label: &'static str| async move {
        store.rollup_reads.store(false, Ordering::Relaxed);
        let raw = reports().await;
        store.rollup_reads.store(true, Ordering::Relaxed);
        let combined = reports().await;
        assert_same(&combined, &raw, label);
    };
    let whole = query(json!({"from": iso(base), "to": iso(base + 4 * HOUR)}))
        .filters()
        .unwrap();
    let whole = &whole;
    let planned = move || async move {
        store
            .rollup_plan("default", whole, None, i64::MIN, i64::MAX)
            .await
            .unwrap()
            .map(|plan| plan.hours)
    };
    assert_eq!(planned().await, None, "no hour is settled before a refresh");
    compare("before refresh").await;

    assert_eq!(store.refresh_rollups_with(0, 0).await.unwrap(), Some(4));
    let all_hours: Vec<i64> = (0..4).map(|h| base + h * HOUR).collect();
    assert_eq!(planned().await, Some(all_hours.clone()));
    compare("after refresh").await;

    // A late snapshot changes v5 (hour 1); an earlier start moves v6 from hour
    // 2 to hour 0; a new view starts in hour 3. Those hours read events again.
    fixture
        .insert(vec![
            with(
                event(
                    "v5-late",
                    "default",
                    "v5",
                    "video-a",
                    Some("viewer-0"),
                    "view_snapshot",
                    5,
                    base + HOUR + 900_000,
                ),
                json!({"playing_time_ms": 99000.0}),
            ),
            with(
                event(
                    "v6-early",
                    "default",
                    "v6",
                    "video-a",
                    Some("viewer-1"),
                    "view_started",
                    0,
                    base + 1_000,
                ),
                json!({"context_browser_name": "Edge"}),
            ),
            event(
                "v99-start",
                "default",
                "v99",
                "video-c",
                Some("new"),
                "view_started",
                1,
                base + 3 * HOUR + 5_000,
            ),
        ])
        .await;
    assert_eq!(planned().await, None);
    let partial = query(json!({"from": iso(base), "to": iso(base + 2 * HOUR)}))
        .filters()
        .unwrap();
    assert!(
        store
            .rollup_plan("default", &partial, None, i64::MIN, i64::MAX)
            .await
            .unwrap()
            .is_none(),
        "hours 0 and 1 changed"
    );
    compare("after late events").await;

    assert_eq!(store.refresh_rollups_with(0, 0).await.unwrap(), Some(4));
    assert_eq!(planned().await, Some(all_hours.clone()));
    compare("after second refresh").await;

    // Hours with an expired event read events, where the event is hidden.
    let cutoff = base + HOUR + 30 * 60_000;
    let plan = store
        .rollup_plan("default", whole, None, cutoff, i64::MAX)
        .await
        .unwrap()
        .unwrap();
    // Hour 2 holds a start of v6, whose earlier start in hour 0 has expired.
    assert_eq!(plan.hours, vec![base + 3 * HOUR]);

    // A page cursor freezes its snapshot: events stored after page one, even
    // once refreshed into rollups, stay out of later pages.
    let page = |cursor: Option<&str>| {
        let mut fields = json!({"group_by": "browser_name", "limit": 1, "from": iso(base), "to": iso(base + 4 * HOUR)});
        if let Some(cursor) = cursor {
            fields["cursor"] = json!(cursor);
        }
        query(fields)
    };
    let first = ok(store.summary("default".into(), page(None), false).await);
    let cursor = first["next_cursor"].as_str().unwrap().to_owned();
    fixture
        .insert(vec![with(
            event(
                "v100-start",
                "default",
                "v100",
                "video-a",
                Some("late"),
                "view_started",
                1,
                base + 2 * HOUR + 7_000,
            ),
            json!({"context_browser_name": "Chrome"}),
        )])
        .await;
    store.refresh_rollups_with(0, 0).await.unwrap();
    let combined = ok(store
        .summary("default".into(), page(Some(&cursor)), false)
        .await);
    store.rollup_reads.store(false, Ordering::Relaxed);
    let raw = ok(store
        .summary("default".into(), page(Some(&cursor)), false)
        .await);
    assert_same(&combined, &raw, "second page");
}

#[tokio::test]
async fn rollup_runs_claim_once_and_skip_until_the_interval() {
    let fixture = Fixture::new(&["default"]).await;
    let store = &fixture.store;
    fixture
        .insert(vec![event(
            "a",
            "default",
            "a",
            "video",
            None,
            "view_started",
            1,
            5000,
        )])
        .await;
    assert_eq!(store.refresh_rollups_with(0, 0).await.unwrap(), Some(1));
    // Another server checking within the interval does nothing.
    assert_eq!(store.refresh_rollups_with(0, 60_000).await.unwrap(), None);
    // A later run with nothing new completes without recomputing.
    assert_eq!(store.refresh_rollups_with(0, 0).await.unwrap(), Some(0));
    let runs = store
        .client
        .all(
            "SELECT version, completed FROM rollup_runs ORDER BY version, completed",
            &Params::default(),
        )
        .await
        .unwrap();
    assert_eq!(
        runs,
        vec![
            vec![json!(1), json!(0)],
            vec![json!(1), json!(1)],
            vec![json!(2), json!(0)],
            vec![json!(2), json!(1)]
        ]
    );
}

#[tokio::test]
async fn lagged_reports_read_the_last_refresh_and_keep_pages_on_their_run() {
    let fixture = Fixture::with_settings(
        &["default"],
        "cache.report_ttl_seconds = 0\nclickhouse.rollup_max_lag_seconds = 3600",
    )
    .await;
    let store = &fixture.store;
    let base = 1_000 * HOUR;
    fixture.insert(rollup_rows(base)).await;
    let exact = || async {
        store.rollup_reads.store(false, Ordering::Relaxed);
        let reports = all_reports(store, base, false).await;
        store.rollup_reads.store(true, Ordering::Relaxed);
        reports
    };
    // Before any refresh, reports read events.
    let before = exact().await;
    assert_same(
        &all_reports(store, base, false).await,
        &before,
        "before refresh",
    );

    // After a refresh, every hour, partial ones included, comes from rollups
    // and matches events read at the same time.
    store.refresh_rollups_with(0, 0).await.unwrap();
    let plan = store
        .lagged_plan("default", &Filters::default(), None, i64::MIN, None)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(plan.hours.len(), 4);
    assert!(!plan.reads_events());
    let refreshed = exact().await;
    assert_same(
        &all_reports(store, base, false).await,
        &refreshed,
        "after refresh",
    );

    // Keep every page of one breakdown, then change the data and refresh.
    let page = |cursor: Option<&str>| {
        let mut fields = json!({"group_by": "browser_name", "limit": 1});
        if let Some(cursor) = cursor {
            fields["cursor"] = json!(cursor);
        }
        serde_json::from_value::<Query>(fields).unwrap()
    };
    let mut pages = vec![ok(store.summary("default".into(), page(None), false).await)];
    while let Some(cursor) = pages.last().unwrap()["next_cursor"].as_str() {
        let next = ok(store
            .summary("default".into(), page(Some(cursor)), false)
            .await);
        pages.push(next);
    }
    assert!(pages.len() > 2);

    // New events stay out of lagged reports until the next refresh.
    fixture
        .insert(vec![
            with(
                event(
                    "v5-late",
                    "default",
                    "v5",
                    "video-a",
                    Some("viewer-0"),
                    "view_snapshot",
                    5,
                    base + HOUR + 900_000,
                ),
                json!({"playing_time_ms": 99000.0}),
            ),
            with(
                event(
                    "v200-start",
                    "default",
                    "v200",
                    "video-a",
                    Some("new"),
                    "view_started",
                    1,
                    base + 2 * HOUR + 5_000,
                ),
                json!({"context_browser_name": "Chrome"}),
            ),
        ])
        .await;
    assert_same(
        &all_reports(store, base, false).await,
        &refreshed,
        "lagging",
    );
    assert_ne!(exact().await, refreshed, "events changed");

    // Later pages keep reading the run of their first page.
    store.refresh_rollups_with(0, 0).await.unwrap();
    let mut cursor = pages[0]["next_cursor"].as_str().unwrap().to_owned();
    for (index, expected) in pages.iter().enumerate().skip(1) {
        let next = ok(store
            .summary("default".into(), page(Some(&cursor)), false)
            .await);
        assert_same(&next, expected, &format!("page {index}"));
        match next["next_cursor"].as_str() {
            Some(next) => cursor = next.to_owned(),
            None => assert_eq!(index, pages.len() - 1),
        }
    }
    // New first pages read the new run, which matches events again.
    assert_same(
        &all_reports(store, base, false).await,
        &exact().await,
        "after second refresh",
    );

    // Hours with an expired event read events, where the event is hidden.
    let cutoff = base + HOUR + 30 * 60_000;
    let plan = store
        .lagged_plan("default", &Filters::default(), None, cutoff, None)
        .await
        .unwrap()
        .unwrap();
    // Hours 0 and 1 hold events received before the cutoff.
    assert_eq!(plan.hours, vec![base + 2 * HOUR, base + 3 * HOUR]);
    assert!(plan.reads_events());
}

#[tokio::test]
async fn lagged_reads_stop_when_refreshes_fall_behind() {
    let fixture =
        Fixture::with_settings(&["default"], "clickhouse.rollup_max_lag_seconds = 1").await;
    let store = &fixture.store;
    fixture
        .insert(vec![event(
            "a",
            "default",
            "a",
            "video",
            None,
            "view_started",
            1,
            5000,
        )])
        .await;
    store.refresh_rollups_with(0, 0).await.unwrap();
    let plan = || async {
        store
            .lagged_plan("default", &Filters::default(), None, i64::MIN, None)
            .await
            .unwrap()
    };
    assert!(plan().await.is_some());
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    assert!(plan().await.is_none(), "an old refresh is not read");
}

#[tokio::test]
async fn servers_lease_distinct_node_ids_and_one_sweeps_each_interval() {
    let fixture = Fixture::new(&["default"]).await;
    // More servers on the same database, none with a configured node ID.
    let mut stores = vec![fixture.store.clone()];
    for _ in 0..3 {
        stores.push(
            Store::open(&fixture.config, fixture.registry.clone())
                .await
                .unwrap(),
        );
    }
    let mut nodes: Vec<i64> = stores
        .iter()
        .map(|store| store.node.load(Ordering::Acquire))
        .collect();
    nodes.sort();
    nodes.dedup();
    assert_eq!(
        nodes.len(),
        stores.len(),
        "every server leases its own node ID"
    );

    let interval = std::time::Duration::from_secs(3600);
    let mut swept = 0;
    for store in &stores {
        if store.purge_expired_once(interval).await.unwrap().is_some() {
            swept += 1;
        }
    }
    assert_eq!(swept, 1, "one server sweeps per interval");
}
