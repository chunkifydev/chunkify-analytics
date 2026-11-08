use super::{Result, Settings};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    logging: Logging,
    server: Server,
    retention: Retention,
    batch: Batch,
    clickhouse: ClickHouse,
    mcp: Mcp,
    rate_limits: crate::limits::Config,
    cache: Cache,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Cache {
    report_ttl_seconds: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Logging {
    level: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Server {
    bind: Option<String>,
    public_url: Option<String>,
    allowed_origins: Option<Vec<String>>,
    trusted_proxies: Option<Vec<String>>,
    geoip_database: Option<PathBuf>,
    max_body_bytes: Option<usize>,
    ingest_per_second: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Retention {
    seconds: Option<u64>,
    interval_seconds: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Batch {
    max_events: Option<usize>,
    max_bytes: Option<usize>,
    delay_ms: Option<u64>,
    queue_max_bytes: Option<usize>,
    queue_max_requests: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ClickHouse {
    url: Option<String>,
    database: Option<String>,
    user: Option<String>,
    password: Option<String>,
    async_insert: Option<bool>,
    registry_refresh_seconds: Option<u64>,
    node_id: Option<u16>,
    deduplication_window_seconds: Option<u64>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Mcp {
    enabled: Option<bool>,
    allowed_hosts: Option<Vec<String>>,
    url: Option<String>,
    secret_key: Option<String>,
}

pub(super) fn load(config_file: Option<&Path>) -> Result<Settings> {
    let file = config_file.unwrap_or_else(|| Path::new("config.toml"));
    let source = match fs::read_to_string(file) {
        Ok(source) => source,
        Err(error) if config_file.is_none() && error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(_) => return Err(format!("Cannot read configuration file: {}", file.display()).into()),
    };
    let config: FileConfig = toml::from_str(&source).map_err(|error: toml::de::Error| {
        let line = error
            .span()
            .map(|span| {
                source[..span.start]
                    .bytes()
                    .filter(|byte| *byte == b'\n')
                    .count()
                    + 1
            })
            .unwrap_or(1);
        format!(
            "Invalid TOML configuration: {} at line {line}. Check field names and value types.",
            file.display()
        )
    })?;
    let mut settings = Settings {
        rate_limits: config.rate_limits,
        ..Settings::default()
    };
    if let Some(seconds) = config.cache.report_ttl_seconds {
        settings.values.insert(
            "cache.report_ttl_seconds".into(),
            seconds.to_string().into(),
        );
    }
    let clickhouse = config.clickhouse;
    for (name, value) in [
        ("clickhouse.url", clickhouse.url),
        ("clickhouse.database", clickhouse.database),
        ("clickhouse.user", clickhouse.user),
        ("clickhouse.password", clickhouse.password),
        (
            "clickhouse.async_insert",
            clickhouse.async_insert.map(|value| value.to_string()),
        ),
        (
            "clickhouse.registry_refresh_seconds",
            clickhouse
                .registry_refresh_seconds
                .map(|value| value.to_string()),
        ),
        (
            "clickhouse.node_id",
            clickhouse.node_id.map(|value| value.to_string()),
        ),
        (
            "clickhouse.deduplication_window_seconds",
            clickhouse
                .deduplication_window_seconds
                .map(|value| value.to_string()),
        ),
    ] {
        if let Some(value) = value {
            settings.values.insert(name.into(), value.into());
        }
    }
    if let Some(value) = config.logging.level {
        settings.values.insert("logging.level".into(), value.into());
    }
    if let Some(value) = config.server.bind {
        settings.values.insert("server.bind".into(), value.into());
    }
    if let Some(value) = config.server.public_url {
        settings
            .values
            .insert("server.public_url".into(), value.into());
    }
    if let Some(value) = config.server.allowed_origins {
        settings
            .values
            .insert("server.allowed_origins".into(), value.join(",").into());
    }
    if let Some(value) = config.server.trusted_proxies {
        settings
            .values
            .insert("server.trusted_proxies".into(), value.join(",").into());
    }
    if let Some(path) = config.server.geoip_database {
        let path = if path.is_absolute() {
            path
        } else {
            file.parent().unwrap_or_else(|| Path::new(".")).join(path)
        };
        settings
            .values
            .insert("server.geoip_database".into(), path.into_os_string());
    }
    if let Some(value) = config.server.max_body_bytes {
        settings
            .values
            .insert("server.max_body_bytes".into(), value.to_string().into());
    }
    if let Some(value) = config.server.ingest_per_second {
        settings
            .values
            .insert("server.ingest_per_second".into(), value.to_string().into());
    }
    if let Some(value) = config.retention.seconds {
        settings
            .values
            .insert("retention.seconds".into(), value.to_string().into());
    }
    if let Some(value) = config.retention.interval_seconds {
        settings.values.insert(
            "retention.interval_seconds".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.batch.max_events {
        settings
            .values
            .insert("batch.max_events".into(), value.to_string().into());
    }
    if let Some(value) = config.batch.max_bytes {
        settings
            .values
            .insert("batch.max_bytes".into(), value.to_string().into());
    }
    if let Some(value) = config.batch.delay_ms {
        settings
            .values
            .insert("batch.delay_ms".into(), value.to_string().into());
    }
    if let Some(value) = config.batch.queue_max_bytes {
        settings
            .values
            .insert("batch.queue_max_bytes".into(), value.to_string().into());
    }
    if let Some(value) = config.batch.queue_max_requests {
        settings
            .values
            .insert("batch.queue_max_requests".into(), value.to_string().into());
    }
    if let Some(value) = config.mcp.enabled {
        settings
            .values
            .insert("mcp.enabled".into(), value.to_string().into());
    }
    if let Some(value) = config.mcp.allowed_hosts {
        settings
            .values
            .insert("mcp.allowed_hosts".into(), value.join(",").into());
    }
    if let Some(value) = config.mcp.url {
        settings.values.insert("mcp.url".into(), value.into());
    }
    if let Some(value) = config.mcp.secret_key {
        settings
            .values
            .insert("mcp.secret_key".into(), value.into());
    }
    Ok(settings)
}
