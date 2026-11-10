//! ClickHouse storage over its HTTP interface. Every query binds caller values as
//! typed query parameters; only fixed identifiers from this crate become SQL text.
//! Analytics servers keep no local state, so several can share one database.
use crate::{api::ApiError, backend::ClickHouseConfig};
use serde_json::Value;
use std::{fmt::Write as _, sync::atomic::Ordering, time::Duration};

mod claims;
pub mod reports;
pub mod rollup;
pub mod store;
#[cfg(test)]
mod tests;

pub use store::Store;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Settings sent with every request. JSON numbers stay numbers, float text is
/// parsed exactly, and outer joins return NULL instead of type defaults.
const QUERY_SETTINGS: &[(&str, &str)] = &[
    ("output_format_json_quote_64bit_integers", "0"),
    ("output_format_json_quote_64bit_floats", "0"),
    ("output_format_json_quote_denormals", "0"),
    ("precise_float_parsing", "1"),
    ("join_use_nulls", "1"),
    // A read whose caller went away stops, so it no longer holds resources
    // after its admission permit is released.
    ("cancel_http_readonly_queries_on_client_close", "1"),
];

/// Typed query parameters, rendered as `{pN:Type}` placeholders.
#[derive(Default, Clone)]
pub struct Params {
    values: Vec<(String, String)>,
}

impl Params {
    fn bind(&mut self, kind: &str, value: String) -> String {
        let name = format!("p{}", self.values.len());
        let placeholder = format!("{{{name}:{kind}}}");
        self.values.push((format!("param_{name}"), value));
        placeholder
    }
    pub fn text(&mut self, value: &str) -> String {
        // Parameter values use ClickHouse's escaped text format.
        let mut escaped = String::with_capacity(value.len());
        for c in value.chars() {
            match c {
                '\\' => escaped.push_str("\\\\"),
                '\t' => escaped.push_str("\\t"),
                '\n' => escaped.push_str("\\n"),
                '\r' => escaped.push_str("\\r"),
                '\0' => escaped.push_str("\\0"),
                c => escaped.push(c),
            }
        }
        self.bind("String", escaped)
    }
    pub fn int(&mut self, value: i64) -> String {
        self.bind("Int64", value.to_string())
    }
    pub fn float(&mut self, value: f64) -> String {
        // Debug formatting is the shortest exact representation, with exponents.
        self.bind("Float64", format!("{value:?}"))
    }
    pub fn boolean(&mut self, value: bool) -> String {
        self.bind("Bool", value.to_string())
    }
    pub fn texts(&mut self, values: &[String]) -> String {
        let mut array = String::from("[");
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                array.push(',');
            }
            array.push('\'');
            for c in value.chars() {
                match c {
                    '\\' | '\'' => {
                        array.push('\\');
                        array.push(c);
                    }
                    c => array.push(c),
                }
            }
            array.push('\'');
        }
        array.push(']');
        self.bind("Array(String)", array)
    }
    pub fn ints(&mut self, values: &[i64]) -> String {
        let mut array = String::from("[");
        for (index, value) in values.iter().enumerate() {
            if index > 0 {
                array.push(',');
            }
            let _ = write!(array, "{value}");
        }
        array.push(']');
        self.bind("Array(Int64)", array)
    }
}

#[derive(Clone)]
pub struct Client {
    http: reqwest::Client,
    config: ClickHouseConfig,
    /// Limit on a whole request, for calls that must not hang when ClickHouse
    /// stalls. Report and export reads stream without one.
    timeout: Option<Duration>,
}

/// Limit for registry, schema, health and coordination requests.
pub const CONTROL_TIMEOUT: Duration = Duration::from_secs(30);

/// Errors that never include SQL text or values, which may contain event data.
fn failure(status: Option<reqwest::StatusCode>, body: &str) -> ApiError {
    let code = body
        .split_once("Code: ")
        .and_then(|(_, rest)| rest.split(|c: char| !c.is_ascii_digit()).next())
        .unwrap_or("unknown");
    tracing::warn!(
        backend = "clickhouse",
        status = status.map(|s| s.as_u16()),
        code,
        "clickhouse_request_failed"
    );
    match status {
        None => ApiError::unavailable(),
        Some(status) if status.is_server_error() && code == "unknown" => ApiError::unavailable(),
        Some(_) => ApiError::internal(),
    }
}

impl Client {
    pub fn new(config: ClickHouseConfig) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(30))
            .build()
            .map_err(|_| "Cannot configure the ClickHouse HTTP client")?;
        Ok(Self {
            http,
            config,
            timeout: None,
        })
    }

    /// The same client, with a limit on each whole request.
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        Self {
            timeout: Some(timeout),
            ..self.clone()
        }
    }

    fn request(
        &self,
        sql: String,
        params: &Params,
        settings: &[(&str, &str)],
        database: bool,
    ) -> reqwest::RequestBuilder {
        let mut url = self.config.url.clone();
        {
            let mut query = url.query_pairs_mut();
            if database {
                query.append_pair("database", &self.config.database);
            }
            for (name, value) in QUERY_SETTINGS.iter().chain(settings) {
                query.append_pair(name, value);
            }
            for (name, value) in &params.values {
                query.append_pair(name, value);
            }
        }
        let mut request = self.http.post(url).body(sql);
        if let Some(timeout) = self.timeout {
            request = request.timeout(timeout);
        }
        if let Some(user) = &self.config.user {
            request = request.header("X-ClickHouse-User", user);
        }
        if let Some(password) = &self.config.password {
            request = request.header("X-ClickHouse-Key", password);
        }
        request
    }

    async fn send(
        &self,
        sql: String,
        params: &Params,
        settings: &[(&str, &str)],
        database: bool,
    ) -> std::result::Result<reqwest::Response, ApiError> {
        let response = self
            .request(sql, params, settings, database)
            .send()
            .await
            .map_err(|_| failure(None, ""))?;
        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(failure(Some(status), &body));
        }
        Ok(response)
    }

    /// Run a statement that returns no rows.
    pub async fn execute(
        &self,
        sql: impl Into<String>,
        params: &Params,
    ) -> std::result::Result<(), ApiError> {
        self.execute_with(sql, params, &[]).await
    }

    pub async fn execute_with(
        &self,
        sql: impl Into<String>,
        params: &Params,
        settings: &[(&str, &str)],
    ) -> std::result::Result<(), ApiError> {
        let response = self.send(sql.into(), params, settings, true).await?;
        // Drain the body so errors reported after the headers are not ignored.
        let body = response.text().await.map_err(|_| failure(None, ""))?;
        if body.contains("DB::Exception") {
            return Err(failure(
                Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
                &body,
            ));
        }
        Ok(())
    }

    /// Stream rows as JSON arrays, in the order of the SELECT list.
    pub async fn rows(&self, sql: &str, params: &Params) -> std::result::Result<Rows, ApiError> {
        let response = self
            .send(
                format!("{sql}\nFORMAT JSONCompactEachRow"),
                params,
                &[],
                true,
            )
            .await?;
        Ok(Rows {
            response,
            buffer: Vec::new(),
            start: 0,
            finished: false,
        })
    }

    /// Collect a bounded result.
    pub async fn all(
        &self,
        sql: &str,
        params: &Params,
    ) -> std::result::Result<Vec<Vec<Value>>, ApiError> {
        let mut rows = self.rows(sql, params).await?;
        let mut result = Vec::new();
        while let Some(row) = rows.next().await? {
            result.push(row);
        }
        Ok(result)
    }

    pub async fn one(
        &self,
        sql: &str,
        params: &Params,
    ) -> std::result::Result<Vec<Value>, ApiError> {
        self.all(sql, params)
            .await?
            .into_iter()
            .next()
            .ok_or_else(ApiError::internal)
    }

    /// Insert newline-delimited JSON objects.
    pub async fn insert(
        &self,
        table: &str,
        body: Vec<u8>,
        settings: &[(&str, &str)],
    ) -> std::result::Result<(), ApiError> {
        let mut url = self.config.url.clone();
        {
            let mut query = url.query_pairs_mut();
            query.append_pair("database", &self.config.database);
            query.append_pair("query", &format!("INSERT INTO {table} FORMAT JSONEachRow"));
            for (name, value) in QUERY_SETTINGS.iter().chain(settings) {
                query.append_pair(name, value);
            }
        }
        let mut request = self.http.post(url).body(body);
        if let Some(timeout) = self.timeout {
            request = request.timeout(timeout);
        }
        if let Some(user) = &self.config.user {
            request = request.header("X-ClickHouse-User", user);
        }
        if let Some(password) = &self.config.password {
            request = request.header("X-ClickHouse-Key", password);
        }
        let response = request.send().await.map_err(|_| failure(None, ""))?;
        let status = response.status();
        let body = response.text().await.map_err(|_| failure(None, ""))?;
        if !status.is_success() || body.contains("DB::Exception") {
            return Err(failure(Some(status), &body));
        }
        Ok(())
    }

    /// Create the database and tables. Safe to run concurrently from many servers.
    /// Fails with [`OUTDATED_SCHEMA`] when the database was created by a
    /// version with another table layout: tables are never upgraded in place.
    pub async fn ensure_schema(&self) -> std::result::Result<(), ApiError> {
        let database = &self.config.database;
        let none = Params::default();
        self.send(
            format!("CREATE DATABASE IF NOT EXISTS {database}"),
            &none,
            &[],
            false,
        )
        .await?
        .text()
        .await
        .map_err(|_| failure(None, ""))?;
        // In a Replicated database, plain MergeTree tables keep their data on
        // one node; replicate every table so all servers share the same data.
        let mut params = Params::default();
        let sql = format!(
            "SELECT engine FROM system.databases WHERE name = {}",
            params.text(database)
        );
        let replicated = self
            .one(&sql, &params)
            .await?
            .first()
            .and_then(Value::as_str)
            == Some("Replicated");
        let engine = |statement: String| {
            if replicated {
                statement
                    .replace("ENGINE = MergeTree", "ENGINE = ReplicatedMergeTree")
                    .replace(
                        "ENGINE = AggregatingMergeTree",
                        "ENGINE = ReplicatedAggregatingMergeTree",
                    )
            } else {
                statement
            }
        };
        // A database with events but another (or no) schema version was
        // created by another release. Check before creating anything.
        let mut params = Params::default();
        let sql = format!(
            "SELECT (SELECT count() FROM system.tables WHERE database = {db} AND name = 'events'),
                (SELECT count() FROM system.tables WHERE database = {db} AND name = 'schema_version')",
            db = params.text(database)
        );
        let tables = self.one(&sql, &params).await?;
        let exists = |index: usize| {
            tables
                .get(index)
                .and_then(Value::as_u64)
                .is_some_and(|n| n > 0)
        };
        let outdated = || ApiError::new(OUTDATED_STATUS, OUTDATED_SCHEMA.into());
        let stored = || async {
            Ok::<_, ApiError>(
                self.one("SELECT max(version) FROM schema_version", &none)
                    .await?
                    .first()
                    .and_then(Value::as_u64)
                    .filter(|version| *version > 0),
            )
        };
        if exists(1) {
            // Another release's version, even before it created its tables.
            if stored()
                .await?
                .is_some_and(|version| version != SCHEMA_VERSION)
            {
                return Err(outdated());
            }
        } else if exists(0) {
            // Events without a version: a release from before schema versions.
            return Err(outdated());
        }
        // Record the layout before creating any other table, so a database
        // with events always has its version, even after an interrupted start.
        self.execute(
            engine(
                "CREATE TABLE IF NOT EXISTS schema_version (version UInt64) ENGINE = MergeTree ORDER BY version"
                    .into(),
            ),
            &none,
        )
        .await?;
        // Concurrent servers may each record the version; duplicates are
        // harmless. Read it back: a server of another release starting on the
        // same empty database at the same time may have recorded its own.
        let mut params = Params::default();
        let version = params.int(SCHEMA_VERSION as i64);
        self.execute(
            format!(
                "INSERT INTO schema_version SELECT {version} WHERE (SELECT count() FROM schema_version) = 0"
            ),
            &params,
        )
        .await?;
        let versions = self
            .one(
                "SELECT min(version), max(version) FROM schema_version",
                &none,
            )
            .await?;
        if versions
            .iter()
            .any(|version| version.as_u64() != Some(SCHEMA_VERSION))
        {
            return Err(outdated());
        }
        let mut tables = vec![
            events_table(),
            "CREATE TABLE IF NOT EXISTS registry (
                version UInt64,
                write_id String,
                document String CODEC(ZSTD(3)),
                written_at DateTime64(3) DEFAULT now64(3)
            ) ENGINE = MergeTree ORDER BY version
            SETTINGS non_replicated_deduplication_window = 100000"
                .to_owned(),
            claims::TABLE.to_owned(),
        ];
        tables.extend(rollup::tables());
        for table in tables {
            self.execute(engine(table), &none).await?;
        }
        Ok(())
    }
}

pub struct Rows {
    response: reqwest::Response,
    buffer: Vec<u8>,
    start: usize,
    finished: bool,
}

impl Rows {
    pub async fn next(&mut self) -> std::result::Result<Option<Vec<Value>>, ApiError> {
        loop {
            if let Some(end) = self.buffer[self.start..].iter().position(|b| *b == b'\n') {
                let line = &self.buffer[self.start..self.start + end];
                self.start += end + 1;
                if line.iter().all(u8::is_ascii_whitespace) {
                    continue;
                }
                return parse_row(line).map(Some);
            }
            if self.finished {
                let line = &self.buffer[self.start..];
                if line.iter().all(u8::is_ascii_whitespace) {
                    return Ok(None);
                }
                let row = parse_row(line);
                self.start = self.buffer.len();
                return row.map(Some);
            }
            // Compact consumed lines before reading more of the response.
            self.buffer.drain(..self.start);
            self.start = 0;
            match self.response.chunk().await.map_err(|_| failure(None, ""))? {
                Some(chunk) => self.buffer.extend_from_slice(&chunk),
                None => self.finished = true,
            }
        }
    }
}

fn parse_row(line: &[u8]) -> std::result::Result<Vec<Value>, ApiError> {
    serde_json::from_slice(line).map_err(|_| {
        // An exception after the headers is written into the body as text.
        failure(
            Some(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            &String::from_utf8_lossy(line),
        )
    })
}

fn clickhouse_type(name: &str, kind: &str) -> String {
    let required = matches!(
        name,
        "event_id"
            | "project_id"
            | "view_id"
            | "video_id"
            | "event_name"
            | "sequence"
            | "occurred_at_ms"
            | "received_at_ms"
            | "event_remainder"
            | "context_format"
    );
    let base = kind;
    let low_cardinality = matches!(
        name,
        "event_name"
            | "project_id"
            | "context_browser_name"
            | "context_os_name"
            | "context_device_type"
    );
    match (required, low_cardinality) {
        (true, true) => format!("LowCardinality({base})"),
        (true, false) => base.into(),
        (false, true) => format!("LowCardinality(Nullable({base}))"),
        (false, false) => format!("Nullable({base})"),
    }
}

/// Location and page context, extracted once at insert time from the stored
/// context JSON. Only JSON strings are dimension values, as in local storage.
pub const CONTEXT_DIMENSIONS: &[(&str, &str)] = &[
    ("ctx_continent", "continent"),
    ("ctx_country", "country"),
    ("ctx_city", "city"),
    ("ctx_page_origin", "page_origin"),
    ("ctx_page_path", "page_path"),
];

/// The playback rate as dimension text, such as `1` or `1.5`, rounded to two
/// decimals so rollups, filters and breakdowns share one value per rate.
/// Storage keeps integral floats such as `2.0` in the event JSON, so the rate
/// falls back to that JSON when its typed column is empty.
const PLAYBACK_RATE_TEXT: &str = "d_playback_rate Nullable(String) MATERIALIZED toString(round(if(isNotNull(e_playback_rate), e_playback_rate, if(JSONType(event_remainder, 'playback_rate') IN ('Int64', 'UInt64', 'Double'), JSONExtractFloat(event_remainder, 'playback_rate'), NULL)), 2))";

/// Custom dimensions of a view start as text: strings unchanged, numbers and
/// booleans as sent (`5`, `true`). Other events keep an empty map.
const CUSTOM_MAP: &str = "c_custom Map(String, String) MATERIALIZED if(event_name = 'chunkify.video.view_started', CAST(arrayMap(entry -> (entry.1, if(JSONType(entry.2) = 'String', JSONExtractString(entry.2), entry.2)), JSONExtractKeysAndValuesRaw(event_remainder, 'custom')), 'Map(String, String)'), map())";

/// Numeric report fields of snapshot and end events, the only events reports
/// measure, extracted once at insert time. Storage keeps integral floats in the
/// event JSON to preserve their representation, so a field falls back to that
/// JSON when its typed column is empty.
pub const REPORT_NUMBERS: &[&str] = &[
    "startup_time_ms",
    "rebuffer_duration_ms",
    "media_duration_seconds",
    "furthest_progress_ratio",
];

fn events_table() -> String {
    let mut columns = vec!["ingest_id Int64".to_owned()];
    for (name, kind) in crate::storage::COLUMNS.iter() {
        columns.push(format!("{name} {}", clickhouse_type(name, kind)));
    }
    columns.push("otlp_payload Nullable(String) CODEC(ZSTD(3))".into());
    for (column, key) in CONTEXT_DIMENSIONS {
        columns.push(format!(
            "{column} Nullable(String) MATERIALIZED if(JSONType(ifNull(context_remainder, '{{}}'), '{key}') = 'String', JSONExtractString(ifNull(context_remainder, '{{}}'), '{key}'), NULL)"
        ));
    }
    columns.push(PLAYBACK_RATE_TEXT.into());
    columns.push(CUSTOM_MAP.into());
    for field in REPORT_NUMBERS {
        columns.push(format!(
            "n_{field} Nullable(Float64) MATERIALIZED if(event_name NOT IN ('chunkify.video.view_snapshot', 'chunkify.video.view_ended'), NULL, if(isNotNull(e_{field}), e_{field}, if(JSONType(event_remainder, '{field}') IN ('Int64', 'UInt64', 'Double'), JSONExtractFloat(event_remainder, '{field}'), NULL)))"
        ));
    }
    // ClickHouse's own clock when the row is written, which rollups use to find
    // rows stored since their last refresh. Callers cannot set it.
    columns.push("stored_at_ms Int64 MATERIALIZED toUnixTimestamp64Milli(now64(3))".into());
    columns.push("INDEX events_event event_id TYPE bloom_filter(0.0001) GRANULARITY 1".into());
    columns.push("INDEX events_stored stored_at_ms TYPE minmax GRANULARITY 1".into());
    columns.push("INDEX events_view view_id TYPE bloom_filter GRANULARITY 4".into());
    columns.push("INDEX events_video video_id TYPE bloom_filter GRANULARITY 4".into());
    columns.push("INDEX events_occurred occurred_at_ms TYPE minmax GRANULARITY 4".into());
    // Project first keeps each tenant contiguous; ingest order serves event pages
    // and watermarks. Monthly receipt partitions keep expiration work bounded.
    format!(
        "CREATE TABLE IF NOT EXISTS events ({})
        ENGINE = MergeTree
        PARTITION BY toYYYYMM(toDateTime(intDiv(received_at_ms, 1000), 'UTC'))
        ORDER BY (project_id, ingest_id)",
        columns.join(",\n            ")
    )
}

/// Run registry operations on a private runtime. Administration commands and
/// startup call these from synchronous code, inside or outside async contexts.
fn block_on<T: Send, F: std::future::Future<Output = Result<T>>>(
    work: impl FnOnce() -> F + Send,
) -> Result<T> {
    let result: std::result::Result<T, String> = std::thread::scope(|scope| {
        scope
            .spawn(|| {
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|_| "Cannot start ClickHouse client".to_owned())?
                    .block_on(work())
                    .map_err(|error| error.to_string())
            })
            .join()
            .unwrap_or_else(|_| Err("ClickHouse client failed".into()))
    });
    result.map_err(Into::into)
}

fn registry_error(error: ApiError) -> Box<dyn std::error::Error> {
    if error.status == OUTDATED_STATUS {
        return OUTDATED_SCHEMA.into();
    }
    "Cannot reach the ClickHouse project registry".into()
}

/// The table layout this release creates. Increase it with every change to a
/// table definition: databases created with another layout must be recreated.
pub const SCHEMA_VERSION: u64 = 2;
/// The status that marks [`OUTDATED_SCHEMA`]; no other schema error uses it.
const OUTDATED_STATUS: reqwest::StatusCode = reqwest::StatusCode::CONFLICT;
/// Startup error for a database created by a release with another layout.
pub const OUTDATED_SCHEMA: &str = "The ClickHouse database was created by an analytics release with a different table layout and cannot be upgraded in place. Use a new clickhouse.database, or drop this one, then start again";

/// Startup message for a schema error: the outdated-layout error as is, any
/// other failure as a connection hint.
pub fn schema_error(error: ApiError) -> Box<dyn std::error::Error> {
    if error.status == OUTDATED_STATUS {
        OUTDATED_SCHEMA.into()
    } else {
        "Cannot create the ClickHouse schema; check clickhouse.url, credentials and permissions"
            .into()
    }
}

/// Create the schema used by every analytics server for this database.
pub fn prepare(config: &ClickHouseConfig) -> Result<()> {
    let config = config.clone();
    block_on(move || async move {
        Client::new(config)?
            .with_timeout(CONTROL_TIMEOUT)
            .ensure_schema()
            .await
            .map_err(schema_error)
    })
}

/// The latest registry document and its version, if `init` has run. Unlike
/// `load_registry`, it never creates the database or tables.
pub fn read_registry(config: &ClickHouseConfig) -> Result<Option<(u64, Vec<u8>)>> {
    let config = config.clone();
    block_on(move || async move {
        let client = Client::new(config.clone())?.with_timeout(CONTROL_TIMEOUT);
        // A missing database or registry is not initialized; reads never
        // create one, so a mistyped database name creates nothing.
        // Without a default database: the configured one may not exist.
        let mut params = Params::default();
        let sql = format!(
            "SELECT count() FROM system.tables WHERE database = {} AND name = 'registry' FORMAT TSV",
            params.text(&config.database)
        );
        let exists = client
            .send(sql, &params, &[], false)
            .await
            .map_err(registry_error)?
            .text()
            .await
            .map_err(|_| "Cannot reach the ClickHouse project registry")?;
        if exists.trim() == "0" {
            return Ok(None);
        }
        latest(&client).await.map_err(registry_error)
    })
}

/// The latest registry document and its version, if `init` has run.
pub fn load_registry(config: &ClickHouseConfig) -> Result<Option<(u64, Vec<u8>)>> {
    let config = config.clone();
    let loaded = block_on(move || async move {
        let client = Client::new(config)?.with_timeout(CONTROL_TIMEOUT);
        client.ensure_schema().await.map_err(registry_error)?;
        latest(&client).await.map_err(registry_error)
    })?;
    Ok(loaded)
}

async fn latest(client: &Client) -> std::result::Result<Option<(u64, Vec<u8>)>, ApiError> {
    let rows = client
        .all(
            "SELECT version, document FROM registry ORDER BY version DESC LIMIT 1",
            &Params::default(),
        )
        .await?;
    rows.into_iter()
        .next()
        .map(|row| {
            let version = row
                .first()
                .and_then(Value::as_u64)
                .ok_or_else(ApiError::internal)?;
            let document = row
                .get(1)
                .and_then(Value::as_str)
                .ok_or_else(ApiError::internal)?;
            Ok((version, document.as_bytes().to_vec()))
        })
        .transpose()
}

const CONFLICT: &str =
    "The project registry changed on another analytics server. Retry the command.";

/// Whether a registry save lost a compare-and-set race.
pub fn is_conflict(error: &(dyn std::error::Error + 'static)) -> bool {
    error.to_string() == CONFLICT
}

/// Compare-and-set the registry. Inserts are deduplicated by version, so when
/// two servers race only one document is stored for that version.
pub fn save_registry(config: &ClickHouseConfig, expected: u64, document: &[u8]) -> Result<u64> {
    let config = config.clone();
    let version = expected + 1;
    let document = String::from_utf8(document.to_vec())?;
    let write_id = uuid::Uuid::new_v4().to_string();
    let saved = block_on(move || async move {
        let client = Client::new(config.clone())?.with_timeout(CONTROL_TIMEOUT);
        let current = latest(&client)
            .await
            .map_err(registry_error)?
            .map_or(0, |(v, _)| v);
        if current != expected {
            return Ok(false);
        }
        let row =
            serde_json::json!({"version": version, "write_id": write_id, "document": document});
        let token = format!("chunkify-registry-{version}");
        client
            .insert(
                "registry",
                serde_json::to_vec(&row)?,
                &[
                    ("async_insert", "0"),
                    ("insert_deduplicate", "1"),
                    ("insert_deduplication_token", &token),
                ],
            )
            .await
            .map_err(registry_error)?;
        let mut params = Params::default();
        let placeholder = params.int(version as i64);
        let winner = client
            .all(
                &format!("SELECT write_id FROM registry WHERE version = {placeholder}"),
                &params,
            )
            .await
            .map_err(registry_error)?;
        Ok(winner.len() == 1
            && winner[0].first().and_then(Value::as_str) == Some(write_id.as_str()))
    })?;
    if !saved {
        return Err(
            "The project registry changed on another analytics server. Retry the command.".into(),
        );
    }
    Ok(version)
}

impl ClickHouseConfig {
    pub fn loaded_version(&self) -> u64 {
        self.registry_version.load(Ordering::Acquire)
    }
    pub fn set_loaded_version(&self, version: u64) {
        self.registry_version.fetch_max(version, Ordering::AcqRel);
    }
}

#[cfg(test)]
pub mod testing {
    /// Configuration for a new, empty database on the ClickHouse server at
    /// CHUNKIFY_TEST_CLICKHOUSE_URL (default `http://127.0.0.1:8123`).
    pub fn config(extra: &str) -> crate::config::Config {
        let directory =
            std::env::temp_dir().join(format!("chunkify-unit-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let url = std::env::var("CHUNKIFY_TEST_CLICKHOUSE_URL")
            .unwrap_or_else(|_| "http://127.0.0.1:8123".into());
        // A test can share another test server's database by naming it.
        let database = if extra.contains("clickhouse.database") {
            String::new()
        } else {
            format!(
                "clickhouse.database = \"chunkify_unit_{}\"\n",
                uuid::Uuid::new_v4().simple()
            )
        };
        let file = directory.join("config.toml");
        let env = directory.join("empty.env");
        std::fs::write(&env, "").unwrap();
        // Tests read right after inserting, so reports stay exact unless a
        // test asks for lagged rollup reads.
        let lag = if extra.contains("rollup_max_lag_seconds") {
            ""
        } else {
            "clickhouse.rollup_max_lag_seconds = 0\n"
        };
        std::fs::write(
            &file,
            format!("clickhouse.url = {url:?}\n{database}{lag}{extra}\n"),
        )
        .unwrap();
        let settings = crate::settings::Settings::load(Some(&env), Some(&file)).unwrap();
        let config = crate::config::Config::load_with(&settings, None).unwrap();
        std::fs::remove_dir_all(directory).unwrap();
        config
    }

    /// Drops the test database when it goes out of scope, even after a panic.
    pub struct Disposable(pub crate::backend::ClickHouseConfig);

    impl Drop for Disposable {
        fn drop(&mut self) {
            drop_database(&self.0);
        }
    }

    pub fn drop_database(config: &crate::backend::ClickHouseConfig) {
        let config = config.clone();
        let _ = super::block_on(move || async move {
            let client = super::Client::new(config.clone())?.with_timeout(super::CONTROL_TIMEOUT);
            client
                .send(
                    format!("DROP DATABASE IF EXISTS {}", config.database),
                    &super::Params::default(),
                    &[],
                    false,
                )
                .await
                .map_err(|_| "drop failed")?;
            Ok(())
        });
    }
}
