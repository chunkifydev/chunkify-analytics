//! ClickHouse storage over its HTTP interface. Every query binds caller values as
//! typed query parameters; only fixed identifiers from this crate become SQL text.
//! Analytics servers keep no local state, so several can share one database.
use crate::{api::ApiError, backend::ClickHouseConfig};
use serde_json::Value;
use std::{fmt::Write as _, sync::atomic::Ordering, time::Duration};

mod claims;
pub mod migrations;
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
            // A field without a column fails the insert instead of being
            // dropped, so a table behind the code never loses data silently.
            query.append_pair("input_format_skip_unknown_fields", "0");
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

fn registry_error(_: ApiError) -> Box<dyn std::error::Error> {
    "Cannot reach the ClickHouse project registry".into()
}

/// Apply pending schema migrations and return the names of those applied.
/// Only `init` passes `create_database`.
pub fn migrate(config: &ClickHouseConfig, create_database: bool) -> Result<Vec<String>> {
    let config = config.clone();
    block_on(move || async move {
        Client::new(config)?
            .migrate(create_database)
            .await
            .map_err(Into::into)
    })
}

/// The latest registry document and its version, if `init` has run. It never
/// creates the database or tables.
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

    /// Create the test database and its tables, as `init` does.
    pub async fn migrate(config: &crate::backend::ClickHouseConfig) {
        super::Client::new(config.clone())
            .unwrap()
            .migrate(true)
            .await
            .unwrap();
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
