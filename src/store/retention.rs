use super::metrics::{CohortMetrics, view_cohort};
use super::*;

impl Store {
    /// Count each attempt once per media-position bucket, independent of replays.
    pub async fn retention(&self, project: String, query: Query) -> Result<Value, ApiError> {
        validate_query(&query)?;
        let export = query.export.clone();
        let filters = query.filters()?;
        self.read("retention", project.clone(), move |db| {
            let through = watermark(db, &project)?;
            let (sql, values) = view_cohort(project, through, &filters, CohortMetrics::Retention);
            let sql = format!("{sql}, measured AS (
                SELECT * FROM view_metrics
                WHERE measurement.media_duration_seconds > 0
                    AND isfinite(measurement.media_duration_seconds)
                    AND json_type(measurement.played_ranges) = 'ARRAY'
            )");
            let (views, samples, duration): (i64, i64, Option<f64>) = db.query_row(
                &format!("{sql} SELECT (SELECT COUNT(*) FROM view_metrics), COUNT(*), MAX(measurement.media_duration_seconds) FROM measured"),
                params_from_iter(values.iter()), |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            let Some(duration) = duration else {
                return Ok(json!({"views":views,"sample_views":0,"duration_seconds":null,"interval_seconds":null,"buckets":[]}));
            };
            // At most 200 buckets, with one-second resolution for short videos.
            let interval = (duration / 200.0).ceil().max(1.0);
            let count = (duration / interval).ceil() as i64;
            let mut args = values;
            args.extend([SqlValue::Double(interval), SqlValue::Double(duration), SqlValue::BigInt(count)]);
            let mut statement = db.prepare(&format!("{sql}, bounds AS (
                SELECT ? AS step, ? AS duration
            ), buckets AS (
                SELECT i * step AS start_seconds, LEAST((i + 1) * step, duration) AS end_seconds
                FROM range(?) AS positions(i), bounds
            ), ranges AS (
                SELECT view_id,
                    TRY_CAST(json_extract(r.value, '$.start_seconds') AS DOUBLE) AS start_seconds,
                    LEAST(TRY_CAST(json_extract(r.value, '$.end_seconds') AS DOUBLE), measurement.media_duration_seconds) AS end_seconds
                FROM measured, json_each(measurement.played_ranges) AS r
            )
            SELECT b.start_seconds, b.end_seconds, COUNT(DISTINCT r.view_id)
            FROM buckets b LEFT JOIN ranges r ON r.start_seconds < b.end_seconds AND r.end_seconds > b.start_seconds
            GROUP BY b.start_seconds, b.end_seconds ORDER BY b.start_seconds"))?;
            let rows = query_rows(&mut statement, &args, export.is_some())?.mapped(|r| {
                let watched: i64 = r.get(2)?;
                Ok(json!({"start_seconds":r.get::<_, f64>(0)?, "end_seconds":r.get::<_, f64>(1)?,
                    "views":watched,"ratio":watched as f64 / samples as f64}))
            });
            let mut buckets = Vec::new();
            for row in rows { crate::exports::emit(&export, &mut buckets, row?)?; }
            Ok(json!({"views":views,"sample_views":samples,"duration_seconds":duration,
                "interval_seconds":interval,"buckets":buckets}))
        }).await
    }
}

pub(crate) fn validate_query(query: &Query) -> Result<(), ApiError> {
    if query.video_id.is_none()
        || query.event_name.is_some()
        || query.group_by.is_some()
        || query.sort_by.is_some()
        || query.order.is_some()
        || query.cursor.is_some()
        || query.limit.is_some()
    {
        return Err(ApiError::bad(
            "Retention requires video_id and accepts only audience and date filters",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn retention_uses_one_measurement_per_identity_and_preserves_unwatched_gaps() {
        let dir = std::env::temp_dir().join(format!("retention-report-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&dir).unwrap();
        let (store, worker) = Store::open(
            &dir.join("analytics.duckdb"),
            BatchConfig {
                max_events: 100,
                max_bytes: 1048576,
                delay: Duration::from_millis(10),
                queue_bytes: 16777216,
                queue_requests: 256,
                bulk: true,
            },
            false,
            64,
            true,
            Some(1),
        )
        .unwrap();
        store.call("fixture", |db| {
            let payload = |ranges: Value| json!({"media_duration_seconds":5,"played_ranges":ranges});
            let ranges = json!([{"start_seconds":0,"end_seconds":1},{"start_seconds":3,"end_seconds":4}]);
            for (id, project, view, video, kind, seq, browser, body) in [
                ("a-start","default","a","video","view_started",1,"Chrome",json!({})),
                ("a-snapshot","default","a","video","view_snapshot",99,"Chrome",payload(json!([{"start_seconds":0,"end_seconds":5}]))),
                ("a-end","default","a","video","view_ended",2,"Chrome",payload(ranges.clone())),
                ("a-duplicate","default","a","video","view_ended",2,"Chrome",payload(ranges.clone())),
                ("a-other-identity","default","a","other","view_ended",999,"Chrome",payload(json!([{"start_seconds":0,"end_seconds":5}]))),
                ("b-start","default","b","video","view_started",1,"Firefox",json!({})),
                ("b-snapshot","default","b","video","view_snapshot",2,"Firefox",payload(json!([{"start_seconds":0.5,"end_seconds":2}]))),
                ("c-start","default","c","video","view_started",1,"Chrome",json!({})),
                ("c-end","default","c","video","view_ended",2,"Chrome",payload(json!([]))),
                ("old-start","default","old","video","view_started",1,"Chrome",json!({})),
                ("old-end","default","old","video","view_ended",2,"Chrome",json!({"media_duration_seconds":5})),
                ("foreign-start","foreign","a","video","view_started",1,"Chrome",json!({})),
                ("foreign-end","foreign","a","video","view_ended",2,"Chrome",payload(ranges.clone())),
                ("orphan","default","orphan","video","view_ended",2,"Chrome",payload(ranges)),
            ] {
                db.execute("INSERT INTO events (event_id, project_id, view_id, video_id, event_name, sequence, occurred_at_ms, received_at_ms, event_remainder, context_format, context_browser_name) VALUES (?,?,?,?,?,?,?,?,?,0,?)",
                    duckdb::params![id,project,view,video,format!("chunkify.video.{kind}"),seq,if kind=="view_started" {1000} else {2000},2000,body.to_string(),browser])?;
            }
            Ok(())
        }).await.unwrap();
        let query = || Query {
            video_id: Some("video".into()),
            from: Some("1970-01-01T00:00:01Z".into()),
            to: Some("1970-01-01T00:00:01.500Z".into()),
            ..Query::default()
        };
        let report = store
            .retention("default".into(), query())
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
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
        let filtered = store
            .retention(
                "default".into(),
                Query {
                    browser_name: Some("Firefox".into()),
                    ..query()
                },
            )
            .await
            .unwrap();
        assert_eq!(filtered["views"], 1);
        assert_eq!(filtered["buckets"][3]["views"], 0);
        let empty = store
            .retention(
                "default".into(),
                Query {
                    view_id: Some("old".into()),
                    ..query()
                },
            )
            .await
            .unwrap();
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
        store
            .call("long", |db| {
                db.execute(
                    "UPDATE events SET event_remainder = ? WHERE event_id = 'b-snapshot'",
                    [json!({"media_duration_seconds":1001,"played_ranges":[]}).to_string()],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let long = store.retention("default".into(), query()).await.unwrap();
        assert_eq!(long["interval_seconds"], 6.0);
        assert_eq!(long["buckets"].as_array().unwrap().len(), 167);
        assert_eq!(
            long["buckets"].as_array().unwrap().last().unwrap()["end_seconds"],
            1001.0
        );
        drop(store);
        worker.join().unwrap().unwrap();
        std::fs::remove_dir_all(dir).unwrap();
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
}
