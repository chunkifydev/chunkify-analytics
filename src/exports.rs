//! Scoped CSV download grants. Rows stream on demand; export files are never retained.
use crate::{
    api::ApiError,
    projects::{Registry, Resolved},
    query,
    store::Store,
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::SecureRandom;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    sync::{Arc, LazyLock, Mutex},
    time::{Duration, Instant},
};
use tokio::sync::{Semaphore, mpsc};

#[derive(Clone)]
pub struct DownloadBase(pub reqwest::Url);

const TTL: Duration = Duration::from_secs(1800);
const MAX_GRANTS: usize = 1024;
static CONTRACT: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("../openapi.json")).expect("bundled contract")
});

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExportRequest {
    pub resource: String,
    #[serde(default)]
    pub query: Map<String, Value>,
}

impl ExportRequest {
    pub fn path(&self) -> Result<&'static str, ApiError> {
        match self.resource.as_str() {
            "summary" => Ok("/v1/summary"),
            "quality" => Ok("/v1/quality"),
            "renditions" => Ok("/v1/renditions"),
            "retention" => Ok("/v1/retention"),
            "engagement" => Ok("/v1/engagement"),
            "errors" => Ok("/v1/errors"),
            "events" => Ok("/v1/events"),
            "videos" => Ok("/v1/videos"),
            "views" => Ok("/v1/views"),
            "timeseries" => Ok("/v1/timeseries"),
            "dimensions" => Ok("/v1/dimensions/{dimension}"),
            _ => Err(ApiError::bad("Unsupported export resource")),
        }
    }

    pub fn validate(&self) -> Result<(), ApiError> {
        if self.query.contains_key("limit") || self.query.contains_key("cursor") {
            return Err(ApiError::bad(
                "Exports include all matching rows; limit and cursor are not accepted",
            ));
        }
        let spec = &*CONTRACT;
        let operation = &spec["paths"][self.path()?]["get"];
        let parameters: Vec<&Value> = operation["parameters"]
            .as_array()
            .expect("parameters")
            .iter()
            .map(|p| spec.pointer(&p["$ref"].as_str().unwrap()[1..]).unwrap())
            .collect();
        for (name, value) in &self.query {
            let p = parameters
                .iter()
                .find(|p| p["name"] == *name)
                .ok_or_else(|| ApiError::bad("Unsupported export query parameter"))?;
            if p["schema"]["type"] == "boolean" && !value.is_boolean() {
                return Err(ApiError::bad("Expected a boolean filter"));
            }
            if p["schema"]["type"] == "string" && !value.is_string() {
                return Err(ApiError::bad("Expected string filters"));
            }
        }
        for p in &parameters {
            if p["required"] == true && !self.query.contains_key(p["name"].as_str().unwrap()) {
                return Err(ApiError::bad("Missing required export query parameter"));
            }
        }
        let mut args = self.query.clone();
        if self.resource == "timeseries" {
            let q: query::TimeseriesQuery = parse(args)?;
            q.validate_export()?;
        } else {
            if self.resource == "dimensions" {
                let _: query::Dimension = serde_json::from_value(
                    args.remove("dimension")
                        .ok_or_else(|| ApiError::bad("dimension is required"))?,
                )
                .map_err(|_| ApiError::bad("Invalid dimension"))?;
            }
            if self.resource == "views" {
                let _: query::ViewsQuery = parse(args.clone())?;
                args.remove("has_fatal_error");
            }
            let q: query::Query = parse(args)?;
            q.filters()?;
            if self.resource == "retention" {
                crate::store::validate_retention_query(&q)?;
            }
            if q.group_by.is_some()
                && !matches!(self.resource.as_str(), "summary" | "quality" | "engagement")
            {
                return Err(ApiError::bad("Unsupported group_by"));
            }
        }
        Ok(())
    }
}
fn parse<T: serde::de::DeserializeOwned>(args: Map<String, Value>) -> Result<T, ApiError> {
    serde_json::from_value(Value::Object(args)).map_err(|_| ApiError::bad("Invalid export query"))
}

#[derive(Clone)]
pub struct Exports {
    store: Store,
    projects: Arc<Registry>,
    grants: Arc<Mutex<HashMap<Vec<u8>, Grant>>>,
    downloads: Arc<Semaphore>,
}
#[derive(Clone)]
struct Grant {
    fingerprint: String,
    request: ExportRequest,
    expires: Instant,
}
pub struct ReportStream {
    pub filename: String,
    pub chunks: mpsc::Receiver<Result<Vec<u8>, ApiError>>,
}

impl Exports {
    pub fn new(store: Store, projects: Arc<Registry>, limits: &crate::limits::Config) -> Self {
        Self {
            store,
            projects,
            grants: Default::default(),
            downloads: Arc::new(Semaphore::new(limits.export_concurrent as usize)),
        }
    }

    pub fn create(
        &self,
        owner: &Resolved,
        request: ExportRequest,
        base: &reqwest::Url,
    ) -> Result<Value, ApiError> {
        request.validate()?;
        let fingerprint = self
            .projects
            .export_fingerprint(owner)
            .ok_or_else(not_found)?;
        let mut bytes = [0; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut bytes)
            .map_err(|_| ApiError::internal())?;
        let token = URL_SAFE_NO_PAD.encode(bytes);
        let key = token_hash(&token);
        let mut grants = self.grants.lock().map_err(|_| ApiError::internal())?;
        grants.retain(|_, g| {
            g.expires > Instant::now() && self.projects.export_owner(&g.fingerprint).is_some()
        });
        if grants.len() >= MAX_GRANTS {
            return Err(ApiError::unavailable());
        }
        owner
            .project
            .limits
            .links
            .take("Project export link rate limit exceeded")?;
        let filename = format!("chunkify-{}.csv", request.resource);
        grants.insert(
            key,
            Grant {
                fingerprint,
                request,
                expires: Instant::now() + TTL,
            },
        );
        let url = base
            .join(&format!("v1/exports/{token}"))
            .map_err(|_| ApiError::internal())?;
        Ok(
            json!({"download_url":url.as_str(),"filename":filename,"content_type":"text/csv",
            "expires_at":(chrono::Utc::now()+chrono::Duration::seconds(TTL.as_secs() as i64)).to_rfc3339_opts(chrono::SecondsFormat::Millis,true)}),
        )
    }

    fn grant(&self, token: &str) -> Result<(Grant, Resolved), ApiError> {
        if token.len() != 43 || URL_SAFE_NO_PAD.decode(token).is_err() {
            return Err(not_found());
        }
        let grant = self
            .grants
            .lock()
            .map_err(|_| ApiError::internal())?
            .get(&token_hash(token))
            .cloned()
            .filter(|g| g.expires > Instant::now())
            .ok_or_else(not_found)?;
        let owner = self
            .projects
            .export_owner(&grant.fingerprint)
            .ok_or_else(not_found)?;
        Ok((grant, owner))
    }
    pub fn filename(&self, token: &str) -> Result<String, ApiError> {
        Ok(format!(
            "chunkify-{}.csv",
            self.grant(token)?.0.request.resource
        ))
    }

    pub async fn download(&self, token: &str) -> Result<ReportStream, ApiError> {
        let (grant, owner) = self.grant(token)?;
        let project = owner.project.id.clone();
        let permit = owner
            .project
            .limits
            .downloads
            .admit(&self.downloads, "export download")?;
        let store = self.store.export_reader(project.clone()).await?;
        let (sender, chunks) = mpsc::channel(2);
        let filename = format!("chunkify-{}.csv", grant.request.resource);
        let sink = Arc::new(Sink {
            columns: columns(&grant.request.resource, &grant.request.query),
            sender: sender.clone(),
            buffer: Mutex::new(Vec::new()),
        });
        tokio::spawn(async move {
            let _permit = permit;
            let result = async {
                sink.header().await?;
                let result = run(&store, project, grant.request, sink.clone()).await?;
                for row in table_rows(&result)? {
                    sink.push(row)?;
                }
                sink.finish().await
            }
            .await;
            if let Err(error) = result {
                let _ = sender.send(Err(error)).await;
            }
        });
        Ok(ReportStream { filename, chunks })
    }
}
fn token_hash(token: &str) -> Vec<u8> {
    ring::digest::digest(&ring::digest::SHA256, token.as_bytes())
        .as_ref()
        .to_vec()
}
fn not_found() -> ApiError {
    ApiError::new(
        axum::http::StatusCode::NOT_FOUND,
        "Export not found or expired".into(),
    )
}

pub struct Sink {
    columns: Vec<Column>,
    sender: mpsc::Sender<Result<Vec<u8>, ApiError>>,
    buffer: Mutex<Vec<u8>>,
}
impl Sink {
    async fn header(&self) -> Result<(), ApiError> {
        let mut bytes = Vec::new();
        write_csv_row(&mut bytes, self.columns.iter().map(|c| json!(c.name)));
        self.sender
            .send(Ok(bytes))
            .await
            .map_err(|_| ApiError::unavailable())
    }
    pub fn push(&self, row: Value) -> Result<(), ApiError> {
        if self.sender.is_closed() {
            return Err(ApiError::unavailable());
        }
        let mut buffer = self.buffer.lock().map_err(|_| ApiError::internal())?;
        write_csv_row(
            &mut buffer,
            self.columns.iter().map(|c| {
                c.path
                    .iter()
                    .try_fold(&row, |value, key| value.get(key))
                    .cloned()
                    .unwrap_or(Value::Null)
            }),
        );
        if buffer.len() >= 64 * 1024 {
            self.sender
                .blocking_send(Ok(std::mem::take(&mut *buffer)))
                .map_err(|_| ApiError::unavailable())?;
        }
        Ok(())
    }
    async fn finish(&self) -> Result<(), ApiError> {
        let bytes = std::mem::take(&mut *self.buffer.lock().map_err(|_| ApiError::internal())?);
        self.sender
            .send(Ok(bytes))
            .await
            .map_err(|_| ApiError::unavailable())
    }
}
pub fn emit(sink: &Option<Arc<Sink>>, rows: &mut Vec<Value>, row: Value) -> Result<(), ApiError> {
    if let Some(sink) = sink {
        sink.push(row)
    } else {
        rows.push(row);
        Ok(())
    }
}

async fn run(
    store: &Store,
    project: String,
    request: ExportRequest,
    sink: Arc<Sink>,
) -> Result<Value, ApiError> {
    let mut args = request.query;
    if request.resource == "timeseries" {
        let mut q: query::TimeseriesQuery = parse(args)?;
        q.export = Some(sink);
        return store.timeseries(project, q).await;
    }
    if request.resource == "views" {
        let mut q: query::ViewsQuery = parse(args)?;
        q.query.export = Some(sink);
        return store.views(project, q).await;
    }
    let dimension = if request.resource == "dimensions" {
        Some(
            serde_json::from_value(
                args.remove("dimension")
                    .ok_or_else(|| ApiError::bad("Missing dimension"))?,
            )
            .map_err(|_| ApiError::bad("Invalid dimension"))?,
        )
    } else {
        None
    };
    let mut q: query::Query = parse(args)?;
    q.export = Some(sink);
    match request.resource.as_str() {
        "events" => store.events(project, q).await,
        "videos" => store.summary(project, q, true).await,
        "summary" => store.summary(project, q, false).await,
        "quality" => store.quality(project, q).await,
        "renditions" => store.renditions(project, q).await,
        "retention" => store.retention(project, q).await,
        "engagement" => store.engagement(project, q).await,
        "errors" => store.errors(project, q).await,
        "dimensions" => {
            store
                .dimension(project, dimension.expect("dimension"), q)
                .await
        }
        _ => Err(ApiError::bad("Invalid resource")),
    }
}
fn table_rows(data: &Value) -> Result<Vec<Value>, ApiError> {
    for name in [
        "groups",
        "buckets",
        "videos",
        "events",
        "views",
        "errors",
        "values",
        "renditions",
    ] {
        if let Some(rows) = data.get(name).and_then(Value::as_array) {
            return Ok(rows.clone());
        }
    }
    if data.is_object() {
        Ok(vec![data.clone()])
    } else {
        Err(ApiError::internal())
    }
}

#[derive(Clone)]
struct Column {
    name: String,
    path: Vec<String>,
}
impl Column {
    fn new(name: impl Into<String>, pointer: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            path: pointer.into()[1..].split('/').map(str::to_owned).collect(),
        }
    }
}

fn scalar_fields(schema: &Value, fields: &mut BTreeSet<String>) {
    if let Some(reference) = schema["$ref"].as_str() {
        scalar_fields(CONTRACT.pointer(&reference[1..]).unwrap(), fields);
    }
    if let Some(properties) = schema["properties"].as_object() {
        for (name, property) in properties {
            let scalar =
                |kind: &str| matches!(kind, "string" | "number" | "integer" | "boolean" | "null");
            let primitive = match &property["type"] {
                Value::String(kind) => scalar(kind),
                Value::Array(kinds) => kinds.iter().all(|kind| kind.as_str().is_some_and(scalar)),
                _ => property.get("enum").is_some() || property.get("const").is_some(),
            };
            if primitive {
                fields.insert(name.clone());
            }
        }
    }
    for composition in ["allOf", "oneOf", "anyOf"] {
        if let Some(schemas) = schema[composition].as_array() {
            for schema in schemas {
                scalar_fields(schema, fields);
            }
        }
    }
}

static EVENT_COLUMNS: LazyLock<Vec<Column>> = LazyLock::new(|| {
    let mut columns = ["received_at", "project_id", "ingest_key_id"]
        .into_iter()
        .map(|name| Column::new(name, format!("/{name}")))
        .collect::<Vec<_>>();
    let mut fields = BTreeSet::new();
    scalar_fields(
        &CONTRACT["components"]["schemas"]["AnalyticsEvent"],
        &mut fields,
    );
    for name in [
        "event_id",
        "event_name",
        "occurred_at",
        "view_id",
        "video_id",
        "viewer_id",
        "sequence",
    ] {
        assert!(fields.remove(name), "documented event column");
        columns.push(Column::new(name, format!("/event/{name}")));
    }
    columns.extend(fields.into_iter().map(|name| {
        let pointer = format!("/event/{name}");
        Column::new(name, pointer)
    }));
    let mut context = BTreeSet::new();
    scalar_fields(
        &CONTRACT["components"]["schemas"]["EventContext"],
        &mut context,
    );
    columns.extend(
        context
            .into_iter()
            .map(|name| Column::new(format!("context.{name}"), format!("/event_context/{name}"))),
    );
    columns
});

fn columns(resource: &str, args: &Map<String, Value>) -> Vec<Column> {
    if resource == "events" {
        return EVENT_COLUMNS.clone();
    }
    let kind = if resource == "timeseries" {
        args.get("kind")
            .and_then(Value::as_str)
            .unwrap_or("summary")
    } else {
        resource
    };
    let fields: &[&str] = match kind {
        "videos" => &[
            "video_id",
            "views",
            "unique_viewers",
            "total_playing_time_ms",
            "playback_score/value",
            "playback_score/sample_views",
        ],
        "retention" => &["start_seconds", "end_seconds", "views", "ratio"],
        "renditions" => &[
            "width",
            "height",
            "codec",
            "bitrate_bps",
            "frame_rate",
            "views",
            "playing_time_ms",
            "playing_time_ratio",
        ],
        "views" => &[
            "view_id",
            "video_id",
            "viewer_id",
            "started_at",
            "browser_name",
            "os_name",
            "device_type",
            "playing_time_ms",
            "startup_time_ms",
            "rebuffer_duration_ms",
            "reached_end",
            "has_ended_event",
            "has_fatal_error",
            "playback_score/value",
            "playback_score/sample_views",
        ],
        "errors" => &["error_code", "fatal", "error_events", "affected_views"],
        "dimensions" => &["value", "views"],
        "quality" => &[
            "views",
            "startup_time_ms/sample_views",
            "startup_time_ms/median",
            "startup_time_ms/p95",
            "buffering/sample_views",
            "buffering/total_playing_time_ms",
            "buffering/total_rebuffer_duration_ms",
            "buffering/ratio",
            "fatal_errors/affected_views",
            "fatal_errors/ratio",
            "startup_outcomes/sample_views",
            "startup_outcomes/started/views",
            "startup_outcomes/started/ratio",
            "startup_outcomes/failed_before_start/views",
            "startup_outcomes/failed_before_start/ratio",
            "startup_outcomes/ended_before_start/views",
            "startup_outcomes/ended_before_start/ratio",
            "startup_outcomes/unknown/views",
            "startup_outcomes/unknown/ratio",
            "playback_score/value",
            "playback_score/sample_views",
        ],
        "engagement" => &[
            "views",
            "total_playing_time_ms",
            "average_playing_time_ms",
            "reached_end_views",
            "reached_end_ratio",
            "progress_reached/sample_views",
            "progress_reached/at_25_percent/views",
            "progress_reached/at_25_percent/ratio",
            "progress_reached/at_50_percent/views",
            "progress_reached/at_50_percent/ratio",
            "progress_reached/at_75_percent/views",
            "progress_reached/at_75_percent/ratio",
            "progress_reached/at_100_percent/views",
            "progress_reached/at_100_percent/ratio",
            "watch_time_ms/sample_views",
            "watch_time_ms/median",
            "watch_time_ms/p95",
            "watch_time_ms/distribution/under_10_seconds",
            "watch_time_ms/distribution/from_10_to_30_seconds",
            "watch_time_ms/distribution/from_30_to_60_seconds",
            "watch_time_ms/distribution/from_1_to_5_minutes",
            "watch_time_ms/distribution/at_least_5_minutes",
        ],
        _ => &["views", "unique_viewers", "total_playing_time_ms"],
    };
    let names: Vec<String> = if args.contains_key("group_by") {
        std::iter::once("value".to_owned())
            .chain(fields.iter().map(|s| format!("data/{s}")))
            .collect()
    } else if resource == "timeseries" {
        std::iter::once("bucket_start".to_owned())
            .chain(fields.iter().map(|s| (*s).to_owned()))
            .collect()
    } else {
        fields.iter().map(|s| (*s).to_owned()).collect()
    };
    names
        .into_iter()
        .map(|name| {
            let pointer = format!("/{name}");
            Column::new(name, pointer)
        })
        .collect()
}

fn write_csv_row(out: &mut Vec<u8>, values: impl Iterator<Item = Value>) {
    for (index, value) in values.enumerate() {
        if index > 0 {
            out.push(b',');
        }
        let (text, string) = match value {
            Value::Null | Value::Array(_) | Value::Object(_) => (String::new(), false),
            Value::String(s) => (s, true),
            value => (value.to_string(), false),
        };
        let quoted = text
            .bytes()
            .any(|byte| matches!(byte, b',' | b'"' | b'\r' | b'\n'));
        if quoted {
            out.push(b'"');
        }
        // CSV quoting alone does not stop spreadsheet formulas in untrusted strings.
        if string
            && text
                .trim_start()
                .starts_with(['=', '+', '-', '@', '\t', '\r'])
        {
            out.push(b'\'');
        }
        for byte in text.bytes() {
            out.push(byte);
            if byte == b'"' {
                out.push(byte);
            }
        }
        if quoted {
            out.push(b'"');
        }
    }
    out.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn download_grants_expire_and_follow_issuer_revocation() {
        let directory =
            std::env::temp_dir().join(format!("chunkify-export-grants-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let config_file = directory.join("config.toml");
        let env_file = directory.join("empty.env");
        std::fs::write(&env_file, "").unwrap();
        std::fs::write(&config_file, "retention.seconds=0\n").unwrap();
        let settings =
            crate::settings::Settings::load(Some(&env_file), Some(&config_file)).unwrap();
        let config = crate::config::Config::load_with(&settings, Some(&directory), None).unwrap();
        let keys = crate::config::Keys {
            projects: vec![crate::projects::primary_project(
                "test-public",
                "test-secret",
            )],
        };
        let registry = Arc::new(Registry::new(&keys, &config).unwrap());
        let owner = registry
            .authorize("test-secret", crate::projects::Permission::Secret)
            .unwrap();
        let (store, worker) = Store::open(
            &directory.join("test.duckdb"),
            config.batch.clone(),
            false,
            64,
            false,
            Some(1),
        )
        .unwrap();
        let service = Exports::new(store, registry.clone(), &config.rate_limits);
        let base = reqwest::Url::parse("https://analytics.example.com/base/").unwrap();
        let create = || {
            service
                .create(
                    &owner,
                    ExportRequest {
                        resource: "events".into(),
                        query: Map::new(),
                    },
                    &base,
                )
                .unwrap()
        };
        let a = create();
        let b = create();
        assert_ne!(a["download_url"], b["download_url"]);
        let token = |v: &Value| {
            v["download_url"]
                .as_str()
                .unwrap()
                .rsplit('/')
                .next()
                .unwrap()
                .to_owned()
        };
        assert!(service.filename(&token(&a)).is_ok());
        service
            .grants
            .lock()
            .unwrap()
            .get_mut(&token_hash(&token(&a)))
            .unwrap()
            .expires = Instant::now() - Duration::from_secs(1);
        assert_eq!(
            service.filename(&token(&a)).unwrap_err().status,
            axum::http::StatusCode::NOT_FOUND
        );
        registry.replace_keys(
            &crate::config::Keys {
                projects: vec![crate::projects::primary_project(
                    "test-public",
                    "rotated-secret",
                )],
            },
            &config,
        );
        assert_eq!(
            service.filename(&token(&b)).unwrap_err().status,
            axum::http::StatusCode::NOT_FOUND
        );
        drop(service);
        worker.join().unwrap().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn csv_escapes_quotes_newlines_and_unicode_and_disarms_formula_strings() {
        let mut bytes = Vec::new();
        write_csv_row(
            &mut bytes,
            vec![
                json!("=SUM(1,2)"),
                json!("a,\"b\"\n雪"),
                Value::Null,
                json!(42),
                json!({"nested":"excluded"}),
                json!([1, 2]),
            ]
            .into_iter(),
        );
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "\"'=SUM(1,2)\",\"a,\"\"b\"\"\n雪\",,42,,\r\n"
        );
    }
}
