use super::*;
use std::path::PathBuf;

struct TestDirectory(PathBuf);
impl Drop for TestDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Fixture {
    database: Arc<Mutex<Database>>,
    writer: Option<Writer>,
    worker: Option<thread::JoinHandle<duckdb::Result<()>>>,
    reader: duckdb::Connection,
    batch: BatchConfig,
    request_bytes: usize,
    _directory: TestDirectory,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        drop(self.writer.take());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn request(id: &str, project: &str, received: i64) -> StoredRequest {
    let mut request = StoredRequest {
        project_id: project.into(),
        ingest_key_id: Some("test-key".into()),
        events: vec![IncomingEvent {
            event: serde_json::json!({
                "event_id": id, "view_id": "view", "video_id": "video",
                "viewer_id": null, "event_name": "chunkify.video.view_started",
                "sequence": 1, "occurred_at": "2026-10-20T00:00:00Z",
                "custom": received,
            }),
            otlp: Some(serde_json::json!({"original": received})),
        }],
        context: Some(serde_json::json!({"client_ip": "127.0.0.1"})),
        received,
        prepared_bytes: 0,
        prepared_version: Writer::PREPARED_VERSION,
    };
    request.prepared_bytes = Writer::prepared_bytes(&request).map_err(|e| e.1).unwrap();
    request
}

impl Fixture {
    fn new(requests: &[StoredRequest], batches_in_memory: usize) -> Self {
        let directory = TestDirectory(
            std::env::temp_dir().join(format!("collector-pipeline-{}", uuid::Uuid::new_v4())),
        );
        std::fs::create_dir(&directory.0).unwrap();
        let path = directory.0.join("queue.sqlite");
        let database = Database::open(&path, 1048576).unwrap();
        for request in requests {
            database.connection.execute(
                "INSERT INTO pending(received_ms, records, payload, project_id) VALUES(?, ?, ?, ?)",
                params![request.received, request.events.len() as i64, serde_json::to_vec(request).unwrap(), request.project_id],
            ).unwrap();
        }
        drop(database);
        let database = Arc::new(Mutex::new(Database::open(&path, 1048576).unwrap()));
        let request_bytes = requests.iter().map(|r| r.prepared_bytes).max().unwrap();
        let batch = BatchConfig {
            max_events: 1,
            max_bytes: request_bytes,
            delay: Duration::ZERO,
            queue_bytes: request_bytes * batches_in_memory,
            queue_requests: 8,
            bulk: true,
        };
        let mut connection = duckdb::Connection::open(directory.0.join("events.duckdb")).unwrap();
        crate::storage::initialize(&mut connection).unwrap();
        let reader = connection.try_clone().unwrap();
        let (writer, worker) = Writer::start(connection, batch.clone(), false).unwrap();
        Self {
            database,
            writer: Some(writer),
            worker: Some(worker),
            reader,
            batch,
            request_bytes,
            _directory: directory,
        }
    }

    fn consume(&self) -> thread::JoinHandle<Result<()>> {
        self.consume_with_pipeline(true)
    }

    fn consume_with_pipeline(&self, prepare_ahead: bool) -> thread::JoinHandle<Result<()>> {
        let database = self.database.clone();
        let writer = self.writer.as_ref().unwrap().clone();
        let batch = self.batch.clone();
        thread::spawn(move || {
            consume(
                &database,
                &AtomicBool::new(true),
                &AtomicBool::new(false),
                &batch,
                &writer,
                prepare_ahead,
            )
        })
    }

    fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut duckdb::Connection) -> std::result::Result<T, ApiError>
        + Send
        + 'static,
    ) -> T {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(
                self.writer
                    .as_ref()
                    .unwrap()
                    .call("pipeline_test", operation),
            )
            .map_err(|e| e.1)
            .unwrap()
    }

    fn count(&self) -> i64 {
        self.reader
            .query_row("SELECT count(*) FROM events", [], |r| r.get(0))
            .unwrap()
    }

    fn assert_drained(&self) {
        let db = self.database.lock().unwrap();
        assert_eq!((db.requests, db.events, db.bytes), (0, 0, 0));
        assert!(db.project_bytes.values().all(|bytes| *bytes == 0));
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM pending", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

// Block the real writer before its next transaction. This gives the consumer a
// deterministic window to prepare ahead, without timing actual disk writes.
fn block_writer(fixture: &Fixture, fail: bool) -> (mpsc::Sender<()>, thread::JoinHandle<()>) {
    let (entered, ready) = mpsc::channel();
    let (release, wait) = mpsc::channel();
    let writer = fixture.writer.as_ref().unwrap().clone();
    let worker = thread::spawn(move || {
        tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(writer.call("pipeline_test_block", move |connection| {
                entered.send(()).unwrap();
                let _ = wait.recv_timeout(Duration::from_secs(5));
                if fail {
                    connection.execute_batch("DROP TABLE collector_write_stage")?;
                }
                Ok(())
            }))
            .map_err(|e| e.1)
            .unwrap();
    });
    ready.recv_timeout(Duration::from_secs(5)).unwrap();
    (release, worker)
}

fn wait_for_two_batches(fixture: &Fixture) -> usize {
    let deadline = Instant::now() + Duration::from_secs(2);
    let writer = fixture.writer.as_ref().unwrap();
    while writer.reserved_replay_bytes() < fixture.request_bytes * 2 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    writer.reserved_replay_bytes()
}

#[test]
fn prepares_one_batch_ahead_while_the_writer_is_blocked() {
    let requests = [
        request("event-1", "project", 111),
        request("event-2", "project", 222),
        request("event-3", "project", 333),
    ];
    let fixture = Fixture::new(&requests, 4);
    let (release, blocked) = block_writer(&fixture, false);
    let consumer = fixture.consume();
    let reserved = wait_for_two_batches(&fixture);
    let pending = fixture.database.lock().unwrap().requests;
    release.send(()).unwrap();
    blocked.join().unwrap();
    consumer.join().unwrap().unwrap();
    assert_eq!(
        reserved,
        fixture.request_bytes * 2,
        "prepare exactly one batch ahead, never all three"
    );
    assert_eq!(pending, 3, "no queue rows may be deleted before commit");
    assert_eq!(fixture.count(), 3);
    fixture.assert_drained();
}

#[test]
fn a_one_batch_memory_budget_falls_back_to_serial_replay() {
    let requests = [
        request("event-1", "project", 111),
        request("event-2", "project", 222),
        request("event-3", "project", 333),
    ];
    let fixture = Fixture::new(&requests, 1);
    fixture.consume().join().unwrap().unwrap();
    assert_eq!(fixture.count(), 3);
    fixture.assert_drained();
    assert_eq!(fixture.writer.as_ref().unwrap().reserved_replay_bytes(), 0);
}

#[test]
fn failed_write_retains_both_batches_and_recovers() {
    let requests = [
        request("event-1", "project", 111),
        request("event-2", "project", 222),
        request("event-3", "project", 333),
    ];
    let fixture = Fixture::new(&requests, 4);
    let (release, blocked) = block_writer(&fixture, true);
    let consumer = fixture.consume();
    let reserved = wait_for_two_batches(&fixture);
    release.send(()).unwrap();
    blocked.join().unwrap();
    assert!(consumer.join().unwrap().is_err());
    assert_eq!(reserved, fixture.request_bytes * 2);
    assert_eq!(fixture.count(), 0);
    assert_eq!(fixture.database.lock().unwrap().requests, 3);
    assert_eq!(fixture.writer.as_ref().unwrap().reserved_replay_bytes(), 0);
    fixture.call(|connection| {
        crate::storage::create_stage(connection)?;
        Ok(())
    });
    fixture.consume().join().unwrap().unwrap();
    assert_eq!(fixture.count(), 3);
    fixture.assert_drained();
}

#[test]
fn replay_preserves_first_delivery_context_and_project_deduplication() {
    let first = request("shared", "project-a", 111);
    let duplicate = request("shared", "project-a", 222);
    let other = request("shared", "project-b", 333);
    let fixture = Fixture::new(&[first, duplicate, other], 4);
    fixture.consume().join().unwrap().unwrap();
    let rows: Vec<(String, i64, String)> = fixture.reader.prepare(
        "SELECT project_id, received_at_ms, json_extract_string(context_remainder, '$.client_ip') FROM events ORDER BY project_id"
    ).unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap().collect::<duckdb::Result<_>>().unwrap();
    assert_eq!(
        rows,
        vec![
            ("project-a".into(), 111, "127.0.0.1".into()),
            ("project-b".into(), 333, "127.0.0.1".into())
        ]
    );
    let payloads: Vec<String> = fixture
        .reader
        .prepare("SELECT payload FROM event_otlp ORDER BY project_id")
        .unwrap()
        .query_map([], |row| row.get(0))
        .unwrap()
        .collect::<duckdb::Result<_>>()
        .unwrap();
    assert_eq!(payloads, vec![r#"{"original":111}"#, r#"{"original":333}"#]);
    fixture.assert_drained();
}

#[test]
fn failed_queue_deletion_replays_an_already_committed_batch_once() {
    let requests = [
        request("event-1", "project", 111),
        request("event-2", "project", 222),
        request("event-3", "project", 333),
    ];
    let fixture = Fixture::new(&requests, 4);
    fixture.database.lock().unwrap().connection.execute_batch(
        "CREATE TRIGGER fail_delete BEFORE DELETE ON pending BEGIN SELECT RAISE(ABORT, 'test delete failure'); END;"
    ).unwrap();
    assert!(fixture.consume().join().unwrap().is_err());
    assert_eq!(
        fixture.count(),
        1,
        "the first batch committed, but its successor must not be submitted"
    );
    assert_eq!(fixture.database.lock().unwrap().requests, 3);
    assert_eq!(fixture.writer.as_ref().unwrap().reserved_replay_bytes(), 0);
    fixture
        .database
        .lock()
        .unwrap()
        .connection
        .execute_batch("DROP TRIGGER fail_delete")
        .unwrap();
    fixture.consume().join().unwrap().unwrap();
    assert_eq!(
        fixture.count(),
        3,
        "replaying the committed first batch must not duplicate it"
    );
    fixture.assert_drained();
}

#[test]
fn legacy_sizing_waits_for_the_preceding_commit_with_a_one_batch_budget() {
    let first = request("event-1", "project", 111);
    let mut legacy = request("event-2", "project", 222);
    legacy.prepared_version = 0;
    legacy.prepared_bytes = 1;
    let fixture = Fixture::new(&[first, legacy], 1);
    let (release, blocked) = block_writer(&fixture, false);
    let writer = fixture.writer.as_ref().unwrap();
    let prepared = writer
        .prepare_replay(&[request("event-1", "project", 111)])
        .map_err(|e| e.1)
        .unwrap()
        .unwrap();
    let completion = writer.submit_replay(prepared).map_err(|e| e.1).unwrap();
    assert_eq!(writer.reserved_replay_bytes(), fixture.batch.queue_bytes);
    let deferred = matches!(
        fixture
            .database
            .lock()
            .unwrap()
            .next(1, &fixture.batch)
            .unwrap(),
        NextBatch::AwaitPreviousCommit
    );
    release.send(()).unwrap();
    blocked.join().unwrap();
    completion.wait().map_err(|e| e.1).unwrap();
    assert!(
        deferred,
        "legacy sizing must wait before allocating prepared rows"
    );
    assert_eq!(fixture.database.lock().unwrap().requests, 2);
    // The producer is already closed. A deferred legacy read must still drain
    // the successor, and recovery deduplicates the already committed first row.
    fixture.consume().join().unwrap().unwrap();
    assert_eq!(fixture.count(), 2);
    fixture.assert_drained();
    assert_eq!(writer.reserved_replay_bytes(), 0);
}

#[test]
fn disabled_pipeline_keeps_replay_sequential() {
    let requests = [
        request("event-1", "project", 111),
        request("event-2", "project", 222),
        request("event-3", "project", 333),
    ];
    let fixture = Fixture::new(&requests, 4);
    let (release, blocked) = block_writer(&fixture, false);
    let consumer = fixture.consume_with_pipeline(false);
    let reserved = wait_for_two_batches(&fixture);
    release.send(()).unwrap();
    blocked.join().unwrap();
    consumer.join().unwrap().unwrap();
    assert_eq!(
        reserved, fixture.request_bytes,
        "disabled pipeline must not prepare ahead"
    );
    assert_eq!(fixture.count(), 3);
    fixture.assert_drained();
}
