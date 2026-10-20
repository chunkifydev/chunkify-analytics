use super::*;
use serde_json::json;

fn row(project: &str, id: &str, marker: &str, otlp: bool) -> Row {
    Row::prepare(&IncomingEvent {
        event: json!({"event_id":id,"view_id":"view","video_id":"video","viewer_id":null,
            "event_name":"chunkify.video.view_started","sequence":1,"occurred_at":"2026-10-20T00:00:00Z","marker":marker}),
        otlp: otlp.then(||json!({"marker":marker})),
    },project,Some("test-key"),None,123).unwrap_or_else(|e|panic!("{}",e.1))
}
fn request(rows: Vec<Row>) -> WriteRequest {
    let (reply, _) = oneshot::channel();
    WriteRequest {
        rows,
        bytes: 1,
        queued: Instant::now(),
        preparation: Duration::ZERO,
        reply,
        _permit: Arc::new(Semaphore::new(1)).try_acquire_owned().unwrap(),
        project_permit: None,
    }
}

#[test]
fn bulk_lookup_preserves_first_delivery_projects_and_parameterized_ids() {
    let mut c = Connection::open_in_memory().unwrap();
    crate::storage::initialize(&mut c).unwrap();
    crate::storage::create_stage(&c).unwrap();
    let mut profile = Profile::new(false, &c, "test").unwrap();
    let id = "same'); DELETE FROM events; --";
    let batch = vec![
        request(vec![
            row("a", id, "first", true),
            row("a", id, "duplicate", true),
        ]),
        request(vec![
            row("b", id, "other-project", true),
            row("a", "native", "native", false),
        ]),
    ];
    assert_eq!(
        write_batch(&mut c, &batch, true, &mut profile).unwrap_or_else(|e| panic!("{}", e.1)),
        vec![1, 2]
    );
    let replay = vec![
        request(vec![
            row("a", id, "replacement", true),
            row("b", id, "replacement", true),
            row("a", "native", "late-otlp", true),
        ]),
        request(vec![row("a", "new", "new", true)]),
    ];
    assert_eq!(
        write_batch(&mut c, &replay, true, &mut profile).unwrap_or_else(|e| panic!("{}", e.1)),
        vec![0, 1]
    );
    let values = c
        .prepare("SELECT project_id,event_id,event_remainder FROM events ORDER BY ingest_id")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<duckdb::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(
        values
            .iter()
            .map(|v| (v.0.as_str(), v.1.as_str()))
            .collect::<Vec<_>>(),
        vec![("a", id), ("b", id), ("a", "native"), ("a", "new")]
    );
    for (value, marker) in values
        .iter()
        .zip(["first", "other-project", "native", "new"])
    {
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&value.2).unwrap()["marker"],
            marker
        );
    }
    let otlp = c
        .prepare("SELECT project_id,event_id,payload FROM event_otlp ORDER BY project_id,event_id")
        .unwrap()
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })
        .unwrap()
        .collect::<duckdb::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(otlp.len(), 3);
    assert!(otlp.iter().all(|r| r.1 != "native"));
    assert!(
        otlp.iter()
            .any(|r| r.0 == "a" && r.1 == id && r.2 == json!({"marker":"first"}).to_string())
    );
    assert!(
        otlp.iter().any(|r| r.0 == "b"
            && r.1 == id
            && r.2 == json!({"marker":"other-project"}).to_string())
    );
    assert_eq!(
        write_batch(&mut c, &[request(vec![])], true, &mut profile)
            .unwrap_or_else(|e| panic!("{}", e.1)),
        vec![0]
    );
}

#[test]
fn bulk_lookup_rolls_back_failed_batches_and_reuses_the_stage() {
    let mut c = Connection::open_in_memory().unwrap();
    crate::storage::initialize(&mut c).unwrap();
    crate::storage::create_stage(&c).unwrap();
    let mut profile = Profile::new(false, &c, "test").unwrap();
    let mut invalid = row("a", "invalid", "invalid", true);
    invalid.values[11] = Value::BigInt(2);
    assert!(
        write_batch(
            &mut c,
            &[request(vec![row("a", "valid", "valid", true), invalid])],
            true,
            &mut profile
        )
        .is_err()
    );
    for table in ["events", "event_otlp", "collector_write_stage"] {
        assert_eq!(
            c.query_row(&format!("SELECT count(*) FROM {table}"), [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
    assert_eq!(
        write_batch(
            &mut c,
            &[request(vec![row("a", "valid", "retry", true)])],
            true,
            &mut profile
        )
        .unwrap_or_else(|e| panic!("{}", e.1)),
        vec![1]
    );
}
