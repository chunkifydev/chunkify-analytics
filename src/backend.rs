//! ClickHouse connection settings. Credentials are never formatted in diagnostics.
use crate::settings::Settings;
use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

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
    /// Time between hourly rollup refreshes. Zero disables rollups for reads too.
    pub rollup_interval: Duration,
    /// How old the last refresh may be for reports to read every hour from it.
    /// Zero keeps reports exact.
    pub rollup_max_lag: Duration,
    /// Registry version loaded or written by this process; used for compare-and-set.
    pub registry_version: Arc<AtomicU64>,
}

impl ClickHouseConfig {
    pub fn load(settings: &Settings) -> Result<Self> {
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
        let rollup_interval = settings
            .var("clickhouse.rollup_interval_seconds")
            .unwrap_or_else(|_| "15".into())
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= 3600)
            .ok_or("clickhouse.rollup_interval_seconds must be between 0 and 3600")?;
        let rollup_max_lag = settings
            .var("clickhouse.rollup_max_lag_seconds")
            .unwrap_or_else(|_| "0".into())
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= 3600)
            .ok_or("clickhouse.rollup_max_lag_seconds must be between 0 and 3600")?;
        Ok(Self {
            rollup_max_lag: Duration::from_secs(rollup_max_lag),
            rollup_interval: Duration::from_secs(rollup_interval),
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
