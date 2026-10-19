use crate::{
    api::ApiError,
    context::{RequestContext, event_context},
    event::IncomingEvent,
    profile::Profile,
};
use duckdb::{
    Connection, Transaction, appender_params_from_iter, params, params_from_iter, types::Value,
};
use std::{
    collections::HashSet,
    env,
    mem::size_of,
    sync::{Arc, mpsc},
    thread,
    time::{Duration, Instant},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, oneshot};

#[derive(Clone)]
pub struct BatchConfig {
    pub max_events: usize,
    pub max_bytes: usize,
    pub delay: Duration,
    pub queue_bytes: usize,
    pub queue_requests: usize,
    pub bulk: bool,
}

impl BatchConfig {
    pub fn load() -> Result<Self, Box<dyn std::error::Error>> {
        fn number(
            name: &str,
            default: usize,
            min: usize,
            max: usize,
        ) -> Result<usize, Box<dyn std::error::Error>> {
            let value = env::var(name)
                .unwrap_or_else(|_| default.to_string())
                .parse::<usize>()
                .map_err(|_| format!("{name} must be between {min} and {max}"))?;
            if !(min..=max).contains(&value) {
                return Err(format!("{name} must be between {min} and {max}").into());
            }
            Ok(value)
        }
        let max_bytes = number("COLLECTOR_BATCH_MAX_BYTES", 1048576, 1, 67108864)?;
        Ok(Self {
            bulk: match env::var("COLLECTOR_INSERT_MODE")
                .as_deref()
                .unwrap_or("bulk")
            {
                "bulk" => true,
                "row" => false,
                _ => return Err("COLLECTOR_INSERT_MODE must be bulk or row".into()),
            },
            max_events: number("COLLECTOR_BATCH_MAX_EVENTS", 100, 1, 10000)?,
            max_bytes,
            delay: Duration::from_millis(number("COLLECTOR_BATCH_DELAY_MS", 10, 0, 1000)? as u64),
            queue_bytes: number("COLLECTOR_QUEUE_MAX_BYTES", 16777216, max_bytes, 268435456)?,
            queue_requests: number("COLLECTOR_QUEUE_MAX_REQUESTS", 256, 1, 4096)?,
        })
    }
}

type Operation = Box<dyn FnOnce(&mut Connection, &mut Profile) + Send>;
enum Job {
    Call(Operation),
    Write(WriteRequest),
}

struct Row {
    values: Vec<Value>,
    otlp: Option<String>,
}
impl Row {
    fn id(&self) -> &str {
        match &self.values[0] {
            Value::Text(id) => id,
            _ => unreachable!("validated event ID"),
        }
    }
    fn project(&self) -> &str {
        match self.values.get(self.values.len() - 2) {
            Some(Value::Text(id)) => id,
            _ => unreachable!("resolved project"),
        }
    }
    fn prepare(
        record: &IncomingEvent,
        project: &str,
        key_id: Option<&str>,
        context: Option<&serde_json::Value>,
        received: i64,
    ) -> Result<Self, ApiError> {
        let values = crate::storage::encode(
            &record.event,
            event_context(&record.event, context).as_ref(),
            received,
            project,
            key_id,
        )?;
        Ok(Self {
            values,
            otlp: record.otlp.as_ref().map(|v| v.to_string()),
        })
    }

    fn heap_bytes(&self) -> usize {
        self.values.capacity() * size_of::<Value>()
            + self
                .values
                .iter()
                .map(|v| match v {
                    Value::Text(v) => v.capacity(),
                    _ => 0,
                })
                .sum::<usize>()
            + self.otlp.as_ref().map_or(0, String::capacity)
    }
}

struct WriteRequest {
    rows: Vec<Row>,
    bytes: usize,
    queued: Instant,
    preparation: Duration,
    reply: oneshot::Sender<Result<usize, ApiError>>,
    _permit: OwnedSemaphorePermit,
    project_permit: Option<crate::projects::Admission>,
}

#[derive(Clone)]
pub struct Writer {
    sender: mpsc::SyncSender<Job>,
    budget: Arc<Semaphore>,
    config: BatchConfig,
    profile: bool,
}

impl Writer {
    // Bump when the prepared row representation changes. Durable requests may
    // have been admitted by an older binary with a different memory estimate.
    pub const PREPARED_VERSION: u32 = 5;

    pub fn prepared_bytes(request: &crate::spool::StoredRequest) -> Result<usize, ApiError> {
        let mut bytes = size_of::<WriteRequest>() + request.events.len() * size_of::<Row>();
        for event in &request.events {
            bytes += Row::prepare(
                event,
                &request.project_id,
                request.ingest_key_id.as_deref(),
                request.context.as_ref(),
                request.received,
            )?
            .heap_bytes();
        }
        Ok(bytes)
    }
    // Called only by the durable-queue thread. Preserve receipt time and request
    // context across recovery; SQLite retains the batch until this commit succeeds.
    pub fn replay(&self, requests: Vec<crate::spool::StoredRequest>) -> Result<usize, ApiError> {
        let started = Instant::now();
        let mut rows = Vec::with_capacity(requests.iter().map(|r| r.events.len()).sum());
        for request in requests {
            for event in request.events {
                rows.push(Row::prepare(
                    &event,
                    &request.project_id,
                    request.ingest_key_id.as_deref(),
                    request.context.as_ref(),
                    request.received,
                )?);
            }
        }
        let bytes = size_of::<WriteRequest>()
            + rows.capacity() * size_of::<Row>()
            + rows.iter().map(Row::heap_bytes).sum::<usize>();
        let permit = self
            .budget
            .clone()
            .try_acquire_many_owned(bytes.try_into().map_err(|_| ApiError::internal())?)
            .map_err(|_| ApiError::unavailable())?;
        let (reply, receiver) = oneshot::channel();
        self.sender
            .send(Job::Write(WriteRequest {
                rows,
                bytes,
                queued: Instant::now(),
                preparation: started.elapsed(),
                reply,
                _permit: permit,
                project_permit: None,
            }))
            .map_err(|_| ApiError::unavailable())?;
        receiver
            .blocking_recv()
            .map_err(|_| ApiError::unavailable())?
    }
    pub fn start(
        mut connection: Connection,
        config: BatchConfig,
        profile: bool,
    ) -> duckdb::Result<(Self, thread::JoinHandle<duckdb::Result<()>>)> {
        if config.bulk {
            crate::storage::create_stage(&connection)?;
        }
        let (sender, receiver) = mpsc::sync_channel(config.queue_requests);
        let budget = Arc::new(Semaphore::new(config.queue_bytes));
        let worker_config = config.clone();
        let mut measurements = Profile::new(profile, &connection, "writer")?;
        let worker = thread::spawn(move || {
            let profile = &mut measurements;
            let mut pending = None;
            while let Some(job) = pending.take().or_else(|| receiver.recv().ok()) {
                match job {
                    Job::Call(operation) => operation(&mut connection, profile),
                    Job::Write(first) => {
                        let deadline = Instant::now() + worker_config.delay;
                        let mut records = first.rows.len();
                        let mut bytes = first.bytes;
                        let mut batch = vec![first];
                        while records < worker_config.max_events && bytes < worker_config.max_bytes
                        {
                            if Instant::now() >= deadline {
                                break;
                            }
                            let next = match receiver.try_recv() {
                                Ok(job) => Some(job),
                                Err(mpsc::TryRecvError::Empty) => receiver
                                    .recv_timeout(
                                        deadline.saturating_duration_since(Instant::now()),
                                    )
                                    .ok(),
                                Err(mpsc::TryRecvError::Disconnected) => None,
                            };
                            match next {
                                Some(Job::Write(request))
                                    if records + request.rows.len() <= worker_config.max_events
                                        && bytes + request.bytes <= worker_config.max_bytes =>
                                {
                                    records += request.rows.len();
                                    bytes += request.bytes;
                                    batch.push(request);
                                }
                                Some(job) => {
                                    pending = Some(job);
                                    break;
                                }
                                None => break,
                            }
                        }
                        flush(
                            &mut connection,
                            batch,
                            records,
                            bytes,
                            worker_config.bulk,
                            profile,
                        );
                    }
                }
            }
            profile.report();
            connection.execute_batch("CHECKPOINT")
        });
        Ok((
            Self {
                sender,
                budget,
                config,
                profile,
            },
            worker,
        ))
    }

    pub async fn call<T: Send + 'static>(
        &self,
        name: &'static str,
        operation: impl FnOnce(&mut Connection) -> Result<T, ApiError> + Send + 'static,
    ) -> Result<T, ApiError> {
        let (sender, receiver) = oneshot::channel();
        let queued = self.profile.then(Instant::now);
        self.sender
            .try_send(Job::Call(Box::new(move |connection, profile| {
                let started = profile.start();
                let result = operation(connection);
                profile.job(name, queued, started);
                let _ = sender.send(result);
            })))
            .map_err(|_| ApiError::unavailable())?;
        receiver.await.map_err(|_| ApiError::unavailable())?
    }

    pub async fn insert(
        &self,
        project: &crate::projects::Access,
        key_id: &str,
        events: Vec<IncomingEvent>,
        request_context: Option<RequestContext>,
    ) -> Result<usize, ApiError> {
        let too_large = || {
            ApiError(
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "Request exceeds the collector write-batch limit".into(),
            )
        };
        if events.len() > self.config.max_events {
            return Err(too_large());
        }
        let started = Instant::now();
        let received = chrono::Utc::now().timestamp_millis();
        let context = request_context
            .map(serde_json::to_value)
            .transpose()
            .map_err(|_| ApiError::internal())?;
        let rows: Vec<_> = events
            .into_iter()
            .map(|record| {
                Row::prepare(
                    &record,
                    &project.id,
                    Some(key_id),
                    context.as_ref(),
                    received,
                )
            })
            .collect::<Result<_, _>>()?;
        let bytes = size_of::<WriteRequest>()
            + rows.capacity() * size_of::<Row>()
            + rows.iter().map(Row::heap_bytes).sum::<usize>();
        if bytes > self.config.max_bytes {
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
        let project_permit = project.admit(bytes)?;
        let (reply, receiver) = oneshot::channel();
        let request = WriteRequest {
            rows,
            bytes,
            queued: Instant::now(),
            preparation: started.elapsed(),
            reply,
            _permit: permit,
            project_permit: Some(project_permit),
        };
        self.sender
            .try_send(Job::Write(request))
            .map_err(|_| ApiError::unavailable())?;
        receiver.await.map_err(|_| ApiError::unavailable())?
    }
}

fn flush(
    connection: &mut Connection,
    batch: Vec<WriteRequest>,
    records: usize,
    bytes: usize,
    bulk: bool,
    profile: &mut Profile,
) {
    let started = profile.start();
    for request in &batch {
        profile.queue("ingest", Some(request.queued), started);
        profile.duration("prepare_request", request.preparation);
    }
    let result = write_batch(connection, &batch, bulk, profile);
    profile.executed("ingest", started);
    if let Ok(counts) = &result {
        profile.committed(batch.len(), records, bytes, counts.iter().sum());
    }
    for (index, request) in batch.into_iter().enumerate() {
        let response = match &result {
            Ok(counts) => Ok(counts[index]),
            Err(_) => Err(ApiError::internal()),
        };
        // Release prepared buffers before waking the queue consumer, which may
        // immediately reserve the same byte budget for its next batch.
        drop(request.rows);
        drop(request._permit);
        drop(request.project_permit);
        let _ = request.reply.send(response);
    }
}

fn write_batch(
    connection: &mut Connection,
    batch: &[WriteRequest],
    bulk: bool,
    profile: &mut Profile,
) -> Result<Vec<usize>, ApiError> {
    let tx = profile.measure("begin", || connection.transaction())?;
    let counts = if bulk {
        write_bulk(&tx, batch, profile)?
    } else {
        write_rows(&tx, batch, profile)?
    };
    profile.measure("commit", || tx.commit())?;
    Ok(counts)
}

fn write_rows(
    tx: &Transaction<'_>,
    batch: &[WriteRequest],
    profile: &mut Profile,
) -> Result<Vec<usize>, ApiError> {
    let mut counts = vec![0; batch.len()];
    for (index, request) in batch.iter().enumerate() {
        for row in &request.rows {
            let added = profile.measure("insert_event", || {
                tx.prepare_cached(&crate::storage::INSERT_SQL)?
                    .execute(params_from_iter(&row.values))
            })?;
            if added > 0 {
                if let Some(otlp) = &row.otlp {
                    profile.measure("insert_otlp", || {
                        tx.prepare_cached("INSERT INTO event_otlp VALUES (?, ?, ?)")?
                            .execute(params![row.project(), &row.values[0], otlp])
                    })?;
                }
                counts[index] += added;
            }
        }
    }
    Ok(counts)
}

fn write_bulk(
    tx: &Transaction<'_>,
    batch: &[WriteRequest],
    profile: &mut Profile,
) -> Result<Vec<usize>, ApiError> {
    // Deduplicate in admission order, including duplicates within an OTLP request.
    // RETURNING identifies which of these first occurrences are new to the database.
    let mut seen = HashSet::new();
    let rows: Vec<_> = batch
        .iter()
        .enumerate()
        .flat_map(|(request, item)| item.rows.iter().map(move |row| (request, row)))
        .filter(|(_, row)| seen.insert((row.project(), row.id())))
        .collect();
    // Empty OTLP exports are valid. Avoid an empty appender/INSERT cycle.
    if rows.is_empty() {
        return Ok(vec![0; batch.len()]);
    }
    profile.measure("append_events", || -> duckdb::Result<()> {
        let mut appender =
            tx.appender_to_catalog_and_db("collector_write_stage", "temp", "main")?;
        for (ordinal, (_, row)) in rows.iter().enumerate() {
            let ordinal = Value::BigInt(ordinal as i64);
            appender.append_row(appender_params_from_iter(
                std::iter::once(&ordinal).chain(row.values.iter()),
            ))?;
        }
        appender.flush()
    })?;
    let inserted: HashSet<(String, String)> =
        profile.measure("insert_event", || -> duckdb::Result<_> {
            tx.prepare_cached(&crate::storage::BULK_SQL)?
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect()
        })?;
    let mut counts = vec![0; batch.len()];
    profile.measure("insert_otlp", || -> duckdb::Result<()> {
        let mut appender = tx.appender("event_otlp")?;
        for (request, row) in rows {
            if inserted.contains(&(row.project().to_owned(), row.id().to_owned())) {
                counts[request] += 1;
                if let Some(otlp) = &row.otlp {
                    appender.append_row(params![row.project(), row.id(), otlp])?;
                }
            }
        }
        appender.flush()
    })?;
    profile.measure("clear_stage", || {
        tx.execute("DELETE FROM collector_write_stage", [])
    })?;
    Ok(counts)
}
