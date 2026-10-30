use super::*;
use std::time::Duration;

#[tokio::test]
async fn summaries_preserve_start_identity_ties_null_measurements_and_page_watermarks() {
    for separate_reads in [false, true] {
        let directory =
            std::env::temp_dir().join(format!("collector-summary-{}", uuid::Uuid::new_v4()));
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
            separate_reads,
            Some(1),
        )
        .unwrap();
        store.call("test_summary_fixture", |connection| {
            connection.execute_batch("INSERT INTO events
                (event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,
                 received_at_ms,event_remainder,context_format,context_browser_name,playing_time_ms)
                SELECT id,project,view_value,video,viewer,'chunkify.video.'||kind,seq,time_value,time_value,'{}',0,browser,playing FROM (VALUES
                ('a-later','default','a','video-a',NULL,'view_started',2,1000,'Firefox',NULL),
                ('a-first','default','a','video-a',NULL,'view_started',1,2000,'Chrome',NULL),
                ('a-tie','default','a','video-a',NULL,'view_started',1,2000,'Safari',NULL),
                ('a-other-start','default','a','video-z','other','view_started',1,3000,'Safari',NULL),
                ('a-snapshot','default','a','video-a',NULL,'view_snapshot',90,8000,'Chrome',1000),
                ('a-end','default','a','video-a',NULL,'view_ended',3,4000,'Chrome',200),
                ('a-end-tie','default','a','video-a',NULL,'view_ended',3,4000,'Chrome',NULL),
                ('a-other-end','default','a','video-z','other','view_ended',100,9000,'Safari',99999),
                ('b-start','default','b','video-b','b-viewer','view_started',1,1000,'Firefox',NULL),
                ('b-snapshot','default','b','video-b','b-viewer','view_snapshot',5,5000,'Firefox',1500),
                ('b-tie','default','b','video-b','b-viewer','view_snapshot',5,5000,'Firefox',2500),
                ('b-lower','default','b','video-b','b-viewer','view_snapshot',4,6000,'Firefox',5000),
                ('c-start','default','c','video-c','c-viewer','view_started',1,1000,'Chrome',NULL),
                ('orphan','default','orphan','video-orphan','orphan','view_ended',1,1000,'Chrome',99999),
                ('a-first','other','a','video-a','other-viewer','view_started',1,1000,'Chrome',NULL),
                ('a-end','other','a','video-a','other-viewer','view_ended',2,2000,'Chrome',900)
                ) AS fixture(id,project,view_value,video,viewer,kind,seq,time_value,browser,playing)").map_err(|error| ApiError::bad(error.to_string()))?;
            Ok(())
        }).await.unwrap_or_else(|error| panic!("{}", error.message));
        let totals = |views, viewers, time| json!({"views":views,"unique_viewers":viewers,"total_playing_time_ms":time});
        for (query, expected) in [
            (Query::default(), totals(3, 2, 2500.0)),
            (
                Query {
                    browser_name: Some("Chrome".into()),
                    ..Query::default()
                },
                totals(2, 1, 0.0),
            ),
            (
                Query {
                    browser_name: Some("Firefox".into()),
                    ..Query::default()
                },
                totals(1, 1, 2500.0),
            ),
            (
                Query {
                    browser_name: Some("Safari".into()),
                    ..Query::default()
                },
                totals(0, 0, 0.0),
            ),
            (
                Query {
                    video_id: Some("video-z".into()),
                    ..Query::default()
                },
                totals(0, 0, 0.0),
            ),
            (
                Query {
                    viewer_id: Some("other".into()),
                    ..Query::default()
                },
                totals(0, 0, 0.0),
            ),
            (
                Query {
                    from: Some("1970-01-01T00:00:02Z".into()),
                    to: Some("1970-01-01T00:00:03Z".into()),
                    ..Query::default()
                },
                totals(1, 0, 0.0),
            ),
            (
                Query {
                    from: Some("1970-01-01T00:00:01Z".into()),
                    to: Some("1970-01-01T00:00:02Z".into()),
                    ..Query::default()
                },
                totals(2, 2, 2500.0),
            ),
        ] {
            assert_eq!(
                store
                    .summary("default".into(), query, false)
                    .await
                    .unwrap_or_else(|error| panic!("{}", error.message)),
                expected
            );
        }
        assert_eq!(
            store
                .summary("other".into(), Query::default(), false)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message)),
            totals(1, 1, 900.0)
        );
        // Historical records can lack durations. Available playing time alone
        // must not turn an unknown buffering measurement into a zero sample.
        assert_eq!(
            store
                .quality("default".into(), Query::default())
                .await
                .unwrap_or_else(|error| panic!("{}", error.message)),
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
            store
                .engagement("default".into(), Query::default())
                .await
                .unwrap_or_else(|error| panic!("{}", error.message)),
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
        let first = store
            .summary(
                "default".into(),
                Query {
                    limit: Some(1),
                    ..Query::default()
                },
                true,
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            first["videos"],
            json!([{"playback_score":{"value":null,"sample_views":0},"video_id":"video-a","views":1,"unique_viewers":0,"total_playing_time_ms":0.0}])
        );
        // An earlier start arriving after page one must not alter its frozen cohort.
        store.call("test_late_start", |connection| {
            connection.execute_batch("INSERT INTO events
                (event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format)
                VALUES ('b-new-first','default','b','video-x','new-viewer','chunkify.video.view_started',0,500,10000,'{}',0)").map_err(|error| ApiError::bad(error.to_string()))?;
            Ok(())
        }).await.unwrap_or_else(|error| panic!("{}", error.message));
        let second = store
            .summary(
                "default".into(),
                Query {
                    limit: Some(1),
                    cursor: Some(first["next_cursor"].as_str().unwrap().into()),
                    ..Query::default()
                },
                true,
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            second["videos"],
            json!([{"playback_score":{"value":null,"sample_views":0},"video_id":"video-b","views":1,"unique_viewers":1,"total_playing_time_ms":2500.0}])
        );
        let third = store
            .summary(
                "default".into(),
                Query {
                    limit: Some(1),
                    cursor: Some(second["next_cursor"].as_str().unwrap().into()),
                    ..Query::default()
                },
                true,
            )
            .await
            .unwrap_or_else(|error| panic!("{}", error.message));
        assert_eq!(
            third["videos"],
            json!([{"playback_score":{"value":null,"sample_views":0},"video_id":"video-c","views":1,"unique_viewers":1,"total_playing_time_ms":0.0}])
        );
        assert!(third["next_cursor"].is_null());
        assert_eq!(
            store
                .summary("default".into(), Query::default(), false)
                .await
                .unwrap_or_else(|error| panic!("{}", error.message)),
            totals(3, 2, 0.0)
        );
        drop(store);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn playback_scores_agree_across_sessions_videos_quality_and_buckets() {
    for separate_reads in [false, true] {
        let directory =
            std::env::temp_dir().join(format!("collector-score-{}", uuid::Uuid::new_v4()));
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
            separate_reads,
            Some(1),
        )
        .unwrap();
        store.call("score_fixture", |db| {
            db.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,context_browser_name,playing_time_ms,e_startup_time_ms,e_rebuffer_duration_ms,e_startup_outcome)
                SELECT id||'-start','default',id,'video','chunkify.video.view_started',1,1000,1000,'{}',0,'Chrome',NULL,NULL,NULL,NULL
                FROM (VALUES ('perfect'),('slow'),('buffer'),('combined'),('fatal'),('missing'),('pending'),('exit')) AS v(id);
                INSERT INTO events(event_id,project_id,view_id,video_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,playing_time_ms,e_startup_time_ms,e_rebuffer_duration_ms,e_startup_outcome)
                SELECT id||'-end','default',id,'video','chunkify.video.view_ended',3,5000,5000,'{}',0,playing,startup,rebuffer,outcome
                FROM (VALUES ('perfect',10000,0,0,'started'),('slow',10000,2000,0,'started'),('buffer',9800,0,200,'started'),('combined',9900,2000,100,'started'),('missing',10000,100,NULL,'started'),('exit',0,NULL,0,'ended_before_start')) AS v(id,playing,startup,rebuffer,outcome);
                INSERT INTO events(event_id,project_id,view_id,video_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,e_error_fatal)
                VALUES ('fatal-error','default','fatal','video','chunkify.video.playback_error',2,2000,2000,'{}',0,TRUE);
                INSERT INTO events(event_id,project_id,view_id,video_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,playing_time_ms,e_startup_time_ms,e_rebuffer_duration_ms)
                VALUES ('old-snapshot','default','slow','video','chunkify.video.view_snapshot',99,9000,9000,'{}',0,9000,0,0)")?;
            Ok(())
        }).await.unwrap_or_else(|e| panic!("{}",e.message));
        let expected = [
            ("perfect", Some(100.0)),
            ("slow", Some(80.0)),
            ("buffer", Some(80.0)),
            ("combined", Some(72.0)),
            ("fatal", Some(0.0)),
            ("missing", None),
            ("pending", None),
            ("exit", None),
        ];
        let page = store
            .views("default".into(), serde_json::from_value(json!({})).unwrap())
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
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
            let detail = store
                .view("default".into(), id.into(), Query::default())
                .await
                .unwrap();
            assert_eq!(detail["view"]["playback_score"], row["playback_score"]);
        }
        let quality = store
            .quality("default".into(), Query::default())
            .await
            .unwrap();
        assert_eq!(quality["playback_score"]["sample_views"], 5);
        assert!((quality["playback_score"]["value"].as_f64().unwrap() - 66.4).abs() < 1e-9);
        let videos = store
            .summary("default".into(), Query::default(), true)
            .await
            .unwrap();
        assert_eq!(
            videos["videos"][0]["playback_score"],
            quality["playback_score"]
        );
        let groups = store
            .quality(
                "default".into(),
                serde_json::from_value(json!({"group_by":"browser_name"})).unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            groups["groups"][0]["data"]["playback_score"],
            quality["playback_score"]
        );
        let series = store.timeseries("default".into(),serde_json::from_value(json!({"kind":"quality","from":"1970-01-01T00:00:00Z","to":"1970-01-03T00:00:00Z","interval":"day"})).unwrap()).await.unwrap();
        assert_eq!(
            series["buckets"][0]["playback_score"],
            quality["playback_score"]
        );
        assert_eq!(
            series["buckets"][1]["playback_score"],
            json!({"value":null,"sample_views":0})
        );
        drop(store);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
