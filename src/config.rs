use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use tracing::level_filters::LevelFilter;
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Config {
    pub admin_key_hash: Option<String>,
    pub backend: crate::backend::Backend,
    pub data_dir: PathBuf,
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
    pub profile: bool,
    pub log_level: LevelFilter,
    pub checkpoint_threshold_mib: u32,
    pub duckdb_threads: Option<usize>,
    pub batch: crate::writer::BatchConfig,
    pub separate_reads: bool,
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Keys {
    pub projects: Vec<crate::projects::Project>,
}

impl Config {
    pub fn is_clickhouse(&self) -> bool {
        matches!(self.backend, crate::backend::Backend::ClickHouse(_))
    }

    pub fn lock(&self) -> Result<fs::File> {
        self.try_lock()?.ok_or_else(|| {
            "Data directory is in use. Stop the collector and any administration command first."
                .into()
        })
    }

    pub fn try_lock(&self) -> Result<Option<fs::File>> {
        if matches!(
            self.backend,
            crate::backend::Backend::S3(_) | crate::backend::Backend::ClickHouse(_)
        ) {
            self.create_data_directory()?;
        }
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(self.data_dir.join("collector.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(Some(file)),
            Err(fs::TryLockError::WouldBlock) => Ok(None),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub fn load_with(
        settings: &crate::settings::Settings,
        data_dir: Option<&Path>,
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
        let checkpoint_threshold_mib = settings
            .var("duckdb.checkpoint_threshold_mib")
            .unwrap_or_else(|_| "16".into())
            .parse::<u32>()
            .map_err(|_| "duckdb.checkpoint_threshold_mib must be between 1 and 1024")?;
        if !(1..=1024).contains(&checkpoint_threshold_mib) {
            return Err("duckdb.checkpoint_threshold_mib must be between 1 and 1024".into());
        }
        let duckdb_threads = settings
            .var("duckdb.threads")
            .ok()
            .map(|value| -> Result<usize> {
                let count = value
                    .parse::<usize>()
                    .map_err(|_| "duckdb.threads must be between 1 and 256")?;
                if !(1..=256).contains(&count) {
                    return Err("duckdb.threads must be between 1 and 256".into());
                }
                Ok(count)
            })
            .transpose()?;
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
        let backend = crate::backend::Backend::load(settings)?;
        let batch = crate::writer::BatchConfig::load(
            settings,
            // ClickHouse batches adaptively: requests queued during one insert
            // form the next batch, so it uses the larger buffered limits.
            matches!(&backend, crate::backend::Backend::S3(s3) if !s3.flush_interval.is_zero())
                || matches!(&backend, crate::backend::Backend::ClickHouse(_)),
        )?;
        let report_cache_seconds = settings
            .var("cache.report_ttl_seconds")
            .unwrap_or_else(|_| {
                if matches!(backend, crate::backend::Backend::S3(_)) {
                    "60"
                } else {
                    "0"
                }
                .into()
            })
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
            backend,
            report_cache_seconds,
            mcp_enabled: settings
                .var("mcp.enabled")
                .unwrap_or_else(|_| "true".into())
                .parse()
                .map_err(|_| "mcp.enabled must be true or false")?,
            mcp_allowed_hosts,
            checkpoint_threshold_mib,
            duckdb_threads,
            batch,
            separate_reads: settings
                .var("experimental.read_worker")
                .unwrap_or_else(|_| "false".into())
                .parse()
                .map_err(|_| "experimental.read_worker must be true or false")?,
            profile: settings
                .var("profile")
                .unwrap_or_else(|_| "false".into())
                .parse()
                .map_err(|_| "profile must be true or false")?,
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
            data_dir: data_dir.map(Path::to_path_buf).unwrap_or_else(|| {
                settings
                    .var_os("data_dir")
                    .map(PathBuf::from)
                    .unwrap_or_else(|| "data".into())
            }),
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

    pub fn database_path(&self) -> PathBuf {
        match &self.backend {
            crate::backend::Backend::Local { path: Some(path) } => path.clone(),
            _ => self.data_dir.join("analytics.duckdb"),
        }
    }

    fn create_data_directory(&self) -> Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.data_dir)?;
        Ok(())
    }

    pub fn initialize(&self) -> Result<serde_json::Value> {
        self.create_data_directory()?;
        let _lock = self.lock()?;
        let public_key = crate::projects::generate_key(crate::projects::Permission::Public);
        let secret_key = crate::projects::generate_key(crate::projects::Permission::Secret);
        let keys = Keys {
            projects: vec![crate::projects::primary_project(&public_key, &secret_key)],
        };
        let output = serde_json::json!({
            "project_id": crate::projects::DEFAULT_PROJECT,
            "public_key": public_key,
            "secret_key": secret_key,
        });
        if let crate::backend::Backend::ClickHouse(clickhouse) = &self.backend {
            crate::clickhouse::prepare(clickhouse)?;
            if crate::clickhouse::load_registry(clickhouse)?.is_some() {
                return Err(
                    "ClickHouse installation is already initialized; run serve to use it".into(),
                );
            }
            if let Some(warning) = self.save_keys(&keys)? {
                eprintln!("{warning}");
            }
            return Ok(output);
        }
        if let crate::backend::Backend::S3(remote) = &self.backend {
            if remote.read_keys()?.is_some() {
                return Err("S3 installation is already initialized; run serve to use it".into());
            }
            if let Some(warning) = self.save_keys(&keys)? {
                eprintln!("{warning}");
            }
            return Ok(output);
        }
        let mut options = fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(self.data_dir.join("keys.json"))?;
        file.write_all(&serde_json::to_vec_pretty(&keys)?)?;
        file.sync_all()?;
        #[cfg(unix)]
        if fs::File::open(&self.data_dir)
            .and_then(|directory| directory.sync_all())
            .is_err()
        {
            eprintln!(
                "Registry was created, but syncing the directory failed; durability across a system crash is uncertain."
            );
        }
        Ok(output)
    }

    pub fn keys(&self) -> Result<Keys> {
        let bytes = match &self.backend {
            crate::backend::Backend::ClickHouse(clickhouse) => {
                let (version, bytes) = crate::clickhouse::load_registry(clickhouse)?
                    .ok_or("ClickHouse installation is not initialized; run init first")?;
                let keys: Keys = serde_json::from_slice(&bytes)?;
                crate::projects::validate(&keys.projects)?;
                clickhouse.set_loaded_version(version);
                return Ok(keys);
            }
            crate::backend::Backend::S3(remote) => remote
                .read_keys()?
                .ok_or("S3 installation is not initialized; run init first")?,
            _ => fs::read(self.data_dir.join("keys.json")).map_err(
                |_| "Cannot read keys.json. Run the init command with this data directory first.",
            )?,
        };
        let keys: Keys = serde_json::from_slice(&bytes)?;
        crate::projects::validate(&keys.projects)?;
        Ok(keys)
    }

    pub(crate) fn save_keys(&self, keys: &Keys) -> Result<Option<String>> {
        if let crate::backend::Backend::ClickHouse(clickhouse) = &self.backend {
            // Compare-and-set against the version this process last loaded,
            // so concurrent administration on another server is never lost.
            let version = crate::clickhouse::save_registry(
                clickhouse,
                clickhouse.loaded_version(),
                &serde_json::to_vec_pretty(keys)?,
            )?;
            clickhouse.set_loaded_version(version);
            return Ok(None);
        }
        if let crate::backend::Backend::S3(remote) = &self.backend {
            remote.write_keys(&serde_json::to_vec_pretty(keys)?)?;
            return Ok(None);
        }
        self.save_local_keys(keys)
    }

    fn save_local_keys(&self, keys: &Keys) -> Result<Option<String>> {
        // Write and sync a private file on the same filesystem before replacing
        // keys.json. Readers see the complete old or complete new key pair.
        let temporary = self.data_dir.join(format!(".keys-{}.tmp", Uuid::new_v4()));
        let result = (|| -> Result<()> {
            let mut options = fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(&serde_json::to_vec_pretty(&keys)?)?;
            file.sync_all()?;
            drop(file);
            fs::rename(&temporary, self.data_dir.join("keys.json"))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        // Rename has committed the change. Even if the directory sync fails,
        // callers must activate these keys and return any newly generated secret.
        #[cfg(unix)]
        if fs::File::open(&self.data_dir)
            .and_then(|directory| directory.sync_all())
            .is_err()
        {
            return Ok(Some("Keys were updated, but syncing the directory failed; durability across a system crash is uncertain.".into()));
        }
        Ok(None)
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
