use super::{Result, Settings};
use serde::Deserialize;
use std::{
    fs,
    path::{Path, PathBuf},
};

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct FileConfig {
    data_dir: Option<PathBuf>,
    profile: Option<bool>,
    server: Server,
    retention: Retention,
    batch: Batch,
    duckdb: Duckdb,
    experimental: Experimental,
    mcp: Mcp,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Server {
    bind: Option<String>,
    allowed_origins: Option<Vec<String>>,
    trusted_proxies: Option<Vec<String>>,
    store_ip: Option<bool>,
    max_body_bytes: Option<usize>,
    max_otlp_records: Option<usize>,
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
    insert_mode: Option<String>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Duckdb {
    threads: Option<usize>,
    checkpoint_threshold_mib: Option<u32>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Experimental {
    durable_queue: Option<bool>,
    replay_pipeline: Option<bool>,
    read_worker: Option<bool>,
    spool_max_bytes: Option<usize>,
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Mcp {
    enabled: Option<bool>,
    allowed_hosts: Option<Vec<String>>,
    collector_url: Option<String>,
    read_token: Option<String>,
}

pub(super) fn load(config_file: Option<&Path>) -> Result<Settings> {
    let file = config_file.unwrap_or_else(|| Path::new("collector.toml"));
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
    let mut settings = Settings::default();
    if let Some(directory) = config.data_dir {
        let directory = if directory.is_absolute() {
            directory
        } else {
            file.parent()
                .unwrap_or_else(|| Path::new("."))
                .join(directory)
        };
        settings
            .values
            .insert("COLLECTOR_DATA_DIR".into(), directory.into_os_string());
    }
    if let Some(value) = config.profile {
        settings
            .values
            .insert("COLLECTOR_PROFILE".into(), value.to_string().into());
    }
    if let Some(value) = config.server.bind {
        settings
            .values
            .insert("COLLECTOR_BIND".into(), value.into());
    }
    if let Some(value) = config.server.allowed_origins {
        settings
            .values
            .insert("COLLECTOR_ALLOWED_ORIGINS".into(), value.join(",").into());
    }
    if let Some(value) = config.server.trusted_proxies {
        settings
            .values
            .insert("COLLECTOR_TRUSTED_PROXIES".into(), value.join(",").into());
    }
    if let Some(value) = config.server.store_ip {
        settings
            .values
            .insert("COLLECTOR_STORE_IP".into(), value.to_string().into());
    }
    if let Some(value) = config.server.max_body_bytes {
        settings
            .values
            .insert("COLLECTOR_MAX_BODY_BYTES".into(), value.to_string().into());
    }
    if let Some(value) = config.server.max_otlp_records {
        settings.values.insert(
            "COLLECTOR_MAX_OTLP_RECORDS".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.server.ingest_per_second {
        settings.values.insert(
            "COLLECTOR_INGEST_PER_SECOND".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.retention.seconds {
        settings.values.insert(
            "COLLECTOR_RETENTION_SECONDS".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.retention.interval_seconds {
        settings.values.insert(
            "COLLECTOR_RETENTION_INTERVAL_SECONDS".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.batch.max_events {
        settings.values.insert(
            "COLLECTOR_BATCH_MAX_EVENTS".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.batch.max_bytes {
        settings
            .values
            .insert("COLLECTOR_BATCH_MAX_BYTES".into(), value.to_string().into());
    }
    if let Some(value) = config.batch.delay_ms {
        settings
            .values
            .insert("COLLECTOR_BATCH_DELAY_MS".into(), value.to_string().into());
    }
    if let Some(value) = config.batch.queue_max_bytes {
        settings
            .values
            .insert("COLLECTOR_QUEUE_MAX_BYTES".into(), value.to_string().into());
    }
    if let Some(value) = config.batch.queue_max_requests {
        settings.values.insert(
            "COLLECTOR_QUEUE_MAX_REQUESTS".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.batch.insert_mode {
        settings
            .values
            .insert("COLLECTOR_INSERT_MODE".into(), value.into());
    }
    if let Some(value) = config.duckdb.threads {
        settings
            .values
            .insert("COLLECTOR_DUCKDB_THREADS".into(), value.to_string().into());
    }
    if let Some(value) = config.duckdb.checkpoint_threshold_mib {
        settings.values.insert(
            "COLLECTOR_CHECKPOINT_THRESHOLD_MIB".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.experimental.durable_queue {
        settings.values.insert(
            "COLLECTOR_EXPERIMENTAL_DURABLE_QUEUE".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.experimental.replay_pipeline {
        settings.values.insert(
            "COLLECTOR_EXPERIMENTAL_REPLAY_PIPELINE".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.experimental.read_worker {
        settings.values.insert(
            "COLLECTOR_EXPERIMENTAL_READ_WORKER".into(),
            value.to_string().into(),
        );
    }
    if let Some(value) = config.experimental.spool_max_bytes {
        settings
            .values
            .insert("COLLECTOR_SPOOL_MAX_BYTES".into(), value.to_string().into());
    }
    if let Some(value) = config.mcp.enabled {
        settings
            .values
            .insert("COLLECTOR_MCP_ENABLED".into(), value.to_string().into());
    }
    if let Some(value) = config.mcp.allowed_hosts {
        settings
            .values
            .insert("COLLECTOR_MCP_ALLOWED_HOSTS".into(), value.join(",").into());
    }
    if let Some(value) = config.mcp.collector_url {
        settings
            .values
            .insert("CHUNKIFY_MCP_COLLECTOR_URL".into(), value.into());
    }
    if let Some(value) = config.mcp.read_token {
        settings
            .values
            .insert("CHUNKIFY_MCP_READ_TOKEN".into(), value.into());
    }
    Ok(settings)
}
