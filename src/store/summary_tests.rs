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
            None,
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
        }).await.unwrap_or_else(|error| panic!("{}", error.1));
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
                    .unwrap_or_else(|error| panic!("{}", error.1)),
                expected
            );
        }
        assert_eq!(
            store
                .summary("other".into(), Query::default(), false)
                .await
                .unwrap_or_else(|error| panic!("{}", error.1)),
            totals(1, 1, 900.0)
        );
        // Historical records can lack durations. Available playing time alone
        // must not turn an unknown buffering measurement into a zero sample.
        assert_eq!(
            store
                .quality("default".into(), Query::default())
                .await
                .unwrap(),
            json!({
                "views":3, "startup_time_ms":{"sample_views":0,"median":null,"p95":null},
                "buffering":{"sample_views":0,"total_playing_time_ms":0.0,"total_rebuffer_duration_ms":0.0,"ratio":null},
                "fatal_errors":{"affected_views":0,"ratio":0.0}
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
            .unwrap_or_else(|error| panic!("{}", error.1));
        assert_eq!(
            first["videos"],
            json!([{"video_id":"video-a","views":1,"unique_viewers":0,"total_playing_time_ms":0.0}])
        );
        // An earlier start arriving after page one must not alter its frozen cohort.
        store.call("test_late_start", |connection| {
            connection.execute_batch("INSERT INTO events
                (event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format)
                VALUES ('b-new-first','default','b','video-x','new-viewer','chunkify.video.view_started',0,500,10000,'{}',0)").map_err(|error| ApiError::bad(error.to_string()))?;
            Ok(())
        }).await.unwrap_or_else(|error| panic!("{}", error.1));
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
            .unwrap_or_else(|error| panic!("{}", error.1));
        assert_eq!(
            second["videos"],
            json!([{"video_id":"video-b","views":1,"unique_viewers":1,"total_playing_time_ms":2500.0}])
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
            .unwrap_or_else(|error| panic!("{}", error.1));
        assert_eq!(
            third["videos"],
            json!([{"video_id":"video-c","views":1,"unique_viewers":1,"total_playing_time_ms":0.0}])
        );
        assert!(third["next_cursor"].is_null());
        assert_eq!(
            store
                .summary("default".into(), Query::default(), false)
                .await
                .unwrap_or_else(|error| panic!("{}", error.1)),
            totals(3, 2, 0.0)
        );
        drop(store);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
