//! Ingestion and read admission for ClickHouse. Several analytics servers can
//! share one database: each batches its own requests, and ClickHouse merges
//! concurrent inserts. Reads use independent HTTP requests per report.
use super::{Client, Params};
use crate::{
    api::ApiError, context::event_context, event::IncomingEvent, geolocation::Location,
    writer::BatchConfig,
};
use duckdb::types::Value as SqlValue;
use serde_json::{Map, Value};
use std::{
    sync::{
        Arc,
        atomic::{AtomicI64, Ordering},
    },
    time::Instant,
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

pub struct Store {
    pub(super) client: Client,
    pub(super) projects: Arc<crate::projects::Registry>,
    limits: crate::limits::Reads,
    cache: crate::reader::cache::ReportCache,
    sender: mpsc::Sender<WriteRequest>,
    budget: Arc<Semaphore>,
    batch: BatchConfig,
}

struct Row {
    project: String,
    event_id: String,
    json: String,
}

struct WriteRequest {
    rows: Vec<Row>,
    bytes: usize,
    queued: Instant,
    reply: oneshot::Sender<Result<usize, ApiError>>,
    _permit: OwnedSemaphorePermit,
    _project: crate::projects::Admission,
}

/// Unique, increasing event order across servers, below 2^53 so JSON clients
/// read cursors exactly: 16-microsecond units since 2025 in the high bits and a
/// 6-bit server identifier in the low bits. Bursts borrow future units.
const ID_EPOCH_MICROS: i64 = 1_735_689_600_000_000;
const NODE_BITS: u32 = 6;

fn id_unit(micros: i64) -> i64 {
    (micros - ID_EPOCH_MICROS).max(0) >> 4
}

/// The smallest event order that can be assigned at or after this time.
pub(super) fn first_id_at(milliseconds: i64) -> i64 {
    id_unit(milliseconds.saturating_mul(1000)) << NODE_BITS
}

struct Ids {
    node: i64,
    last_unit: AtomicI64,
}

impl Ids {
    fn new(node: Option<u16>) -> Self {
        let node = node.map_or_else(
            || (uuid::Uuid::new_v4().as_u128() & ((1 << NODE_BITS) - 1)) as i64,
            i64::from,
        );
        Self {
            node,
            last_unit: AtomicI64::new(0),
        }
    }

    fn next(&self, count: usize) -> Result<Vec<i64>, ApiError> {
        let now = id_unit(chrono::Utc::now().timestamp_micros());
        let mut first = 0;
        self.last_unit
            .try_update(Ordering::AcqRel, Ordering::Acquire, |last| {
                first = now.max(last + 1);
                first.checked_add(count.max(1) as i64 - 1)
            })
            .map_err(|_| ApiError::internal())?;
        (0..count as i64)
            .map(|offset| {
                (first + offset)
                    .checked_shl(NODE_BITS)
                    .filter(|id| *id < 1 << 53)
                    .map(|id| id | self.node)
                    .ok_or_else(ApiError::internal)
            })
            .collect()
    }
}

fn json_value(value: SqlValue) -> Value {
    match value {
        SqlValue::Null => Value::Null,
        SqlValue::Boolean(value) => Value::Bool(value),
        SqlValue::Int(value) => Value::from(value),
        SqlValue::BigInt(value) => Value::from(value),
        SqlValue::Double(value) => {
            serde_json::Number::from_f64(value).map_or(Value::Null, Value::Number)
        }
        SqlValue::Text(value) => Value::String(value),
        _ => unreachable!("storage encodes only scalar values"),
    }
}

/// One JSONEachRow object without `ingest_id`, which is assigned at insert.
fn encode_row(
    record: &IncomingEvent,
    project: &str,
    key_id: &str,
    context: Option<&Value>,
    received: i64,
) -> Result<String, ApiError> {
    let values = crate::storage::encode(
        &record.event,
        event_context(&record.event, context).as_ref(),
        received,
        project,
        Some(key_id),
    )?;
    let mut object = Map::with_capacity(values.len() + 1);
    for ((name, _), value) in crate::storage::COLUMNS.iter().zip(values) {
        object.insert(name.clone(), json_value(value));
    }
    object.insert(
        "otlp_payload".into(),
        record
            .otlp
            .as_ref()
            .map_or(Value::Null, |otlp| Value::String(otlp.to_string())),
    );
    serde_json::to_string(&object).map_err(|_| ApiError::internal())
}

impl Store {
    pub async fn open(
        config: &crate::config::Config,
        settings: &crate::backend::ClickHouseConfig,
        projects: Arc<crate::projects::Registry>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let client = Client::new(settings.clone())?;
        client
            .ensure_schema()
            .await
            .map_err(|_| "Cannot create the ClickHouse schema; check clickhouse.url, credentials and permissions")?;
        let (sender, receiver) = mpsc::channel(config.batch.queue_requests);
        let ids = Ids::new(settings.node_id);
        tokio::spawn(write_batches(
            client.clone(),
            receiver,
            config.batch.clone(),
            settings.clone(),
            ids,
            projects.clone(),
        ));
        Ok(Arc::new(Self {
            limits: crate::limits::Reads::new(&config.rate_limits, projects.clone()),
            cache: crate::reader::cache::ReportCache::new(config.report_cache_seconds),
            budget: Arc::new(Semaphore::new(config.batch.queue_bytes)),
            batch: config.batch.clone(),
            client,
            projects,
            sender,
        }))
    }

    pub async fn health(&self) -> Result<(), ApiError> {
        self.client
            .one("SELECT 1", &Params::default())
            .await
            .map(|_| ())
    }

    pub async fn insert(
        &self,
        project: &crate::projects::Access,
        key_id: &str,
        events: Vec<IncomingEvent>,
        request_context: Option<Location>,
    ) -> Result<usize, ApiError> {
        let too_large = || {
            ApiError::new(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds the collector write-batch limit".into(),
            )
        };
        if events.len() > self.batch.max_events {
            return Err(too_large());
        }
        let received = chrono::Utc::now().timestamp_millis();
        let context = request_context
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| ApiError::internal())?;
        let rows = events
            .iter()
            .map(|record| {
                Ok(Row {
                    project: project.id.clone(),
                    event_id: record.event["event_id"]
                        .as_str()
                        .ok_or_else(ApiError::internal)?
                        .to_owned(),
                    json: encode_row(record, &project.id, key_id, context.as_ref(), received)?,
                })
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let bytes = std::mem::size_of::<WriteRequest>()
            + rows
                .iter()
                .map(|row| row.json.len() + row.project.len() + row.event_id.len() + 64)
                .sum::<usize>();
        if bytes > self.batch.max_bytes {
            return Err(too_large());
        }
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(bytes as u32)
            .map_err(|_| ApiError::unavailable())?;
        if bytes > project.queue_bytes {
            return Err(too_large());
        }
        let admission = project.admit(bytes)?;
        let (reply, receiver) = oneshot::channel();
        self.sender
            .try_send(WriteRequest {
                rows,
                bytes,
                queued: Instant::now(),
                reply,
                _permit: permit,
                _project: admission,
            })
            .map_err(|_| ApiError::unavailable())?;
        receiver.await.map_err(|_| ApiError::unavailable())?
    }

    /// Reads from all report types share per-project and server admission.
    pub(super) async fn admit(&self, project: &str) -> Result<crate::limits::Admission, ApiError> {
        self.limits.admit(project).await
    }

    pub(super) fn cutoff(&self, project: &str) -> Result<i64, ApiError> {
        Ok(self
            .projects
            .retention
            .read()
            .map_err(|_| ApiError::internal())?
            .project_cutoff(project, chrono::Utc::now().timestamp_millis()))
    }

    pub(super) async fn cached<F>(
        &self,
        name: &'static str,
        project: &str,
        query: impl serde::Serialize,
        export: bool,
        work: F,
    ) -> Result<Value, ApiError>
    where
        F: std::future::Future<Output = Result<Value, ApiError>>,
    {
        // Export downloads have their own budget and bypass the report cache.
        if export {
            return work.await;
        }
        let _admission = self.admit(project).await?;
        let retention = self
            .projects
            .retention
            .read()
            .map_err(|_| ApiError::internal())?
            .project_seconds(project);
        self.cache
            .read(project, (name, retention, query), work)
            .await
    }

    /// Hide expired rows in reads immediately; delete them in the background.
    pub async fn purge_expired(&self) -> Result<usize, ApiError> {
        if !self
            .projects
            .retention
            .read()
            .map_err(|_| ApiError::internal())?
            .enabled()
        {
            return Ok(0);
        }
        let mut params = Params::default();
        let condition = format!("NOT ({})", retained(&self.projects, &mut params)?);
        let expired = self
            .client
            .one(
                &format!("SELECT count() FROM events WHERE {condition}"),
                &params,
            )
            .await?
            .first()
            .and_then(Value::as_u64)
            .ok_or_else(ApiError::internal)?;
        if expired > 0 {
            self.client
                .execute(format!("DELETE FROM events WHERE {condition}"), &params)
                .await?;
        }
        Ok(expired as usize)
    }
}

/// Rows still within each project's retention period. Expired rows are hidden
/// from reads and duplicate checks before the background sweep deletes them.
fn retained(projects: &crate::projects::Registry, params: &mut Params) -> Result<String, ApiError> {
    let policy = projects
        .retention
        .read()
        .map_err(|_| ApiError::internal())?;
    let now = chrono::Utc::now().timestamp_millis();
    let (ids, cutoffs): (Vec<String>, Vec<i64>) = policy
        .overrides()
        .map(|(id, _)| (id.to_owned(), policy.project_cutoff(id, now)))
        .unzip();
    Ok(format!(
        "received_at_ms >= transform(project_id, {}, {}, {})",
        params.texts(&ids),
        params.ints(&cutoffs),
        params.int(policy.default_cutoff(now))
    ))
}

async fn write_batches(
    client: Client,
    mut receiver: mpsc::Receiver<WriteRequest>,
    config: BatchConfig,
    settings: crate::backend::ClickHouseConfig,
    ids: Ids,
    projects: Arc<crate::projects::Registry>,
) {
    let mut pending: Option<WriteRequest> = None;
    loop {
        let first = match pending.take() {
            Some(request) => request,
            None => match receiver.recv().await {
                Some(request) => request,
                None => return,
            },
        };
        let deadline = tokio::time::Instant::now() + config.delay;
        let mut records = first.rows.len();
        let mut bytes = first.bytes;
        let mut batch = vec![first];
        // Requests that arrive while the previous batch is saved are already
        // queued, so batches grow with load without delaying a quiet server.
        while records < config.max_events && bytes < config.max_bytes {
            let next = match receiver.try_recv() {
                Ok(request) => Some(request),
                Err(_) => tokio::time::timeout_at(deadline, receiver.recv())
                    .await
                    .ok()
                    .flatten(),
            };
            match next {
                Some(request)
                    if records + request.rows.len() <= config.max_events
                        && bytes + request.bytes <= config.max_bytes =>
                {
                    records += request.rows.len();
                    bytes += request.bytes;
                    batch.push(request);
                }
                Some(request) => {
                    pending = Some(request);
                    break;
                }
                None => break,
            }
        }
        // One batch at a time per server keeps duplicate detection exact for
        // requests handled by this server.
        flush(&client, batch, records, bytes, &settings, &ids, &projects).await;
    }
}

/// Event IDs already stored recently, among the given rows.
async fn existing(
    client: &Client,
    rows: &[&Row],
    window: std::time::Duration,
    projects: &crate::projects::Registry,
) -> Result<std::collections::HashSet<(String, String)>, ApiError> {
    let mut found = std::collections::HashSet::new();
    if window.is_zero() {
        return Ok(found);
    }
    let since = first_id_at(chrono::Utc::now().timestamp_millis() - window.as_millis() as i64);
    for chunk in rows.chunks(500) {
        let mut owners: Vec<String> = chunk.iter().map(|row| row.project.clone()).collect();
        owners.sort();
        owners.dedup();
        let ids: Vec<String> = chunk.iter().map(|row| row.event_id.clone()).collect();
        let mut params = Params::default();
        let sql = format!(
            "SELECT DISTINCT project_id, event_id FROM events
            WHERE project_id IN {} AND ingest_id >= {} AND event_id IN {} AND {}",
            params.texts(&owners),
            params.int(since),
            params.texts(&ids),
            retained(projects, &mut params)?
        );
        for row in client.all(&sql, &params).await? {
            if let (Some(project), Some(id)) = (
                row.first().and_then(Value::as_str),
                row.get(1).and_then(Value::as_str),
            ) {
                found.insert((project.to_owned(), id.to_owned()));
            }
        }
    }
    Ok(found)
}

async fn flush(
    client: &Client,
    batch: Vec<WriteRequest>,
    records: usize,
    bytes: usize,
    settings: &crate::backend::ClickHouseConfig,
    ids: &Ids,
    projects: &crate::projects::Registry,
) {
    let started = Instant::now();
    let queue_ms = batch
        .iter()
        .map(|request| (started - request.queued).as_millis())
        .max()
        .unwrap_or_default();
    tracing::debug!(
        backend = "clickhouse",
        requests = batch.len(),
        received = records,
        "batch_started"
    );
    let result = async {
        // Keep the first occurrence of each event ID in admission order, and
        // skip IDs already stored, as local storage does.
        let mut seen = std::collections::HashSet::new();
        let candidates: Vec<(usize, &Row)> = batch
            .iter()
            .enumerate()
            .flat_map(|(index, request)| request.rows.iter().map(move |row| (index, row)))
            .filter(|(_, row)| seen.insert((row.project.as_str(), row.event_id.as_str())))
            .collect();
        let rows: Vec<&Row> = candidates.iter().map(|(_, row)| *row).collect();
        let stored = existing(client, &rows, settings.deduplication_window, projects).await?;
        let new: Vec<(usize, &Row)> = candidates
            .into_iter()
            .filter(|(_, row)| !stored.contains(&(row.project.clone(), row.event_id.clone())))
            .collect();
        let mut counts = vec![0; batch.len()];
        if new.is_empty() {
            return Ok(counts);
        }
        let mut body = Vec::with_capacity(bytes);
        for ((request, row), id) in new.iter().zip(ids.next(new.len())?) {
            body.extend_from_slice(format!("{{\"ingest_id\":{id},").as_bytes());
            body.extend_from_slice(&row.json.as_bytes()[1..]);
            body.push(b'\n');
            counts[*request] += 1;
        }
        let insert: &[(&str, &str)] = if settings.async_insert {
            &[("async_insert", "1"), ("wait_for_async_insert", "1")]
        } else {
            &[("async_insert", "0")]
        };
        client.insert("events", body, insert).await?;
        Ok::<_, ApiError>(counts)
    }
    .await;
    let duration_ms = started.elapsed().as_millis();
    match &result {
        Ok(counts) => {
            let inserted = counts.iter().sum::<usize>();
            tracing::info!(
                backend = "clickhouse",
                requests = batch.len(),
                received = records,
                inserted,
                duplicates = records - inserted,
                bytes,
                queue_ms,
                duration_ms,
                "batch_saved"
            )
        }
        Err(_) => tracing::error!(
            backend = "clickhouse",
            requests = batch.len(),
            received = records,
            bytes,
            queue_ms,
            duration_ms,
            stage = "insert_events",
            "batch_failed"
        ),
    }
    for (index, request) in batch.into_iter().enumerate() {
        let response = match &result {
            Ok(counts) => Ok(counts[index]),
            Err(_) => Err(ApiError::internal()),
        };
        let reply = request.reply;
        drop(request.rows);
        drop(request._permit);
        drop(request._project);
        let _ = reply.send(response);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_increase_stay_json_safe_and_keep_the_server_identifier() {
        let ids = Ids::new(Some(7));
        let first = ids.next(3).unwrap();
        let second = ids.next(1).unwrap();
        assert!(first.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(first.iter().chain(&second).all(|id| id & 63 == 7));
        assert!(second[0] > first[2]);
        assert!(second[0] < 1 << 53);
        assert!(first_id_at(chrono::Utc::now().timestamp_millis() - 1000) < first[0]);
    }

    #[test]
    fn params_escape_text_values() {
        let mut params = Params::default();
        assert_eq!(params.text("a\tb\\c"), "{p0:String}");
        assert_eq!(params.values[0].1, "a\\tb\\\\c");
        assert_eq!(params.float(1e308), "{p1:Float64}");
        assert_eq!(params.values[1].1, "1e308");
        params.texts(&["it's".into()]);
        assert_eq!(params.values[2].1, "['it\\'s']");
    }
}
