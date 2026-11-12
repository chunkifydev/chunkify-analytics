//! Ingestion and read admission for ClickHouse. Several analytics servers can
//! share one database: each batches its own requests, and ClickHouse merges
//! concurrent inserts. Reads use independent HTTP requests per report.
use super::{Client, Params};
use crate::{
    api::ApiError, batch::BatchConfig, context::event_context, event::IncomingEvent,
    geolocation::Location,
};
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
    /// `client` with `CONTROL_TIMEOUT`, for health checks and coordination.
    pub(super) control: Client,
    /// Identifies this server in shared claims.
    instance: String,
    /// This server's node ID, configured or leased, and its lease.
    pub(super) node: Arc<NodeLease>,
    pub(super) projects: Arc<crate::projects::Registry>,
    limits: crate::limits::Reads,
    cache: crate::cache::ReportCache,
    sender: mpsc::Sender<WriteRequest>,
    budget: Arc<Semaphore>,
    batch: BatchConfig,
    pub(super) rollup_interval: std::time::Duration,
    pub(super) rollup_max_lag: std::time::Duration,
    /// Whether reports read settled hours from rollups.
    pub(super) rollup_reads: std::sync::atomic::AtomicBool,
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
    lease: Arc<NodeLease>,
    last_unit: AtomicI64,
}

/// Length of a node ID lease. A server holds the current and the next term,
/// and renews well before the current one ends.
const NODE_TERM_MS: i64 = 60_000;
/// Taken off a lease's end, for clock differences and request time.
const LEASE_MARGIN_MS: i64 = 5_000;

/// This server's node ID, and until when (local clock) it is known to hold
/// it. Event IDs are assigned only while the lease is valid, so two servers
/// never use one node ID at the same time.
pub(super) struct NodeLease {
    node: AtomicI64,
    valid_until_ms: AtomicI64,
    configured: bool,
}

impl NodeLease {
    pub(super) fn node(&self) -> i64 {
        self.node.load(Ordering::Acquire)
    }

    fn valid(&self) -> bool {
        self.valid_until_ms.load(Ordering::Acquire) > chrono::Utc::now().timestamp_millis()
    }
}

/// Claim `node` for the current term and the next. On success, returns until
/// when the claims cover this server, by the local clock.
async fn hold_node(client: &Client, owner: &str, node: i64) -> Result<Option<i64>, ApiError> {
    let local = chrono::Utc::now().timestamp_millis();
    let now = client.now_ms().await?;
    let term = now / NODE_TERM_MS;
    if client.claim("node", node, term, owner).await?
        && client.claim("node", node, term + 1, owner).await?
    {
        return Ok(Some(
            local + (term + 2) * NODE_TERM_MS - now - LEASE_MARGIN_MS,
        ));
    }
    Ok(None)
}

/// Lease a node ID that no other running server holds, configured or leased,
/// for this term or the next.
async fn lease_node(client: &Client, owner: &str) -> Result<(i64, i64), ApiError> {
    let term = client.now_ms().await? / NODE_TERM_MS;
    let taken = client
        .claimed_by_others("node", term..=term + 1, owner)
        .await?;
    let start = (uuid::Uuid::new_v4().as_u128() % (1 << NODE_BITS)) as i64;
    for node in (0..1 << NODE_BITS).map(|offset| (start + offset) % (1 << NODE_BITS)) {
        if taken.contains(&node) {
            continue;
        }
        if let Some(valid_until) = hold_node(client, owner, node).await? {
            return Ok((node, valid_until));
        }
    }
    Err(ApiError::unavailable())
}

/// Renew the lease a term ahead. A leased ID moves to another free node if
/// another server took it; a configured ID waits until it is free again.
async fn renew_node(client: Client, owner: String, lease: Arc<NodeLease>) {
    loop {
        tokio::time::sleep(std::time::Duration::from_millis(NODE_TERM_MS as u64 / 3)).await;
        let current = lease.node();
        match hold_node(&client, &owner, current).await {
            Ok(Some(valid_until)) => lease.valid_until_ms.store(valid_until, Ordering::Release),
            Ok(None) if lease.configured => {
                tracing::error!(backend = "clickhouse", node = current, "node_id_in_use")
            }
            Ok(None) => match lease_node(&client, &owner).await {
                Ok((next, valid_until)) => {
                    tracing::warn!(
                        backend = "clickhouse",
                        from = current,
                        to = next,
                        "node_id_moved"
                    );
                    lease.node.store(next, Ordering::Release);
                    lease.valid_until_ms.store(valid_until, Ordering::Release);
                }
                Err(_) => tracing::error!(backend = "clickhouse", "node_id_lease_failed"),
            },
            Err(_) => tracing::warn!(backend = "clickhouse", "node_id_renewal_failed"),
        }
    }
}

impl Ids {
    fn new(lease: Arc<NodeLease>) -> Self {
        Self {
            lease,
            last_unit: AtomicI64::new(0),
        }
    }

    fn next(&self, count: usize) -> Result<Vec<i64>, ApiError> {
        // Without a valid lease another server may hold this node ID.
        if !self.lease.valid() {
            return Err(ApiError::unavailable());
        }
        let node = self.lease.node();
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
                    .map(|id| id | node)
                    .ok_or_else(ApiError::internal)
            })
            .collect()
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
        object.insert(name.clone(), value);
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
        projects: Arc<crate::projects::Registry>,
    ) -> Result<Arc<Self>, Box<dyn std::error::Error>> {
        let settings = &config.clickhouse;
        let client = Client::new(settings.clone())?;
        // Servers never create or change tables: `init` and `migrate` do.
        client
            .with_timeout(super::CONTROL_TIMEOUT)
            .check_schema()
            .await?;
        let (sender, receiver) = mpsc::channel(config.batch.queue_requests);
        // Every server leases its node ID, configured or not, so leased IDs
        // never collide with configured ones; configured IDs never move.
        let control = client.with_timeout(super::CONTROL_TIMEOUT);
        let instance = uuid::Uuid::new_v4().to_string();
        let (node, valid_until) = match settings.node_id {
            Some(node) => {
                let node = i64::from(node);
                let valid_until = hold_node(&control, &instance, node)
                    .await
                    .map_err(|_| "Cannot lease the configured node ID: ClickHouse is unavailable")?
                    .ok_or("clickhouse.node_id is used by another running server; give each server its own")?;
                (node, valid_until)
            }
            None => lease_node(&control, &instance).await.map_err(|_| {
                "Cannot lease a node ID: all 64 are in use or ClickHouse is unavailable; set clickhouse.node_id"
            })?,
        };
        let node = Arc::new(NodeLease {
            node: AtomicI64::new(node),
            valid_until_ms: AtomicI64::new(valid_until),
            configured: settings.node_id.is_some(),
        });
        tokio::spawn(renew_node(control.clone(), instance.clone(), node.clone()));
        let ids = Ids::new(node.clone());
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
            cache: crate::cache::ReportCache::new(config.report_cache_seconds),
            budget: Arc::new(Semaphore::new(config.batch.queue_bytes)),
            batch: config.batch.clone(),
            control,
            instance,
            node,
            // Rollups are read only by lagged reports, so they refresh only then.
            rollup_interval: if settings.rollup_max_lag.is_zero() {
                std::time::Duration::ZERO
            } else {
                settings.rollup_interval
            },
            rollup_max_lag: settings.rollup_max_lag,
            rollup_reads: (!settings.rollup_interval.is_zero()).into(),
            client,
            projects,
            sender,
        }))
    }

    /// This server's node ID: configured, or leased while it runs. A leased ID
    /// can move to another free value if the lease could not be renewed.
    pub fn node_id(&self) -> i64 {
        self.node.node()
    }

    /// Time between rollup refreshes; zero when rollups are disabled.
    pub fn rollup_interval(&self) -> std::time::Duration {
        self.rollup_interval
    }

    pub async fn health(&self) -> Result<(), ApiError> {
        self.control
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

    /// Run the expiration sweep if this server claims it for the current
    /// `interval`, so servers sharing a database do not repeat it. Returns
    /// `None` when another server has the claim.
    pub async fn purge_expired_once(
        &self,
        interval: std::time::Duration,
    ) -> Result<Option<usize>, ApiError> {
        let term = self.control.now_ms().await? / (interval.as_millis() as i64).max(1000);
        if !self.control.claim("sweep", 0, term, &self.instance).await? {
            return Ok(None);
        }
        self.purge_expired().await.map(Some)
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
        // Delete expired events first: that is what retention promises. Rollup
        // reads already skip hours with expired events; then drop the derived
        // rows of views and hours whose events have all expired.
        self.purge_rollups(|params, column| retained_by(&self.projects, params, column))
            .await?;
        Ok(expired as usize)
    }
}

/// Rows still within each project's retention period. Expired rows are hidden
/// from reads and duplicate checks before the background sweep deletes them.
fn retained(projects: &crate::projects::Registry, params: &mut Params) -> Result<String, ApiError> {
    retained_by(projects, params, "received_at_ms")
}

/// [`retained`] for another receipt time expression, such as a rollup's latest receipt.
fn retained_by(
    projects: &crate::projects::Registry,
    params: &mut Params,
    received: &str,
) -> Result<String, ApiError> {
    let policy = projects
        .retention
        .read()
        .map_err(|_| ApiError::internal())?;
    let now = chrono::Utc::now().timestamp_millis();
    let (ids, cutoffs): (Vec<String>, Vec<i64>) = policy
        .overrides()
        .map(|id| (id.to_owned(), policy.project_cutoff(id, now)))
        .unzip();
    Ok(format!(
        "{received} >= transform(project_id, {}, {}, {})",
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
            &[
                ("async_insert", "1"),
                ("wait_for_async_insert", "1"),
                ("async_insert_busy_timeout_min_ms", "10"),
                ("async_insert_busy_timeout_max_ms", "50"),
            ]
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
        let lease = |valid_until_ms| {
            Arc::new(NodeLease {
                node: AtomicI64::new(7),
                valid_until_ms: AtomicI64::new(valid_until_ms),
                configured: true,
            })
        };
        // An expired lease assigns no IDs: another server may hold the node.
        assert!(Ids::new(lease(0)).next(1).is_err());
        let ids = Ids::new(lease(i64::MAX));
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
