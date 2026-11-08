//! Storage selection and connection settings. Credentials are never formatted in diagnostics.
use crate::settings::Settings;
use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

#[derive(Clone)]
pub enum Backend {
    Local { path: Option<PathBuf> },
    S3(S3Config),
    ClickHouse(ClickHouseConfig),
}

/// A shared ClickHouse server or cluster. Every analytics server using the same
/// database sees the same events and project registry, so servers can scale out.
#[derive(Clone)]
pub struct ClickHouseConfig {
    pub url: reqwest::Url,
    pub database: String,
    pub user: Option<String>,
    pub password: Option<String>,
    pub async_insert: bool,
    pub registry_refresh: Duration,
    pub node_id: Option<u16>,
    pub deduplication_window: Duration,
    /// Registry version loaded or written by this process; used for compare-and-set.
    pub registry_version: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct S3Config {
    pub flush_interval: Duration,
    pub bucket: String,
    pub prefix: String,
    pub endpoint: Option<String>,
    pub region: String,
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    pub session_token: Option<String>,
    pub url_style: String,
    pub use_ssl: bool,
}

impl Default for Backend {
    fn default() -> Self {
        Self::Local { path: None }
    }
}

impl Backend {
    pub fn load(settings: &Settings) -> Result<Self> {
        match settings
            .var("storage.backend")
            .as_deref()
            .unwrap_or("local")
        {
            "local" => Ok(Self::Local {
                path: settings.var_os("storage.path").map(PathBuf::from),
            }),
            "s3" => Ok(Self::S3(S3Config::load(settings)?)),
            "clickhouse" => Ok(Self::ClickHouse(ClickHouseConfig::load(settings)?)),
            _ => Err("storage.backend must be local, s3 or clickhouse".into()),
        }
    }
}

impl ClickHouseConfig {
    fn load(settings: &Settings) -> Result<Self> {
        let optional = |name| settings.var(name).ok().filter(|value| !value.is_empty());
        let url = optional("clickhouse.url").unwrap_or_else(|| "http://127.0.0.1:8123".into());
        let mut url = reqwest::Url::parse(&url).map_err(|_| "Invalid clickhouse.url")?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("clickhouse.url must be an HTTP(S) URL without credentials, query or fragment; use clickhouse.user and clickhouse.password".into());
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let database =
            optional("clickhouse.database").unwrap_or_else(|| "chunkify_analytics".into());
        if database.is_empty()
            || database.len() > 64
            || !database
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_')
            || database.as_bytes()[0].is_ascii_digit()
        {
            return Err("clickhouse.database must have 1..64 ASCII letters, digits or underscores and cannot start with a digit".into());
        }
        let async_insert = settings
            .var("clickhouse.async_insert")
            .unwrap_or_else(|_| "true".into())
            .parse()
            .map_err(|_| "clickhouse.async_insert must be true or false")?;
        let refresh = settings
            .var("clickhouse.registry_refresh_seconds")
            .unwrap_or_else(|_| "5".into())
            .parse::<u64>()
            .ok()
            .filter(|seconds| (1..=3600).contains(seconds))
            .ok_or("clickhouse.registry_refresh_seconds must be between 1 and 3600")?;
        let node_id = settings
            .var("clickhouse.node_id")
            .ok()
            .map(|value| {
                value
                    .parse::<u16>()
                    .ok()
                    .filter(|id| *id < 64)
                    .ok_or("clickhouse.node_id must be between 0 and 63")
            })
            .transpose()?;
        let deduplication_window = settings
            .var("clickhouse.deduplication_window_seconds")
            .unwrap_or_else(|_| "604800".into())
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= 31_536_000)
            .ok_or("clickhouse.deduplication_window_seconds must be between 0 and 31536000")?;
        Ok(Self {
            deduplication_window: Duration::from_secs(deduplication_window),
            url,
            database,
            user: optional("clickhouse.user"),
            password: optional("clickhouse.password"),
            async_insert,
            registry_refresh: Duration::from_secs(refresh),
            node_id,
            registry_version: Arc::new(AtomicU64::new(0)),
        })
    }
}

impl S3Config {
    fn load(settings: &Settings) -> Result<Self> {
        let optional = |name| settings.var(name).ok().filter(|value| !value.is_empty());
        let url = optional("storage.url").ok_or("S3 storage requires storage.url")?;
        let parsed = reqwest::Url::parse(&url).map_err(|_| "Invalid S3 storage URL")?;
        let bucket = parsed
            .host_str()
            .ok_or("S3 storage URL requires a bucket")?;
        let prefix = parsed.path().trim_matches('/');
        if parsed.scheme() != "s3"
            || !parsed.username().is_empty()
            || parsed.password().is_some()
            || parsed.port().is_some()
            || parsed.query().is_some()
            || parsed.fragment().is_some()
            || !bucket
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b".-".contains(&c))
            || prefix.split('/').any(|part| {
                part == ".."
                    || part == "."
                    || part.contains(['*', '?', '[', ']', '%', '\\'])
                    || part.chars().any(char::is_control)
            })
        {
            return Err("S3 storage URL must be s3://bucket/optional-prefix without credentials, escapes or wildcards".into());
        }
        let access_key_id = optional("storage.access_key_id");
        let secret_access_key = optional("storage.secret_access_key");
        if access_key_id.is_some() != secret_access_key.is_some() {
            return Err("S3 access_key_id and secret_access_key must be provided together".into());
        }
        let region = optional("storage.region").unwrap_or_else(|| "us-east-1".into());
        let endpoint = optional("storage.endpoint");
        if let Some(endpoint) = &endpoint
            && (endpoint.contains(['/', '@', '?', '#'])
                || endpoint.parse::<axum::http::uri::Authority>().is_err())
        {
            return Err(
                "S3 endpoint must be a hostname with an optional port; use use_ssl to select HTTPS"
                    .into(),
            );
        }
        let url_style = optional("storage.url_style").unwrap_or_else(|| "path".into());
        if !matches!(url_style.as_str(), "path" | "vhost") {
            return Err("S3 url_style must be path or vhost".into());
        }
        let use_ssl = settings
            .var("storage.use_ssl")
            .unwrap_or_else(|_| "true".into())
            .parse()
            .map_err(|_| "S3 use_ssl must be true or false")?;
        let flush_interval = settings
            .var("storage.flush_interval_seconds")
            .unwrap_or_else(|_| "0".into())
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= 3600)
            .ok_or("storage.flush_interval_seconds must be between 0 and 3600")?;
        Ok(Self {
            flush_interval: Duration::from_secs(flush_interval),
            bucket: bucket.into(),
            prefix: if prefix.is_empty() {
                String::new()
            } else {
                format!("{prefix}/")
            },
            endpoint,
            region,
            access_key_id,
            secret_access_key,
            session_token: optional("storage.session_token"),
            url_style,
            use_ssl,
        })
    }
}
