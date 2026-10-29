use super::metrics::{CohortMetrics, view_cohort};
use super::*;

impl Store {
    pub async fn renditions(&self, project: String, query: Query) -> Result<Value, ApiError> {
        if query.event_name.is_some()
            || query.group_by.is_some()
            || query.sort_by.is_some()
            || query.order.is_some()
            || query.cursor.is_some()
        {
            return Err(ApiError::bad("Unsupported rendition query parameter"));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        self.read("renditions", project.clone(), move |connection| {
            let through = watermark(connection, &project)?;
            let (sql, mut values) = view_cohort(project, through, &filters, CohortMetrics::Renditions);
            // Expand only the selected final/latest cumulative snapshot. Summing
            // every snapshot would count the same watch time repeatedly.
            let sql = format!("{sql}, rendition_entries AS (
                SELECT view_id,
                    TRY_CAST(json_extract(entry.value, '$.width') AS DOUBLE) AS width,
                    TRY_CAST(json_extract(entry.value, '$.height') AS DOUBLE) AS height,
                    json_extract_string(entry.value, '$.codec') AS codec,
                    TRY_CAST(json_extract(entry.value, '$.bitrate_bps') AS DOUBLE) AS bitrate_bps,
                    TRY_CAST(json_extract(entry.value, '$.frame_rate') AS DOUBLE) AS frame_rate,
                    TRY_CAST(json_extract(entry.value, '$.playing_time_ms') AS DOUBLE) AS playing_time_ms
                FROM view_metrics, json_each(measurement.rendition_playing_time) AS entry
                WHERE entry.type = 'OBJECT'
            ), measured_renditions AS (
                SELECT * FROM rendition_entries WHERE playing_time_ms > 0
            ), ranked_renditions AS (
                SELECT width, height, codec, bitrate_bps, frame_rate,
                    COUNT(DISTINCT view_id) AS views, SUM(playing_time_ms) AS playing_time_ms
                FROM measured_renditions GROUP BY width, height, codec, bitrate_bps, frame_rate
            )");
            let (views, samples, total, bitrate_time, average, count): (i64, i64, f64, f64, Option<f64>, i64) = connection.query_row(
                &format!("{sql} SELECT
                    (SELECT COUNT(*) FROM view_metrics), COUNT(DISTINCT view_id),
                    COALESCE(SUM(playing_time_ms), 0),
                    COALESCE(SUM(playing_time_ms) FILTER (WHERE bitrate_bps IS NOT NULL), 0),
                    SUM(bitrate_bps * playing_time_ms) / NULLIF(SUM(playing_time_ms) FILTER (WHERE bitrate_bps IS NOT NULL), 0),
                    (SELECT COUNT(*) FROM ranked_renditions)
                    FROM measured_renditions"), params_from_iter(values.iter()),
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)))?;
            if !total.is_finite() || !bitrate_time.is_finite() || average.is_some_and(|n| !n.is_finite()) {
                return Err(ApiError::internal());
            }
            values.push(SqlValue::BigInt(limit as i64));
            let mut statement = connection.prepare(&format!("{sql}
                SELECT width, height, codec, bitrate_bps, frame_rate, views, playing_time_ms
                FROM ranked_renditions ORDER BY playing_time_ms DESC,
                    width ASC NULLS FIRST, height ASC NULLS FIRST, codec ASC NULLS FIRST,
                    bitrate_bps ASC NULLS FIRST, frame_rate ASC NULLS FIRST LIMIT ?"))?;
            let rows = query_rows(&mut statement, &values, export.is_some())?.mapped(|row| {
                let time: f64 = row.get(6)?;
                Ok(json!({"width":row.get::<_, Option<f64>>(0)?, "height":row.get::<_, Option<f64>>(1)?,
                    "codec":row.get::<_, Option<String>>(2)?, "bitrate_bps":row.get::<_, Option<f64>>(3)?,
                    "frame_rate":row.get::<_, Option<f64>>(4)?, "views":row.get::<_, i64>(5)?,
                    "playing_time_ms":time, "playing_time_ratio": time / total}))
            });
            let mut renditions = Vec::new();
            for row in rows { crate::exports::emit(&export, &mut renditions, row?)?; }
            Ok(json!({"views":views, "sample_views":samples, "total_playing_time_ms":total,
                "bitrate_sample_playing_time_ms":bitrate_time, "average_bitrate_bps":average,
                "total_renditions":count, "renditions":renditions}))
        }).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn renditions_select_one_measurement_per_view_and_weight_known_bitrates() {
        let directory =
            std::env::temp_dir().join(format!("collector-renditions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let (store, worker) = Store::open(
            &directory.join("analytics.duckdb"),
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
        store.call("rendition_fixture", |c| {
            let rendition = |width, height, bitrate, time| json!({"width":width,"height":height,"codec":"avc1","bitrate_bps":bitrate,"frame_rate":30,"playing_time_ms":time});
            let a = json!({"rendition_playing_time":[rendition(1280,720,Some(2000000),6000),rendition(640,360,Some(1000000),4000)]});
            let latest = json!({"rendition_playing_time":[rendition(1280,720,Some(2000000),2000),rendition(640,360,None,1000)]});
            for (id, project, view, video, viewer, kind, seq, browser, payload) in [
                ("a-start","default","a","video",None,"view_started",1,"Chrome",json!({})),
                ("a-snapshot","default","a","video",None,"view_snapshot",90,"Chrome",latest.clone()),
                ("a-end","default","a","video",None,"view_ended",2,"Chrome",a.clone()),
                ("a-wrong-identity","default","a","other-video",Some("other"),"view_ended",100,"Chrome",a.clone()),
                ("b-start","default","b","video",None,"view_started",1,"Chrome",json!({})),
                ("b-snapshot","default","b","video",None,"view_snapshot",2,"Chrome",a.clone()),
                ("b-end","default","b","video",None,"view_ended",3,"Chrome",json!({"rendition_playing_time":null})),
                ("c-start","default","c","video",None,"view_started",1,"Firefox",json!({})),
                ("c-old","default","c","video",None,"view_snapshot",2,"Firefox",a.clone()),
                ("c-latest","default","c","video",None,"view_snapshot",3,"Firefox",latest),
                ("d-start","default","d","video",None,"view_started",1,"Chrome",json!({})),
                ("orphan","default","orphan","video",None,"view_ended",1,"Chrome",a.clone()),
                ("foreign-start","other","x","video",None,"view_started",1,"Chrome",json!({})),
                ("foreign-end","other","x","video",None,"view_ended",2,"Chrome",a),
            ] {
                c.execute("INSERT INTO events (event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,context_browser_name) VALUES (?,?,?,?,?,?,?,?,?,?,0,?)",
                    duckdb::params![id,project,view,video,viewer,format!("chunkify.video.{kind}"),seq,if kind=="view_started" {1000} else {2000},2000,payload.to_string(),browser])?;
            }
            Ok(())
        }).await.unwrap();
        let result = store
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
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(result["views"], 4);
        assert_eq!(result["sample_views"], 2);
        assert_eq!(result["total_renditions"], 3);
        assert_eq!(result["total_playing_time_ms"], 13000.0);
        assert_eq!(result["bitrate_sample_playing_time_ms"], 12000.0);
        assert!(
            (result["average_bitrate_bps"].as_f64().unwrap() - 20000000000.0 / 12000.0).abs()
                < 0.001
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
        let chrome = store
            .renditions(
                "default".into(),
                Query {
                    browser_name: Some("Chrome".into()),
                    ..Query::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(chrome["average_bitrate_bps"], 1600000.0);
        assert_eq!(chrome["views"], 3);
        for view in ["b", "d", "missing"] {
            let empty = store
                .renditions(
                    "default".into(),
                    Query {
                        view_id: Some(view.into()),
                        ..Query::default()
                    },
                )
                .await
                .unwrap();
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
        drop(store);
        worker.join().unwrap().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
