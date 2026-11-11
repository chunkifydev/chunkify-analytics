use serde::{Deserialize, Serialize};
use std::{net::SocketAddr, path::PathBuf};
use tracing::level_filters::LevelFilter;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Config {
    pub admin_key_hash: Option<String>,
    pub clickhouse: crate::backend::ClickHouseConfig,
    pub mcp_enabled: bool,
    pub mcp_allowed_hosts: Vec<String>,
    pub bind: SocketAddr,
    pub public_url: Option<reqwest::Url>,
    pub allowed_origins: Vec<String>,
    pub max_body_bytes: usize,
    pub ingest_per_second: u32,
    pub rate_limits: crate::limits::Config,
    pub report_cache_seconds: u64,
    pub retention_seconds: u64,
    pub retention_interval_seconds: u64,
    pub geoip_database: PathBuf,
    pub log_level: LevelFilter,
    pub batch: crate::batch::BatchConfig,
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    pub projects: Vec<crate::projects::Project>,
}

impl Config {
    pub fn load_with(
        settings: &crate::settings::Settings,
        bind: Option<SocketAddr>,
    ) -> Result<Self> {
        settings.rate_limits.validate()?;
        let allowed_origins: Vec<String> = settings
            .var("server.allowed_origins")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(String::from)
            .collect();
        for origin in &allowed_origins {
            validate_origin(origin)?;
        }
        let max_body_bytes = settings
            .var("server.max_body_bytes")
            .unwrap_or_else(|_| "65536".into())
            .parse()?;
        if max_body_bytes == 0 {
            return Err("server.max_body_bytes must be positive".into());
        }
        let ingest_per_second = settings
            .var("server.ingest_per_second")
            .unwrap_or_else(|_| "100".into())
            .parse()?;
        if ingest_per_second == 0 {
            return Err("server.ingest_per_second must be positive".into());
        }
        let retention_seconds = settings
            .var("retention.seconds")
            .unwrap_or_else(|_| "2592000".into())
            .parse::<u64>()
            .map_err(|_| "retention.seconds must be a non-negative whole number")?;
        if retention_seconds > crate::retention::MAX_SECONDS {
            return Err("retention.seconds is too large".into());
        }
        let retention_interval_seconds = settings
            .var("retention.interval_seconds")
            .unwrap_or_else(|_| "3600".into())
            .parse::<u64>()
            .map_err(|_| "retention.interval_seconds must be a positive whole number")?;
        if !(1..=86400).contains(&retention_interval_seconds) {
            return Err("retention.interval_seconds must be between 1 and 86400".into());
        }
        let mcp_allowed_hosts: Vec<String> = settings
            .var("mcp.allowed_hosts")
            .unwrap_or_else(|_| "localhost,127.0.0.1,[::1]".into())
            .split(',')
            .map(str::trim)
            .filter(|host| !host.is_empty())
            .map(str::to_owned)
            .collect();
        if mcp_allowed_hosts.is_empty()
            || mcp_allowed_hosts.iter().any(|host| {
                host.contains('*')
                    || host.contains('@')
                    || host.parse::<axum::http::uri::Authority>().is_err()
            })
        {
            return Err(
                "mcp.allowed_hosts must contain literal hostnames or host:port authorities".into(),
            );
        }
        let clickhouse = crate::backend::ClickHouseConfig::load(settings)?;
        let batch = crate::batch::BatchConfig::load(settings)?;
        let report_cache_seconds = settings
            .var("cache.report_ttl_seconds")
            .unwrap_or_else(|_| "0".into())
            .parse::<u64>()
            .ok()
            .filter(|seconds| *seconds <= 60)
            .ok_or("cache.report_ttl_seconds must be between 0 and 60")?;
        let public_url = settings
            .var("server.public_url")
            .ok()
            .map(|value| {
                let mut url =
                    reqwest::Url::parse(&value).map_err(|_| "Invalid server.public_url")?;
                if !matches!(url.scheme(), "http" | "https")
                    || url.host_str().is_none()
                    || !url.username().is_empty()
                    || url.password().is_some()
                    || url.query().is_some()
                    || url.fragment().is_some()
                {
                    return Err(
                        "server.public_url must be HTTP(S), without credentials, query or fragment",
                    );
                }
                if !url.path().ends_with('/') {
                    url.set_path(&format!("{}/", url.path()));
                }
                Ok(url)
            })
            .transpose()?;
        Ok(Self {
            rate_limits: settings.rate_limits.clone(),
            public_url,
            admin_key_hash: settings
                .var("admin_key")
                .ok()
                .filter(|key| !key.trim().is_empty())
                .map(|key| crate::projects::hash(&key)),
            clickhouse,
            report_cache_seconds,
            mcp_enabled: settings
                .var("mcp.enabled")
                .unwrap_or_else(|_| "true".into())
                .parse()
                .map_err(|_| "mcp.enabled must be true or false")?,
            mcp_allowed_hosts,
            batch,
            log_level: match settings.var("logging.level").as_deref().unwrap_or("info") {
                "off" => LevelFilter::OFF,
                "error" => LevelFilter::ERROR,
                "warn" => LevelFilter::WARN,
                "info" => LevelFilter::INFO,
                "debug" => LevelFilter::DEBUG,
                "trace" => LevelFilter::TRACE,
                _ => {
                    return Err(
                        "logging.level must be off, error, warn, info, debug or trace".into(),
                    );
                }
            },
            trusted_proxies: crate::context::trusted_proxies(
                &settings.var("server.trusted_proxies").unwrap_or_default(),
            )?,
            geoip_database: settings
                .var_os("server.geoip_database")
                .map(PathBuf::from)
                .unwrap_or(crate::geolocation::default_path()?),
            bind: match bind {
                Some(bind) => bind,
                None => settings
                    .var("server.bind")
                    .unwrap_or_else(|_| "127.0.0.1:4174".into())
                    .parse()?,
            },
            allowed_origins,
            max_body_bytes,
            ingest_per_second,
            retention_seconds,
            retention_interval_seconds,
        })
    }

    /// Create the ClickHouse database, apply the schema migrations and save
    /// the initial project registry once.
    pub fn initialize(&self) -> Result<serde_json::Value> {
        crate::clickhouse::migrate(&self.clickhouse, true)?;
        if crate::clickhouse::read_registry(&self.clickhouse)?.is_some() {
            return Err(
                "This ClickHouse database is already initialized; run serve to use it".into(),
            );
        }
        let public_key = crate::projects::generate_key(crate::projects::Permission::Public);
        let secret_key = crate::projects::generate_key(crate::projects::Permission::Secret);
        let keys = Keys {
            projects: vec![crate::projects::primary_project(&public_key, &secret_key)],
        };
        self.save_keys(&keys)?;
        Ok(serde_json::json!({
            "project_id": crate::projects::DEFAULT_PROJECT,
            "public_key": public_key,
            "secret_key": secret_key,
        }))
    }

    /// The current project registry, which stores only credential hashes.
    pub fn keys(&self) -> Result<Keys> {
        // Reads never create a database, for example under a mistyped name.
        let (version, bytes) = crate::clickhouse::read_registry(&self.clickhouse)?
            .ok_or("This ClickHouse database is not initialized; run init first")?;
        let keys: Keys = serde_json::from_slice(&bytes)?;
        crate::projects::validate(&keys.projects)?;
        self.clickhouse.set_loaded_version(version);
        Ok(keys)
    }

    /// Compare-and-set against the version this process last loaded, so
    /// concurrent administration through another server is never lost.
    pub(crate) fn save_keys(&self, keys: &Keys) -> Result<()> {
        let version = crate::clickhouse::save_registry(
            &self.clickhouse,
            self.clickhouse.loaded_version(),
            &serde_json::to_vec_pretty(keys)?,
        )?;
        self.clickhouse.set_loaded_version(version);
        Ok(())
    }
}

pub(crate) fn validate_origin(origin: &str) -> std::result::Result<(), String> {
    let uri: axum::http::Uri = origin.parse().map_err(|_| "Invalid origin")?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.authority().is_none()
        || uri.path() != "/"
        || uri.query().is_some()
        || origin.ends_with('/')
        || origin.contains('@')
    {
        return Err(
            "Allowed origins must be exact HTTP(S) origins without a trailing slash or path."
                .into(),
        );
    }
    Ok(())
}
