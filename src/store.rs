use crate::{
    api::ApiError,
    context::RequestContext,
    event::IncomingEvent,
    query::{Cursor, Query},
    reader::Reader,
    writer::{BatchConfig, Writer},
};
use duckdb::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};
use std::{path::Path, thread};

pub struct DatabaseWorker {
    database: thread::JoinHandle<duckdb::Result<()>>,
    spool: Option<crate::spool::Workers>,
    reader: Option<thread::JoinHandle<()>>,
    // Keep the writer alive until readers finish and the spool finishes replay.
    writer: Writer,
}
impl DatabaseWorker {
    pub fn join(self) -> thread::Result<Result<(), String>> {
        let reader = self.reader.map(|worker| worker.join()).transpose();
        let spool = self.spool.map(|worker| worker.join()).transpose();
        drop(self.writer);
        let database = self.database.join();
        reader?;
        Ok(spool?
            .unwrap_or(Ok(()))
            .and(database?.map_err(|e| e.to_string())))
    }
}

pub enum Receipt {
    Committed(usize),
    Queued,
}

#[derive(Clone)]
pub struct Store {
    writer: Writer,
    spool: Option<crate::spool::Spool>,
    reader: Option<Reader>,
}

impl Store {
    pub fn open(
        path: &Path,
        batch: BatchConfig,
        profile: bool,
        checkpoint_threshold_mib: u32,
        spool_config: Option<crate::spool::SpoolConfig>,
        separate_reads: bool,
    ) -> Result<(Self, DatabaseWorker), Box<dyn std::error::Error>> {
        let spool_path = path.with_file_name("ingestion.sqlite");
        if spool_config.is_none() {
            crate::spool::Spool::require_empty(&spool_path)?;
        }
        let settings = duckdb::Config::default().with(
            "checkpoint_threshold",
            format!("{checkpoint_threshold_mib} MiB"),
        )?;
        let mut connection = Connection::open_with_flags(path, settings)?;
        crate::storage::initialize(&mut connection)?;
        // Clone the connection to the same database instance, after migration.
        // This remains one process and one writer for the data directory.
        let (reader, reader_worker) = if separate_reads {
            let (reader, worker) = Reader::start(connection.try_clone()?, profile)?;
            (Some(reader), Some(worker))
        } else {
            (None, None)
        };
        let (writer, worker) = Writer::start(connection, batch.clone(), profile)?;
        let (spool, spool_worker) = if let Some(config) = spool_config {
            let (spool, worker) =
                crate::spool::Spool::start(&spool_path, config, batch, writer.clone())?;
            (Some(spool), Some(worker))
        } else {
            (None, None)
        };
        Ok((
            Self {
                writer: writer.clone(),
                spool,
                reader,
            },
            DatabaseWorker {
                database: worker,
                spool: spool_worker,
                reader: reader_worker,
                writer,
            },
        ))
    }

    pub fn ensure_healthy(&self) -> Result<(), ApiError> {
        if let Some(spool) = &self.spool {
            spool.ensure_healthy()?;
        }
        Ok(())
    }
    pub async fn ingestion(&self) -> Result<Value, ApiError> {
        match &self.spool {
            Some(spool) => Ok(json!({ "mode": "durable_queue", "queue": spool.stats().await? })),
            None => Ok(json!({ "mode": "direct" })),
        }
    }

    pub async fn drain(&self) -> Result<(), ApiError> {
        if let Some(spool) = &self.spool {
            let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
            loop {
                let stats = spool.stats().await?;
                if stats.failed || tokio::time::Instant::now() >= deadline {
                    return Err(ApiError::unavailable());
                }
                if stats.pending_requests == 0 {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
        Ok(())
    }

    pub async fn call<T: Send + 'static>(
        &self,
        name: &'static str,
        operation: impl FnOnce(&mut Connection) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        self.writer.call(name, operation).await
    }

    pub async fn insert(
        &self,
        event: Value,
        request_context: Option<RequestContext>,
    ) -> Result<Receipt, ApiError> {
        self.insert_batch(vec![IncomingEvent { event, otlp: None }], request_context)
            .await
    }

    async fn read<T: Send + 'static>(
        &self,
        name: &'static str,
        operation: impl FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        match &self.reader {
            Some(reader) => reader.call(name, operation).await,
            None => {
                self.writer
                    .call(name, move |connection| {
                        crate::reader::snapshot(connection, operation)
                    })
                    .await
            }
        }
    }

    pub async fn purge_expired(&self, retention_seconds: u64) -> Result<usize, ApiError> {
        if retention_seconds == 0 {
            return Ok(0);
        }
        self.call("retention", move |connection| {
            let cutoff = chrono::Utc::now().timestamp_millis() - (retention_seconds * 1000) as i64;
            let tx = connection.transaction()?;
            tx.execute("DELETE FROM event_otlp WHERE event_id IN (SELECT event_id FROM events WHERE received_at_ms < ?)", [cutoff])?;
            let removed = tx.execute("DELETE FROM events WHERE received_at_ms < ?", [cutoff])?;
            tx.commit()?;
            Ok(removed)
        }).await
    }

    pub async fn delete_viewer(&self, viewer_id: String) -> Result<usize, ApiError> {
        self.call("delete_viewer", move |connection| {
            let tx = connection.transaction()?;
            tx.execute("DELETE FROM event_otlp WHERE event_id IN (SELECT event_id FROM events WHERE viewer_id = ?)", [&viewer_id])?;
            let removed = tx.execute("DELETE FROM events WHERE viewer_id = ?", [&viewer_id])?;
            tx.commit()?;
            Ok(removed)
        }).await
    }

    pub async fn insert_batch(
        &self,
        events: Vec<IncomingEvent>,
        request_context: Option<RequestContext>,
    ) -> Result<Receipt, ApiError> {
        if let Some(spool) = &self.spool {
            spool.insert(events, request_context).await?;
            Ok(Receipt::Queued)
        } else {
            self.writer
                .insert(events, request_context)
                .await
                .map(Receipt::Committed)
        }
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
        self.read("events", move |connection| {
            let through = match &cursor {
                Some(c) => c.through,
                None => watermark(connection)?,
            };
            let after = cursor.map_or(0, |c| c.after);
            let (conditions, mut values) = filters.sql();
            values.extend([
                SqlValue::BigInt(after),
                SqlValue::BigInt(through),
                SqlValue::BigInt((limit + 1) as i64),
            ]);
            let mut statement = connection.prepare(&crate::storage::page_sql(&conditions))?;
            let mut rows = statement.query(params_from_iter(values.iter()))?;
            let mut events = Vec::new();
            let mut last = after;
            let mut next_cursor = None;
            while let Some(row) = rows.next()? {
                if events.len() == limit {
                    next_cursor = Some(
                        Cursor {
                            version: 1,
                            kind: "events".into(),
                            after: last,
                            through,
                            filters: filters.clone(),
                        }
                        .encode()?,
                    );
                    break;
                }
                let (id, record) = crate::storage::decode(row)?;
                last = id;
                events.push(record);
            }
            Ok(json!({ "events": events, "next_cursor": next_cursor }))
        })
        .await
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
        self.read(if per_video { "videos" } else { "summary" }, move |connection| {
            let through = match &cursor { Some(c) => c.through, None => watermark(connection)? };
            let (conditions, filter_values) = filters.sql();
            let mut values = vec![SqlValue::BigInt(through)];
            values.extend(filter_values);
            // Date, identity and context filters select starts. Each selected view contributes its
            // whole cumulative value, even when its final event occurs after the date range.
            let sql = format!("WITH retained AS (SELECT * FROM events WHERE ingest_id <= ?),
                started AS (
                    SELECT view_id, video_id, viewer_id, occurred_at_ms, context_browser_name, context_os_name, context_device_type FROM retained
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::{sync::mpsc, time::Duration};

    #[tokio::test]
    async fn dashboard_reads_do_not_wait_for_busy_ingestion_worker() {
        let directory =
            std::env::temp_dir().join(format!("collector-reader-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let batch = BatchConfig {
            max_events: 100,
            max_bytes: 1048576,
            delay: Duration::from_millis(10),
            queue_bytes: 16777216,
            queue_requests: 256,
            bulk: true,
        };
        let (store, worker) = Store::open(
            &directory.join("analytics.duckdb"),
            batch,
            false,
            64,
            None,
            true,
        )
        .unwrap();
        let (entered, ready) = tokio::sync::oneshot::channel();
        let (release, blocked) = mpsc::channel();
        let busy_store = store.clone();
        let busy = tokio::spawn(async move {
            busy_store
                .call("test_busy_writer", move |_| {
                    let _ = entered.send(());
                    blocked.recv_timeout(Duration::from_secs(5)).unwrap();
                    Ok(())
                })
                .await
        });
        ready.await.unwrap();
        let result = tokio::time::timeout(
            Duration::from_millis(250),
            store.summary(Query::default(), false),
        )
        .await;
        release.send(()).unwrap();
        assert!(busy.await.unwrap().is_ok());
        drop(store);
        assert!(worker.join().unwrap().is_ok());
        std::fs::remove_dir_all(directory).unwrap();
        assert!(
            result.is_ok_and(|value| value.is_ok_and(|summary| summary["views"] == 0)),
            "dashboard read was blocked behind ingestion work"
        );
    }
}
