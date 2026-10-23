use super::*;
use std::time::Duration;

#[tokio::test]
async fn pages_keep_otlp_project_scope_order_and_watermark() {
    for separate_reads in [false, true] {
        let directory =
            std::env::temp_dir().join(format!("collector-page-lookup-{}", uuid::Uuid::new_v4()));
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
        store.call("page_fixture",|c|{
            c.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format) VALUES
            ('shared','a','v','video','chunkify.video.view_started',1,0,0,'{}',0),
            ('shared','b','v','video','chunkify.video.view_started',1,0,0,'{}',0),
            ('native','a','v','video','chunkify.video.view_started',2,1,1,'{}',0),
            ('native','b','v','video','chunkify.video.view_started',2,1,1,'{}',0),
            ('last','a','v','video','chunkify.video.view_started',3,2,2,'{}',0);
            INSERT INTO event_otlp VALUES ('a','shared','{\"project\":\"a\"}'),('b','shared','{\"project\":\"b\"}'),('b','native','invalid foreign JSON'),('a','last','{\"last\":true}');")?;Ok(())
        }).await.unwrap_or_else(|e|panic!("{}",e.1));
        let first = store
            .events(
                "a".into(),
                Query {
                    limit: Some(2),
                    ..Query::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(first["events"].as_array().unwrap().len(), 2);
        assert_eq!(first["events"][0]["event"]["event_id"], "shared");
        assert_eq!(first["events"][0]["otlp"], json!({"project":"a"}));
        assert_eq!(first["events"][1]["event"]["event_id"], "native");
        assert!(first["events"][1].get("otlp").is_none());
        let newest = store
            .events(
                "a".into(),
                serde_json::from_value(json!({
                    "limit": 2, "order": "desc", "video_id": "video"
                }))
                .unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
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
                        serde_json::from_value(json!({
                            "order": order, "video_id": video, "cursor": newest_cursor
                        }))
                        .unwrap()
                    )
                    .await
                    .is_err()
            );
        }
        store.call("late_event",|c|{c.execute_batch("INSERT INTO events(event_id,project_id,view_id,video_id,event_name,sequence,occurred_at_ms,received_at_ms,event_remainder,context_format) VALUES ('late','a','v','video','chunkify.video.view_started',4,3,3,'{}',0)")?;Ok(())}).await.unwrap_or_else(|e|panic!("{}",e.1));
        let second = store
            .events(
                "a".into(),
                Query {
                    limit: Some(2),
                    cursor: Some(first["next_cursor"].as_str().unwrap().into()),
                    ..Query::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(second["events"].as_array().unwrap().len(), 1);
        assert_eq!(second["events"][0]["otlp"], json!({"last":true}));
        assert!(second["next_cursor"].is_null());
        let older = store
            .events(
                "a".into(),
                serde_json::from_value(json!({
                    "limit": 1, "order": "desc", "video_id": "video", "cursor": newest_cursor
                }))
                .unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(older["events"].as_array().unwrap().len(), 1);
        assert_eq!(older["events"][0]["event"]["event_id"], "shared");
        assert_eq!(older["events"][0]["otlp"], json!({"project":"a"}));
        assert!(older["next_cursor"].is_null());
        let refreshed = store
            .events(
                "a".into(),
                serde_json::from_value(json!({
                    "order": "desc", "limit": 1
                }))
                .unwrap(),
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(refreshed["events"][0]["event"]["event_id"], "late");
        let filtered = store.events("a".into(), serde_json::from_value(json!({
            "order": "desc", "from": "1970-01-01T00:00:00.001Z", "to": "1970-01-01T00:00:00.003Z"
        })).unwrap()).await.unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(filtered["events"].as_array().unwrap().len(), 2);
        assert_eq!(filtered["events"][0]["event"]["event_id"], "last");
        assert_eq!(filtered["events"][1]["event"]["event_id"], "native");

        let other = store
            .events(
                "b".into(),
                Query {
                    limit: Some(1),
                    ..Query::default()
                },
            )
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert_eq!(other["events"][0]["otlp"], json!({"project":"b"}));
        let empty = store
            .events("missing".into(), Query::default())
            .await
            .unwrap_or_else(|e| panic!("{}", e.1));
        assert!(empty["events"].as_array().unwrap().is_empty());
        drop(store);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
    }
}
