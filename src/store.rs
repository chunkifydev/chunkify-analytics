use crate::{
    api::ApiError,
    event::{self, IncomingEvent},
    query::{Cursor, Query},
};
use duckdb::{Connection, params, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};
use std::{path::Path, thread};
use tokio::sync::{mpsc, oneshot};

type Job = Box<dyn FnOnce(&mut Connection) + Send>;
type DatabaseWorker = thread::JoinHandle<duckdb::Result<()>>;

#[derive(Clone)]
pub struct Store {
    sender: mpsc::Sender<Job>,
}

impl Store {
    pub fn open(path: &Path) -> Result<(Self, DatabaseWorker), Box<dyn std::error::Error>> {
        let mut connection = Connection::open(path)?;
        let tx = connection.transaction()?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS collector_schema (version INTEGER NOT NULL);
            INSERT INTO collector_schema SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM collector_schema);")?;
        let version: i32 =
            tx.query_row("SELECT version FROM collector_schema", [], |r| r.get(0))?;
        if !(1..=2).contains(&version) {
            return Err("Unsupported database schema version".into());
        }
        tx.execute_batch(
            "CREATE SEQUENCE IF NOT EXISTS event_order START 1;
            CREATE TABLE IF NOT EXISTS events (
                ingest_id BIGINT PRIMARY KEY DEFAULT nextval('event_order'),
                event_id VARCHAR UNIQUE NOT NULL,
                view_id VARCHAR NOT NULL,
                video_id VARCHAR NOT NULL,
                viewer_id VARCHAR,
                event_name VARCHAR NOT NULL,
                sequence BIGINT NOT NULL,
                occurred_at_ms BIGINT NOT NULL,
                playing_time_ms DOUBLE,
                received_at_ms BIGINT NOT NULL,
                payload VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS event_otlp (
                event_id VARCHAR PRIMARY KEY,
                payload VARCHAR NOT NULL
            );",
        )?;
        if version == 1 {
            tx.execute_batch("UPDATE collector_schema SET version = 2;")?;
        }
        tx.commit()?;
        let (sender, mut receiver) = mpsc::channel::<Job>(64);
        let worker = thread::spawn(move || {
            while let Some(job) = receiver.blocking_recv() {
                job(&mut connection);
            }
            connection.execute_batch("CHECKPOINT")
        });
        Ok((Self { sender }, worker))
    }

    // One bounded queue owns the connection. Database work never blocks Tokio's HTTP threads.
    pub async fn call<T: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut Connection) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let (sender, receiver) = oneshot::channel();
        self.sender
            .try_send(Box::new(move |connection| {
                let _ = sender.send(operation(connection));
            }))
            .map_err(|_| ApiError::unavailable())?;
        receiver.await.map_err(|_| ApiError::unavailable())?
    }

    pub async fn insert(&self, event: Value) -> Result<bool, ApiError> {
        Ok(self
            .insert_batch(vec![IncomingEvent { event, otlp: None }])
            .await?
            > 0)
    }

    pub async fn insert_batch(&self, events: Vec<IncomingEvent>) -> Result<usize, ApiError> {
        self.call(move |connection| {
            let tx = connection.transaction()?;
            let mut inserted = 0;
            let received_at = chrono::Utc::now().timestamp_millis();
            for record in events {
                let event = record.event;
                let added = tx.execute(
                "INSERT INTO events (event_id, view_id, video_id, viewer_id, event_name, sequence, occurred_at_ms, playing_time_ms, received_at_ms, payload)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?) ON CONFLICT(event_id) DO NOTHING",
                params![event["event_id"].as_str(), event["view_id"].as_str(), event["video_id"].as_str(), event["viewer_id"].as_str(),
                    event["event_name"].as_str(), event["sequence"].as_i64(), event::timestamp(event["occurred_at"].as_str().unwrap())?,
                    event["playing_time_ms"].as_f64(), received_at, event.to_string()],
            )?;
                if added > 0 {
                    if let Some(otlp) = record.otlp {
                        tx.execute("INSERT INTO event_otlp VALUES (?, ?)", params![event["event_id"].as_str(), otlp.to_string()])?;
                    }
                    inserted += added;
                }
            }
            tx.commit()?;
            Ok(inserted)
        }).await
    }

    pub async fn events(&self, query: Query) -> Result<Value, ApiError> {
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<i64>::decode(value, "events", &filters))
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|c| c.after < 0 || c.after > c.through)
        {
            return Err(ApiError::bad("Invalid cursor position"));
        }
        self.call(move |connection| {
            let through = match &cursor { Some(c) => c.through, None => watermark(connection)? };
            let after = cursor.map_or(0, |c| c.after);
            let (conditions, mut values) = filters.sql();
            values.extend([SqlValue::BigInt(after), SqlValue::BigInt(through), SqlValue::BigInt((limit + 1) as i64)]);
            let mut statement = connection.prepare(&format!("SELECT ingest_id, received_at_ms, events.payload, event_otlp.payload FROM events
                LEFT JOIN event_otlp USING (event_id)
                WHERE {conditions} AND ingest_id > ? AND ingest_id <= ? ORDER BY ingest_id LIMIT ?"))?;
            let rows = statement.query_map(params_from_iter(values.iter()), |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, Option<String>>(3)?)))?;
            let mut events = Vec::new();
            let mut last = after;
            let mut next_cursor = None;
            for row in rows {
                let (id, received, payload, otlp_payload) = row?;
                if events.len() == limit {
                    next_cursor = Some(Cursor { version: 1, kind: "events".into(), after: last, through, filters: filters.clone() }.encode()?);
                    break;
                }
                last = id;
                let event: Value = serde_json::from_str(&payload).map_err(|_| ApiError::internal())?;
                let mut record = json!({ "received_at": chrono::DateTime::from_timestamp_millis(received), "event": event });
                if let Some(payload) = otlp_payload {
                    record["otlp"] = serde_json::from_str(&payload).map_err(|_| ApiError::internal())?;
                }
                events.push(record);
            }
            Ok(json!({ "events": events, "next_cursor": next_cursor }))
        }).await
    }

    pub async fn summary(&self, query: Query, per_video: bool) -> Result<Value, ApiError> {
        if query.event_name.is_some()
            || (!per_video && (query.cursor.is_some() || query.limit.is_some()))
        {
            return Err(ApiError::bad("Unsupported summary query parameter"));
        }
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<String>::decode(value, "videos", &filters))
            .transpose()?;
        self.call(move |connection| {
            let through = match &cursor { Some(c) => c.through, None => watermark(connection)? };
            let (conditions, filter_values) = filters.sql();
            let mut values = vec![SqlValue::BigInt(through)];
            values.extend(filter_values);
            // Date and identity filters select starts. Each selected view contributes its
            // whole cumulative value, even when its final event occurs after the date range.
            let sql = format!("WITH retained AS (SELECT * FROM events WHERE ingest_id <= ?),
                started AS (
                    SELECT view_id, video_id, viewer_id, occurred_at_ms FROM retained
                    WHERE event_name = 'chunkify.video.view_started'
                    QUALIFY ROW_NUMBER() OVER (PARTITION BY view_id ORDER BY sequence, occurred_at_ms, ingest_id) = 1
                ), cohort AS (SELECT * FROM started WHERE {conditions}),
                latest AS (
                    SELECT view_id, video_id, viewer_id, playing_time_ms FROM retained
                    WHERE event_name IN ('chunkify.video.view_ended', 'chunkify.video.view_snapshot')
                    QUALIFY ROW_NUMBER() OVER (
                        PARTITION BY view_id, video_id, viewer_id
                        ORDER BY (event_name = 'chunkify.video.view_ended') DESC, sequence DESC, occurred_at_ms DESC, ingest_id DESC
                    ) = 1
                ), view_metrics AS (
                    SELECT cohort.video_id, cohort.viewer_id, COALESCE(latest.playing_time_ms, 0) AS playing_time_ms
                    FROM cohort LEFT JOIN latest ON cohort.view_id = latest.view_id AND cohort.video_id = latest.video_id
                        AND cohort.viewer_id IS NOT DISTINCT FROM latest.viewer_id
                )");
            if !per_video {
                let result = connection.query_row(&format!("{sql}
                    SELECT COUNT(*), COUNT(DISTINCT viewer_id), COALESCE(SUM(playing_time_ms), 0) FROM view_metrics"),
                    params_from_iter(values.iter()), |row| Ok(json!({
                        "views": row.get::<_, i64>(0)?, "unique_viewers": row.get::<_, i64>(1)?,
                        "total_playing_time_ms": row.get::<_, f64>(2)?
                    })))?;
                return Ok(result);
            }
            let after = cursor.map_or(String::new(), |c| c.after);
            values.extend([SqlValue::Text(after.clone()), SqlValue::BigInt((limit + 1) as i64)]);
            let mut statement = connection.prepare(&format!("{sql}
                SELECT video_id, COUNT(*), COUNT(DISTINCT viewer_id), SUM(playing_time_ms) FROM view_metrics
                WHERE video_id > ? GROUP BY video_id ORDER BY video_id LIMIT ?"))?;
            let rows = statement.query_map(params_from_iter(values.iter()), |row| Ok((row.get::<_, String>(0)?, json!({
                "video_id": row.get::<_, String>(0)?, "views": row.get::<_, i64>(1)?,
                "unique_viewers": row.get::<_, i64>(2)?, "total_playing_time_ms": row.get::<_, f64>(3)?
            }))))?;
            let mut videos = Vec::new();
            let mut last = after;
            let mut next_cursor = None;
            for row in rows {
                let (id, video) = row?;
                if videos.len() == limit {
                    next_cursor = Some(Cursor { version: 1, kind: "videos".into(), after: last, through, filters: filters.clone() }.encode()?);
                    break;
                }
                last = id;
                videos.push(video);
            }
            Ok(json!({ "videos": videos, "next_cursor": next_cursor }))
        }).await
    }
}

fn watermark(connection: &Connection) -> Result<i64, ApiError> {
    Ok(connection.query_row(
        "SELECT COALESCE(MAX(ingest_id), 0) FROM events",
        [],
        |row| row.get(0),
    )?)
}
