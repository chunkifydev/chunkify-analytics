use super::*;
use std::time::Duration;

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
