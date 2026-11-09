use super::*;
use std::time::Duration;

#[tokio::test]
async fn counted_groups_rank_full_cohorts_and_preserve_pagination_snapshots() {
    use crate::query::{BreakdownBy, Dimension, MetricKind};
    for separate_reads in [false, true] {
        let directory =
            std::env::temp_dir().join(format!("collector-group-ranking-{}", uuid::Uuid::new_v4()));
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
        store.call("group_ranking_fixture", |db| {
            db.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,context_browser_name,context_os_name,context_device_type,context_remainder,context_continent,context_country,context_city,context_page_origin,context_page_path)
                SELECT id,project,id,'video',id,'chunkify.video.view_started',1,1000,1000,'{}',0,label,label,label,
                    json_object('continent',label,'country',label,'city',label,'page_origin',label,'page_path',label),
                    label,label,label,label,label
                FROM (VALUES ('z1','default','Z'),('z2','default','Z'),('z3','default','Z'),('z4','default','Z'),
                    ('n1','default',NULL),('n2','default',NULL),('a1','default','A'),('a2','default','A'),
                    ('b1','default','B'),('b2','default','B'),('c1','default','C'),('other','other','A')) fixture(id,project,label)")?;
            Ok(())
        }).await.unwrap_or_else(|e| panic!("{}", e.message));
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
                    let page = match metrics {
                        MetricKind::Summary => store.summary("default".into(), query, false).await,
                        MetricKind::Quality => store.quality("default".into(), query).await,
                        MetricKind::Engagement => store.engagement("default".into(), query).await,
                    }
                    .unwrap_or_else(|e| panic!("{}", e.message));
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
                let page = store
                    .dimension(
                        "default".into(),
                        dimension,
                        Query {
                            limit: Some(1),
                            cursor,
                            ..Query::default()
                        },
                    )
                    .await
                    .unwrap_or_else(|e| panic!("{}", e.message));
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
        store.call("error_ranking_fixture", |db| {
            db.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,e_error_fatal)
                SELECT event_id||'-error',project_id,view_id,video_id,viewer_id,'chunkify.video.playback_error',2,2000,2000,
                    json_object('error_code',context_browser_name),0,FALSE FROM events WHERE project_id='default' AND event_name='chunkify.video.view_started';
                INSERT INTO events(event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,e_error_fatal)
                SELECT event_id||'-fatal',project_id,view_id,video_id,viewer_id,'chunkify.video.playback_error',3,3000,3000,
                    '{\"error_code\":\"A\"}',0,TRUE FROM events WHERE project_id='default' AND event_name='chunkify.video.view_started' AND context_browser_name='A'")?;
            Ok(())
        }).await.unwrap_or_else(|e| panic!("{}",e.message));
        let mut cursor = None;
        let mut errors = Vec::new();
        loop {
            let page = store
                .errors(
                    "default".into(),
                    Query {
                        limit: Some(1),
                        cursor,
                        ..Query::default()
                    },
                )
                .await
                .unwrap_or_else(|e| panic!("{}", e.message));
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

        let first = store
            .summary(
                "default".into(),
                Query {
                    group_by: Some(BreakdownBy::OsName),
                    limit: Some(1),
                    ..Query::default()
                },
                false,
            )
            .await
            .unwrap();
        let dimensions = store
            .dimension(
                "default".into(),
                Dimension::OsName,
                Query {
                    limit: Some(1),
                    ..Query::default()
                },
            )
            .await
            .unwrap();
        let error_page = store
            .errors(
                "default".into(),
                Query {
                    limit: Some(1),
                    ..Query::default()
                },
            )
            .await
            .unwrap();
        store.call("late_ranking_fixture", |db| {
            db.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,context_os_name)
                SELECT 'late-'||i,'default','late-'||i,'video','late-'||i,'chunkify.video.view_started',1,1000,4000,'{}',0,'A' FROM range(8) fixture(i);
                INSERT INTO events(event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format,e_error_fatal)
                SELECT event_id||'-error',project_id,view_id,video_id,viewer_id,'chunkify.video.playback_error',2,2000,4000,'{\"error_code\":\"A\"}',0,FALSE
                FROM events WHERE event_id LIKE 'late-%'")?;
            Ok(())
        }).await.unwrap_or_else(|e| panic!("{}",e.message));
        let remaining = store
            .summary(
                "default".into(),
                Query {
                    group_by: Some(BreakdownBy::OsName),
                    cursor: first["next_cursor"].as_str().map(str::to_owned),
                    ..Query::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(
            remaining["groups"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| json!([r["value"], r["data"]["views"]]))
                .collect::<Vec<_>>(),
            expected[1..]
        );
        let remaining = store
            .dimension(
                "default".into(),
                Dimension::OsName,
                Query {
                    cursor: dimensions["next_cursor"].as_str().map(str::to_owned),
                    ..Query::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            remaining["values"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| json!([r["value"], r["views"]]))
                .collect::<Vec<_>>(),
            expected[1..]
        );
        let remaining = store
            .errors(
                "default".into(),
                Query {
                    cursor: error_page["next_cursor"].as_str().map(str::to_owned),
                    ..Query::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            remaining["errors"]
                .as_array()
                .unwrap()
                .iter()
                .map(|r| json!([r["error_code"], r["fatal"], r["affected_views"]]))
                .collect::<Vec<_>>(),
            errors[1..]
        );
        let refreshed = store
            .summary(
                "default".into(),
                Query {
                    group_by: Some(BreakdownBy::OsName),
                    ..Query::default()
                },
                false,
            )
            .await
            .unwrap();
        assert_eq!(refreshed["groups"][0]["value"], "A");
        assert_eq!(refreshed["groups"][0]["data"]["views"], 10);
        drop(store);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}

#[tokio::test]
async fn views_sort_by_start_with_stable_ties_and_watermark() {
    for separate_reads in [false, true] {
        let directory =
            std::env::temp_dir().join(format!("collector-views-{}", uuid::Uuid::new_v4()));
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
        // UUID order, arrival order and occurrence order differ. All attempts share one viewer.
        store.call("views_fixture", |db| {
            db.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format)
                SELECT id,project,id,'video','same-viewer','chunkify.video.view_started',1,started,started,'{}',0
                FROM (VALUES ('a','default',2000),('b','default',3000),('z','default',1000),('m','default',2000),('other','other',4000)) AS fixture(id,project,started)")?;
            Ok(())
        }).await.unwrap_or_else(|e| panic!("{}", e.message));
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
            let first = store
                .views(
                    "default".into(),
                    serde_json::from_value(query.clone()).unwrap(),
                )
                .await
                .unwrap_or_else(|e| panic!("{}", e.message));
            assert_eq!(ids(&first), expected[..2]);
            let cursor = first["next_cursor"].clone();
            assert!(cursor.is_string());
            query["cursor"] = cursor;
            let second = store
                .views(
                    "default".into(),
                    serde_json::from_value(query.clone()).unwrap(),
                )
                .await
                .unwrap_or_else(|e| panic!("{}", e.message));
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
        let first = store
            .views(
                "default".into(),
                serde_json::from_value(json!({"limit":2})).unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
        store.call("late_view", |db| {
            db.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,viewer_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format)
                SELECT id,'default',id,'video','same-viewer','chunkify.video.view_started',1,started,4000,'{}',0
                FROM (VALUES ('late',1500),('newest',4000)) AS fixture(id,started)")?;
            Ok(())
        }).await.unwrap_or_else(|e| panic!("{}", e.message));
        let second = store
            .views(
                "default".into(),
                serde_json::from_value(json!({"limit":2,"cursor":first["next_cursor"]})).unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(ids(&second), ["a", "z"]);
        assert!(second["next_cursor"].is_null());
        let refreshed = store
            .views("default".into(), serde_json::from_value(json!({})).unwrap())
            .await
            .unwrap_or_else(|e| panic!("{}", e.message));
        assert_eq!(ids(&refreshed), ["newest", "b", "m", "a", "late", "z"]);
        drop(store);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
