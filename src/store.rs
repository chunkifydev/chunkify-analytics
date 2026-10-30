use crate::{
    api::ApiError,
    event::IncomingEvent,
    geolocation::Location,
    query::{Cursor, Query, SortOrder},
    reader::Reader,
    writer::{BatchConfig, Writer},
};
use duckdb::{Connection, params_from_iter, types::Value as SqlValue};
use serde_json::{Value, json};
use std::{path::Path, sync::Arc, thread};

pub struct DatabaseWorker {
    database: thread::JoinHandle<duckdb::Result<()>>,
    reader: Option<thread::JoinHandle<()>>,
    // Keep the writer alive until readers finish.
    writer: Writer,
}
impl DatabaseWorker {
    pub fn join(self) -> thread::Result<Result<(), String>> {
        let reader = self.reader.map(|worker| worker.join()).transpose();
        drop(self.writer);
        let database = self.database.join();
        reader?;
        Ok(database?.map_err(|e| e.to_string()))
    }
}

#[derive(Clone)]
pub struct Store {
    writer: Writer,
    reader: Option<Reader>,
    remote: Option<RemoteReads>,
    limits: Option<Arc<crate::limits::Reads>>,
}

#[derive(Clone)]
struct RemoteReads {
    store: Arc<crate::s3_store::RemoteStore>,
    projects: Arc<crate::projects::Registry>,
}

impl Store {
    pub fn from_config(
        config: &crate::config::Config,
        projects: Arc<crate::projects::Registry>,
    ) -> Result<(Self, DatabaseWorker), Box<dyn std::error::Error>> {
        let crate::backend::Backend::S3(settings) = &config.backend else {
            let (mut store, worker) = Self::open(
                &config.database_path(),
                config.batch.clone(),
                config.profile,
                config.checkpoint_threshold_mib,
                config.separate_reads,
                config.duckdb_threads,
            )?;
            store.limits = Some(Arc::new(crate::limits::Reads::new(
                &config.rate_limits,
                projects,
            )));
            return Ok((store, worker));
        };
        let settings_db =
            database_settings(config.checkpoint_threshold_mib, config.duckdb_threads)?;
        let connection = Connection::open_in_memory_with_flags(settings_db)?;
        let temporary = config.data_dir.join("temporary");
        std::fs::create_dir_all(&temporary)?;
        connection.execute_batch(&format!(
            "SET temp_directory = {}; SET max_temp_directory_size = '1 GiB';",
            crate::s3_store::literal(&temporary.to_string_lossy())
        ))?;
        let remote = Arc::new(crate::s3_store::RemoteStore::open(
            settings.clone(),
            &connection,
            &config.data_dir,
        )?);
        let (writer, database) = Writer::start_with_storage(
            connection,
            config.batch.clone(),
            config.profile,
            Some(remote.clone()),
        )?;
        Ok((
            Self {
                writer: writer.clone(),
                reader: None,
                limits: Some(Arc::new(crate::limits::Reads::new(
                    &config.rate_limits,
                    projects.clone(),
                ))),
                remote: Some(RemoteReads {
                    store: remote,
                    projects,
                }),
            },
            DatabaseWorker {
                database,
                reader: None,
                writer,
            },
        ))
    }

    pub fn open(
        path: &Path,
        batch: BatchConfig,
        profile: bool,
        checkpoint_threshold_mib: u32,
        separate_reads: bool,
        duckdb_threads: Option<usize>,
    ) -> Result<(Self, DatabaseWorker), Box<dyn std::error::Error>> {
        if let Some(parent) = path.parent().filter(|path| !path.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        let settings = database_settings(checkpoint_threshold_mib, duckdb_threads)?;
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
        Ok((
            Self {
                writer: writer.clone(),
                reader,
                remote: None,
                limits: None,
            },
            DatabaseWorker {
                database: worker,
                reader: reader_worker,
                writer,
            },
        ))
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
        project: &crate::projects::Access,
        key_id: &str,
        event: Value,
        request_context: Option<Location>,
    ) -> Result<usize, ApiError> {
        self.insert_batch(
            project,
            key_id,
            vec![IncomingEvent { event, otlp: None }],
            request_context,
        )
        .await
    }

    async fn read<T: Send + 'static>(
        &self,
        name: &'static str,
        project: String,
        operation: impl FnOnce(&Connection) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let admission = self
            .limits
            .as_ref()
            .map(|limits| limits.admit(&project))
            .transpose()?;
        // The queued database operation owns its permits even if HTTP/MCP cancels.
        let operation = move |connection: &Connection| {
            let _admission = admission;
            operation(connection)
        };
        if let Some(reader) = &self.reader {
            return reader.call(name, operation).await;
        }
        if let Some(remote) = self.remote.clone() {
            return self
                .writer
                .call(name, move |connection| {
                    let policy = remote
                        .projects
                        .retention
                        .read()
                        .map_err(|_| ApiError::internal())?;
                    remote.store.prepare_read(connection, &project, &policy)?;
                    drop(policy);
                    crate::reader::snapshot(connection, operation)
                })
                .await;
        }
        self.writer
            .call(name, move |connection| {
                crate::reader::snapshot(connection, operation)
            })
            .await
    }

    pub async fn export_reader(&self, project: String) -> Result<Self, ApiError> {
        let connection = self
            .call("export_connection", |db| Ok(db.try_clone()?))
            .await?;
        let remote = self.remote.clone();
        let connection = tokio::task::spawn_blocking(move || {
            if let Some(remote) = remote {
                crate::storage::create_parquet_stage(&connection)?;
                let policy = remote
                    .projects
                    .retention
                    .read()
                    .map_err(|_| ApiError::internal())?;
                remote.store.prepare_read(&connection, &project, &policy)?;
            }
            Ok::<_, ApiError>(connection)
        })
        .await
        .map_err(|_| ApiError::unavailable())??;
        let (reader, _) = Reader::start(connection, false)?;
        Ok(Self {
            writer: self.writer.clone(),
            reader: Some(reader),
            remote: self.remote.clone(),
            limits: None,
        })
    }

    pub async fn purge_expired(
        &self,
        projects: std::sync::Arc<crate::projects::Registry>,
    ) -> Result<usize, ApiError> {
        let remote = self.remote.as_ref().map(|remote| remote.store.clone());
        self.call("retention", move |connection| {
            // Read the current policy when the writer runs this job, and hold it
            // through commit so a successful live update cannot race a stale sweep.
            let policy = projects
                .retention
                .read()
                .map_err(|_| ApiError::internal())?;
            if !policy.enabled() {
                return Ok(0);
            }
            match remote {
                Some(remote) => remote.purge(&policy, chrono::Utc::now().timestamp_millis()),
                None => Ok(policy.purge(connection, chrono::Utc::now().timestamp_millis())?),
            }
        })
        .await
    }

    pub async fn insert_batch(
        &self,
        project: &crate::projects::Access,
        key_id: &str,
        events: Vec<IncomingEvent>,
        request_context: Option<Location>,
    ) -> Result<usize, ApiError> {
        self.writer
            .insert(project, key_id, events, request_context)
            .await
    }

    pub async fn events(&self, project: String, query: Query) -> Result<Value, ApiError> {
        let export = query.export.clone();
        if query.sort_by.is_some() || query.group_by.is_some() {
            return Err(ApiError::bad(
                "sort_by and group_by are not supported by events",
            ));
        }
        let order = query.order.unwrap_or_default();
        // Preserve existing ascending cursors; bind descending cursors to their order.
        let kind = match order {
            SortOrder::Asc => "events",
            SortOrder::Desc => "events_desc",
        };
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|value| Cursor::<i64>::decode(value, kind, &filters, &project))
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|c| c.after < 0 || c.after > c.through)
        {
            return Err(ApiError::bad("Invalid cursor position"));
        }
        let remote = self.remote.is_some();
        self.read("events", project.clone(), move |connection| {
            let through = match &cursor {
                Some(c) => c.through,
                None => watermark(connection, &project)?,
            };
            let after = cursor.as_ref().map_or(0, |c| c.after);
            let (lower, upper) = match order {
                SortOrder::Asc => (after, through),
                SortOrder::Desc => (
                    0,
                    cursor
                        .as_ref()
                        .map_or(through, |c| c.after.saturating_sub(1)),
                ),
            };
            let (conditions, mut values) = filters.sql();
            let conditions = format!("({conditions}) AND project_id = ?");
            values.push(SqlValue::Text(project.clone()));
            values.extend([
                SqlValue::BigInt(lower),
                SqlValue::BigInt(upper),
                SqlValue::BigInt((limit + 1) as i64),
            ]);
            let mut statement = connection.prepare(&(if remote {
                crate::storage::parquet_page_sql
            } else if export.is_some() {
                crate::storage::export_sql
            } else {
                crate::storage::page_sql
            })(
                &conditions,
                matches!(order, SortOrder::Desc),
            ))?;
            let mut rows = query_rows(&mut statement, &values, export.is_some())?;
            let mut events = Vec::new();
            let mut last = after;
            let mut next_cursor = None;
            while let Some(row) = rows.next()? {
                if events.len() == limit {
                    next_cursor = Some(
                        Cursor {
                            version: 1,
                            project_id: project.clone(),
                            kind: kind.into(),
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
                crate::exports::emit(&export, &mut events, record)?;
            }
            Ok(json!({ "events": events, "next_cursor": next_cursor }))
        })
        .await
    }
}

fn database_settings(
    checkpoint_threshold_mib: u32,
    threads: Option<usize>,
) -> duckdb::Result<duckdb::Config> {
    let settings = duckdb::Config::default().with(
        "checkpoint_threshold",
        format!("{checkpoint_threshold_mib} MiB"),
    )?;
    match threads {
        Some(threads) => settings.with("threads", threads.to_string()),
        None => Ok(settings),
    }
}

fn watermark(connection: &Connection, project: &str) -> Result<i64, ApiError> {
    Ok(connection.query_row(
        "SELECT COALESCE(MAX(ingest_id), 0) FROM events WHERE project_id = ?",
        [project],
        |row| row.get(0),
    )?)
}

fn query_rows<'s>(
    statement: &'s mut duckdb::Statement<'_>,
    values: &[SqlValue],
    streaming: bool,
) -> duckdb::Result<duckdb::Rows<'s>> {
    if streaming {
        // Initialize DuckDB's streaming result, then consume it through the
        // fallible row API so fetch failures abort the HTTP stream cleanly.
        drop(statement.stream_arrow(params_from_iter(values.iter()))?);
        Ok(statement.raw_query())
    } else {
        statement.query(params_from_iter(values.iter()))
    }
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
            true,
            None,
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
            store.summary("default".into(), Query::default(), false),
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

#[cfg(test)]
mod summary_tests;

#[cfg(test)]
mod page_lookup_tests;

#[cfg(test)]
mod limits_tests;

mod exploration;
mod metrics;
mod renditions;
mod retention;
pub(crate) use retention::validate_query as validate_retention_query;

#[cfg(test)]
mod exploration_tests;
