//! Experimental, opt-in SQLite queue. HTTP success follows a FULL synchronous
//! SQLite commit. DuckDB replay and queue deletion are separate transactions;
//! replay is at least once and relies on DuckDB's event_id deduplication.
use crate::{
    api::ApiError,
    context::RequestContext,
    event::IncomingEvent,
    writer::{BatchConfig, Writer},
};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    env,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

type Result<T> = std::result::Result<T, String>;

#[derive(Clone)]
pub struct SpoolConfig {
    pub max_bytes: usize,
}
impl SpoolConfig {
    pub fn load() -> std::result::Result<Option<Self>, Box<dyn std::error::Error>> {
        let enabled: bool = env::var("COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE")
            .unwrap_or_else(|_| "false".into())
            .parse()?;
        if !enabled {
            return Ok(None);
        }
        let max_bytes = env::var("COLLECTOR_SPOOL_MAX_BYTES")
            .unwrap_or_else(|_| "67108864".into())
            .parse()?;
        if !(4096..=268435456).contains(&max_bytes) {
            return Err("COLLECTOR_SPOOL_MAX_BYTES must be between 4096 and 268435456".into());
        }
        Ok(Some(Self { max_bytes }))
    }
}

#[derive(Serialize, Deserialize)]
pub struct StoredRequest {
    #[serde(default = "crate::projects::default_project")]
    pub project_id: String,
    #[serde(default)]
    pub ingest_key_id: Option<String>,
    pub events: Vec<IncomingEvent>,
    pub context: Option<serde_json::Value>,
    pub received: i64,
    pub prepared_bytes: usize,
    #[serde(default)]
    pub prepared_version: u32,
}

struct Pending {
    project_id: String,
    project_limit: usize,
    project_permit: Option<crate::projects::Admission>,
    payload: Vec<u8>,
    records: usize,
    received: i64,
    reply: oneshot::Sender<std::result::Result<(), ApiError>>,
    _permit: OwnedSemaphorePermit,
}

#[derive(Serialize)]
pub struct Stats {
    pub pending_requests: usize,
    pub pending_events: usize,
    pub pending_bytes: usize,
    pub oldest_age_ms: i64,
    pub failed: bool,
}

struct Database {
    connection: Connection,
    requests: usize,
    events: usize,
    bytes: usize,
    max_bytes: usize,
    project_bytes: HashMap<String, usize>,
}
impl Database {
    fn open(path: &Path, max_bytes: usize) -> Result<Self> {
        let connection = Connection::open(path).map_err(|e| e.to_string())?;
        connection
            .execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;
            PRAGMA fullfsync=ON; PRAGMA checkpoint_fullfsync=ON;
            PRAGMA wal_autocheckpoint=1000; PRAGMA journal_size_limit=8388608;
            CREATE TABLE IF NOT EXISTS pending (
                id INTEGER PRIMARY KEY AUTOINCREMENT, received_ms INTEGER NOT NULL,
                records INTEGER NOT NULL, payload BLOB NOT NULL);",
            )
            .map_err(|e| e.to_string())?;
        let has_project: bool = connection
            .query_row(
                "SELECT count(*) > 0 FROM pragma_table_info('pending') WHERE name = 'project_id'",
                [],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if !has_project {
            connection
                .execute_batch(
                    "ALTER TABLE pending ADD COLUMN project_id TEXT NOT NULL DEFAULT 'default'",
                )
                .map_err(|e| e.to_string())?;
        }
        connection
            .execute_batch(
                "CREATE INDEX IF NOT EXISTS pending_project ON pending(project_id, received_ms)",
            )
            .map_err(|e| e.to_string())?;
        let project_bytes = {
            let mut statement = connection.prepare("SELECT project_id, coalesce(sum(length(payload)),0) FROM pending GROUP BY project_id").map_err(|e| e.to_string())?;
            statement
                .query_map([], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
                })
                .map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<HashMap<_, _>>>()
                .map_err(|e| e.to_string())?
        };
        // Bound database allocation as well as logical payloads. SQLite reuses
        // freed pages; the WAL has its own automatic checkpoint threshold.
        let pages = (max_bytes.saturating_mul(2) + 8388608).div_ceil(4096);
        connection
            .pragma_update(None, "max_page_count", pages as i64)
            .map_err(|e| e.to_string())?;
        let (requests, events, bytes) = connection.query_row(
            "SELECT count(*), coalesce(sum(records),0), coalesce(sum(length(payload)),0) FROM pending", [],
            |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize, r.get::<_, i64>(2)? as usize))).map_err(|e| e.to_string())?;
        Ok(Self {
            connection,
            requests,
            events,
            bytes,
            max_bytes,
            project_bytes,
        })
    }
    fn append(&mut self, batch: &[Pending]) -> Result<()> {
        let tx = self.connection.transaction().map_err(|e| e.to_string())?;
        {
            let mut statement = tx
                .prepare_cached(
                    "INSERT INTO pending (received_ms, records, payload, project_id) VALUES (?, ?, ?, ?)",
                )
                .map_err(|e| e.to_string())?;
            for request in batch {
                statement
                    .execute(params![
                        request.received,
                        request.records as i64,
                        request.payload,
                        request.project_id
                    ])
                    .map_err(|e| e.to_string())?;
            }
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.requests += batch.len();
        self.events += batch.iter().map(|r| r.records).sum::<usize>();
        self.bytes += batch.iter().map(|r| r.payload.len()).sum::<usize>();
        for request in batch {
            *self
                .project_bytes
                .entry(request.project_id.clone())
                .or_default() += request.payload.len();
        }
        Ok(())
    }
    fn next(&self, batch: &BatchConfig) -> Result<Vec<(i64, StoredRequest, usize)>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, payload, records, project_id FROM pending ORDER BY id LIMIT ?")
            .map_err(|e| e.to_string())?;
        let mut rows = statement
            .query([batch.max_events as i64])
            .map_err(|e| e.to_string())?;
        let mut result = Vec::new();
        let mut bytes = 0;
        let mut events = 0;
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let payload: Vec<u8> = row.get(1).map_err(|e| e.to_string())?;
            let count = row.get::<_, i64>(2).map_err(|e| e.to_string())? as usize;
            if !result.is_empty()
                && (events + count > batch.max_events || bytes + payload.len() > batch.max_bytes)
            {
                break;
            }
            let mut stored: StoredRequest =
                serde_json::from_slice(&payload).map_err(|e| e.to_string())?;
            let project: String = row.get(3).map_err(|e| e.to_string())?;
            if stored.project_id != project {
                return Err("Queued project ownership mismatch".into());
            }
            if stored.prepared_version != Writer::PREPARED_VERSION {
                stored.prepared_bytes = Writer::prepared_bytes(&stored).map_err(|e| e.1)?;
                stored.prepared_version = Writer::PREPARED_VERSION;
            }
            if stored.prepared_bytes > batch.max_bytes {
                return Err("Queued request exceeds the current prepared write limit; increase COLLECTOR_BATCH_MAX_BYTES to recover it".into());
            }
            if !result.is_empty() && bytes + stored.prepared_bytes > batch.max_bytes {
                break;
            }
            bytes += stored.prepared_bytes;
            events += count;
            result.push((
                row.get(0).map_err(|e| e.to_string())?,
                stored,
                payload.len(),
            ));
        }
        Ok(result)
    }
    fn complete(
        &mut self,
        through: i64,
        requests: usize,
        events: usize,
        bytes: usize,
    ) -> Result<()> {
        let removed = {
            let mut statement = self.connection.prepare("SELECT project_id, sum(length(payload)) FROM pending WHERE id <= ? GROUP BY project_id").map_err(|e| e.to_string())?;
            statement
                .query_map([through], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
                })
                .map_err(|e| e.to_string())?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(|e| e.to_string())?
        };
        self.connection
            .execute("DELETE FROM pending WHERE id <= ?", [through])
            .map_err(|e| e.to_string())?;
        for (project, bytes) in removed {
            if let Some(pending) = self.project_bytes.get_mut(&project) {
                *pending -= bytes;
            }
        }
        self.requests -= requests;
        self.events -= events;
        self.bytes -= bytes;
        Ok(())
    }
}

#[derive(Clone)]
pub struct Spool {
    sender: mpsc::SyncSender<Pending>,
    database: Arc<Mutex<Database>>,
    failed: Arc<AtomicBool>,
    budget: Arc<Semaphore>,
    batch: BatchConfig,
}

pub struct Workers {
    producer: thread::JoinHandle<Result<()>>,
    consumer: thread::JoinHandle<Result<()>>,
}
impl Workers {
    pub fn join(self) -> thread::Result<Result<()>> {
        let producer = self.producer.join();
        let consumer = self.consumer.join();
        Ok(producer?.and(consumer?))
    }
}

impl Spool {
    pub fn require_empty(path: &Path) -> Result<()> {
        if !path.exists() {
            return Ok(());
        }
        let connection =
            Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| e.to_string())?;
        let count: i64 = connection
            .query_row("SELECT count(*) FROM pending", [], |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if count > 0 {
            return Err("Pending durable events exist. Enable COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE to recover them first.".into());
        }
        Ok(())
    }
    pub fn start(
        path: &Path,
        config: SpoolConfig,
        batch: BatchConfig,
        writer: Writer,
    ) -> Result<(Self, Workers)> {
        let database = Arc::new(Mutex::new(Database::open(path, config.max_bytes)?));
        let failed = Arc::new(AtomicBool::new(false));
        let closed = Arc::new(AtomicBool::new(false));
        let (sender, receiver) = mpsc::sync_channel(batch.queue_requests);
        let spool = Self {
            sender,
            database: database.clone(),
            failed: failed.clone(),
            budget: Arc::new(Semaphore::new(batch.queue_bytes)),
            batch: batch.clone(),
        };
        let producer = {
            let database = database.clone();
            let failed = failed.clone();
            let closed = closed.clone();
            thread::spawn(move || {
                let result = produce(receiver, &database, &failed, batch.max_bytes);
                if result.is_err() {
                    failed.store(true, Ordering::Release);
                    eprintln!("Durable queue producer failed; pending events retained");
                }
                closed.store(true, Ordering::Release);
                result
            })
        };
        let consumer = thread::spawn(move || {
            let result = consume(&database, &closed, &failed, &batch, &writer);
            if result.is_err() {
                failed.store(true, Ordering::Release);
                eprintln!("Durable queue consumer failed; pending events retained");
            }
            result
        });
        Ok((spool, Workers { producer, consumer }))
    }
    pub async fn insert(
        &self,
        project: &crate::projects::Access,
        key_id: &str,
        events: Vec<IncomingEvent>,
        context: Option<RequestContext>,
    ) -> std::result::Result<(), ApiError> {
        if self.failed.load(Ordering::Acquire) {
            return Err(ApiError::unavailable());
        }
        let records = events.len();
        if records > self.batch.max_events {
            return Err(ApiError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds queue batch limit".into(),
            ));
        }
        let received = chrono::Utc::now().timestamp_millis();
        let mut stored = StoredRequest {
            project_id: project.id.clone(),
            ingest_key_id: Some(key_id.into()),
            events,
            context: context
                .map(serde_json::to_value)
                .transpose()
                .map_err(|_| ApiError::internal())?,
            received,
            prepared_bytes: 0,
            prepared_version: Writer::PREPARED_VERSION,
        };
        stored.prepared_bytes = Writer::prepared_bytes(&stored)?;
        if stored.prepared_bytes > self.batch.max_bytes {
            return Err(ApiError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds prepared write limit".into(),
            ));
        }
        let payload = serde_json::to_vec(&stored).map_err(|_| ApiError::internal())?;
        if payload.len() > self.batch.max_bytes {
            return Err(ApiError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds queue byte limit".into(),
            ));
        }
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(payload.len() as u32)
            .map_err(|_| ApiError::unavailable())?;
        if payload.len() > project.queue_bytes || payload.len() > project.spool_bytes {
            return Err(ApiError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds project queue limit".into(),
            ));
        }
        let project_permit = project.admit(payload.len())?;
        let (reply, response) = oneshot::channel();
        self.sender
            .try_send(Pending {
                project_id: project.id.clone(),
                project_limit: project.spool_bytes,
                project_permit: Some(project_permit),
                payload,
                records,
                received,
                reply,
                _permit: permit,
            })
            .map_err(|_| ApiError::unavailable())?;
        response.await.map_err(|_| ApiError::unavailable())?
    }
    pub fn ensure_healthy(&self) -> std::result::Result<(), ApiError> {
        if self.failed.load(Ordering::Acquire) {
            Err(ApiError::unavailable())
        } else {
            Ok(())
        }
    }
    pub async fn stats(&self, project: Option<String>) -> std::result::Result<Stats, ApiError> {
        let database = self.database.clone();
        let failed = self.failed.clone();
        tokio::task::spawn_blocking(move || {
            let db = database.lock().map_err(|_| ApiError::internal())?;
            let (requests, events, bytes, oldest) = db.connection.query_row(
                "SELECT count(*), coalesce(sum(records),0), coalesce(sum(length(payload)),0), min(received_ms) FROM pending WHERE (? IS NULL OR project_id = ?)",
                params![project, project], |r| Ok((r.get::<_, i64>(0)? as usize, r.get::<_, i64>(1)? as usize, r.get::<_, i64>(2)? as usize, r.get::<_, Option<i64>>(3)?)))
                .map_err(|_| ApiError::internal())?;
            Ok(Stats {
                pending_requests: requests,
                pending_events: events,
                pending_bytes: bytes,
                oldest_age_ms: oldest
                    .map_or(0, |t| (chrono::Utc::now().timestamp_millis() - t).max(0)),
                failed: failed.load(Ordering::Acquire),
            })
        })
        .await
        .map_err(|_| ApiError::internal())?
    }
}

fn produce(
    receiver: mpsc::Receiver<Pending>,
    database: &Mutex<Database>,
    failed: &AtomicBool,
    max_bytes: usize,
) -> Result<()> {
    while let Ok(first) = receiver.recv() {
        let mut batch = vec![first];
        let mut bytes = batch[0].payload.len();
        // Group durable acknowledgements to amortize sync, without waiting on
        // DuckDB. A bounded one-millisecond window also serves sparse traffic.
        let deadline = Instant::now() + Duration::from_millis(1);
        while batch.len() < 100 && bytes < max_bytes && Instant::now() < deadline {
            match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
                Ok(request) => {
                    bytes += request.payload.len();
                    batch.push(request);
                }
                Err(_) => break,
            }
        }
        let mut db = database.lock().map_err(|_| "Queue mutex poisoned")?;
        let mut admitted = Vec::new();
        let mut reserved = db.bytes;
        let mut project_reserved = HashMap::new();
        for request in batch {
            let used = project_reserved
                .entry(request.project_id.clone())
                .or_insert_with(|| {
                    db.project_bytes
                        .get(&request.project_id)
                        .copied()
                        .unwrap_or(0)
                });
            if *used + request.payload.len() > request.project_limit {
                let _ = request.reply.send(Err(ApiError::unavailable()));
                continue;
            }
            if failed.load(Ordering::Acquire) || reserved + request.payload.len() > db.max_bytes {
                let _ = request.reply.send(Err(ApiError::unavailable()));
            } else {
                reserved += request.payload.len();
                *used += request.payload.len();
                admitted.push(request);
            }
        }
        if admitted.is_empty() {
            continue;
        }
        let result = db.append(&admitted);
        drop(db);
        for request in admitted {
            drop(request.payload);
            drop(request._permit);
            drop(request.project_permit);
            let _ = request.reply.send(if result.is_ok() {
                Ok(())
            } else {
                Err(ApiError::internal())
            });
        }
        result?;
    }
    Ok(())
}

fn consume(
    database: &Mutex<Database>,
    closed: &AtomicBool,
    failed: &AtomicBool,
    batch: &BatchConfig,
    writer: &Writer,
) -> Result<()> {
    loop {
        let pending = database
            .lock()
            .map_err(|_| "Queue mutex poisoned")?
            .next(batch)?;
        if pending.is_empty() {
            if closed.load(Ordering::Acquire) {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(2));
            continue;
        }
        if failed.load(Ordering::Acquire) {
            return Err("Queue failed; pending events retained".into());
        }
        let through = pending.last().unwrap().0;
        let requests = pending.len();
        let events = pending.iter().map(|(_, r, _)| r.events.len()).sum();
        let bytes = pending.iter().map(|(_, _, size)| size).sum();
        // No queue lock or SQLite transaction remains open during DuckDB work.
        writer
            .replay(pending.into_iter().map(|(_, r, _)| r).collect())
            .map_err(|_| "DuckDB replay failed; pending events retained")?;
        database
            .lock()
            .map_err(|_| "Queue mutex poisoned")?
            .complete(through, requests, events, bytes)?;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn replay_recalculates_legacy_prepared_sizes_before_forming_a_batch() {
        let directory =
            std::env::temp_dir().join(format!("collector-spool-upgrade-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let database = Database::open(&directory.join("queue.sqlite"), 1048576).unwrap();
        let payload = serde_json::json!({"events": [{"event": {
            "event_id": "event", "view_id": "view", "video_id": "video", "viewer_id": null,
            "event_name": "chunkify.video.view_started", "sequence": 1,
            "occurred_at": "2026-10-19T00:00:00Z"
        }, "otlp": null}], "context": null, "received": 123, "prepared_bytes": 1});
        let stored: StoredRequest = serde_json::from_value(payload.clone()).unwrap();
        let actual = Writer::prepared_bytes(&stored).map_err(|e| e.1).unwrap();
        let encoded = serde_json::to_vec(&payload).unwrap();
        assert!(encoded.len() < actual);
        for _ in 0..3 {
            database
                .connection
                .execute(
                    "INSERT INTO pending(received_ms, records, payload) VALUES(123, 1, ?)",
                    [&encoded],
                )
                .unwrap();
        }
        let batch = BatchConfig {
            max_events: 100,
            max_bytes: actual + encoded.len(),
            delay: Duration::ZERO,
            queue_bytes: actual + encoded.len(),
            queue_requests: 8,
            bulk: true,
        };
        let pending = database.next(&batch).unwrap();
        assert_eq!(
            pending.len(),
            1,
            "legacy estimate must not admit all three requests into one batch"
        );
        assert_eq!(pending[0].1.prepared_bytes, actual);
        assert_eq!(pending[0].1.prepared_version, Writer::PREPARED_VERSION);
        assert_eq!(pending[0].1.received, 123);
        let smaller = BatchConfig {
            max_bytes: actual - 1,
            ..batch
        };
        assert!(
            database.next(&smaller).is_err(),
            "an oversized first request must remain queued"
        );
        assert_eq!(
            database
                .connection
                .query_row("SELECT count(*) FROM pending", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            3
        );
        drop(database);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[tokio::test]
    async fn failed_sqlite_commit_never_acknowledges() {
        let directory =
            std::env::temp_dir().join(format!("chunkify-spool-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let db = Database::open(&directory.join("queue.sqlite"), 4096).unwrap();
        db.connection.execute_batch("PRAGMA foreign_keys=ON;
                CREATE TABLE test_parent (id INTEGER PRIMARY KEY);
                CREATE TABLE test_child (parent INTEGER REFERENCES test_parent(id) DEFERRABLE INITIALLY DEFERRED);
                CREATE TRIGGER fail_at_commit AFTER INSERT ON pending BEGIN INSERT INTO test_child VALUES (999); END;").unwrap();
        let (reply, response) = oneshot::channel();
        let request = Pending {
            project_id: "default".into(),
            project_limit: 4096,
            project_permit: None,
            payload: b"test".to_vec(),
            records: 1,
            received: 0,
            reply,
            _permit: Arc::new(Semaphore::new(4))
                .try_acquire_many_owned(4)
                .unwrap(),
        };
        let database = Arc::new(Mutex::new(db));
        let worker_db = database.clone();
        let (sender, receiver) = mpsc::sync_channel(1);
        let worker =
            thread::spawn(move || produce(receiver, &worker_db, &AtomicBool::new(false), 4096));
        assert!(sender.send(request).is_ok());
        drop(sender);
        assert!(
            matches!(response.await, Ok(Err(_))),
            "failed durable write must return an error, never success"
        );
        assert!(
            worker
                .join()
                .unwrap()
                .unwrap_err()
                .contains("FOREIGN KEY constraint failed")
        );
        {
            let db = database.lock().unwrap();
            assert_eq!(db.requests, 0);
            assert_eq!(
                db.connection
                    .query_row("SELECT count(*) FROM pending", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0
            );
        }
        drop(database);
        std::fs::remove_dir_all(directory).unwrap();
    }
}
