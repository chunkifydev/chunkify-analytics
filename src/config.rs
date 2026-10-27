use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Config {
    pub admin_key_hash: Option<String>,
    pub backend: crate::backend::Backend,
    pub data_dir: PathBuf,
    pub mcp_enabled: bool,
    pub mcp_allowed_hosts: Vec<String>,
    pub bind: SocketAddr,
    pub allowed_origins: Vec<String>,
    pub max_body_bytes: usize,
    pub ingest_per_second: u32,
    pub retention_seconds: u64,
    pub retention_interval_seconds: u64,
    pub store_ip: bool,
    pub profile: bool,
    pub checkpoint_threshold_mib: u32,
    pub duckdb_threads: Option<usize>,
    pub batch: crate::writer::BatchConfig,
    pub separate_reads: bool,
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Keys {
    pub public_key: String,
    pub secret_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<crate::projects::Project>>,
}

impl Config {
    pub fn lock(&self) -> Result<fs::File> {
        self.try_lock()?.ok_or_else(|| {
            "Data directory is in use. Stop the collector and any administration command first."
                .into()
        })
    }

    pub fn try_lock(&self) -> Result<Option<fs::File>> {
        if matches!(self.backend, crate::backend::Backend::S3(_)) {
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
        if matches!(backend, crate::backend::Backend::S3(_))
            && settings.var("experimental.read_worker").as_deref() == Ok("true")
        {
            return Err("S3 storage uses a single worker; disable read_worker".into());
        }
        Ok(Self {
            admin_key_hash: settings
                .var("admin_key")
                .ok()
                .filter(|key| !key.trim().is_empty())
                .map(|key| crate::projects::hash(&key)),
            backend,
            mcp_enabled: settings
                .var("mcp.enabled")
                .unwrap_or_else(|_| "true".into())
                .parse()
                .map_err(|_| "mcp.enabled must be true or false")?,
            mcp_allowed_hosts,
            checkpoint_threshold_mib,
            duckdb_threads,
            batch: crate::writer::BatchConfig::load(settings)?,
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
            trusted_proxies: crate::context::trusted_proxies(
                &settings.var("server.trusted_proxies").unwrap_or_default(),
            )?,
            store_ip: settings
                .var("server.store_ip")
                .unwrap_or_else(|_| "true".into())
                .parse()
                .map_err(|_| "server.store_ip must be true or false")?,
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

    pub fn initialize(&self) -> Result<()> {
        self.create_data_directory()?;
        let _lock = self.lock()?;
        let keys = Keys {
            projects: None,
            public_key: crate::projects::generate_key(crate::projects::Permission::Public),
            secret_key: crate::projects::generate_key(crate::projects::Permission::Secret),
        };
        if let crate::backend::Backend::S3(remote) = &self.backend {
            if remote.read_keys()?.is_some() {
                return Err("S3 installation is already initialized; run serve to use it".into());
            }
            if let Some(warning) = self.save_keys(&keys)? {
                eprintln!("{warning}");
            }
            println!(
                "Initialized S3 storage. Keys are cached in {}.",
                self.key_cache_path().display()
            );
            return Ok(());
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
        fs::File::open(&self.data_dir)?.sync_all()?;
        println!(
            "Initialized {}. Keys are in keys.json.",
            self.data_dir.display()
        );
        Ok(())
    }

    pub fn key_cache_path(&self) -> PathBuf {
        self.data_dir
            .join(if matches!(self.backend, crate::backend::Backend::S3(_)) {
                "s3-keys.json"
            } else {
                "keys.json"
            })
    }

    pub fn keys(&self) -> Result<Keys> {
        if let crate::backend::Backend::S3(remote) = &self.backend {
            let bytes = remote
                .read_keys()?
                .ok_or("S3 installation is not initialized; run init first")?;
            let keys = decode_keys(&bytes)?;
            let _ = self.save_local_keys(&keys);
            return Ok(keys);
        }
        let bytes = fs::read(self.data_dir.join("keys.json")).map_err(
            |_| "Cannot read keys.json. Run the init command with this data directory first.",
        )?;
        decode_keys(&bytes)
    }

    pub(crate) fn save_keys(&self, keys: &Keys) -> Result<Option<String>> {
        if let crate::backend::Backend::S3(remote) = &self.backend {
            remote.write_keys(&serde_json::to_vec_pretty(keys)?)?;
            return Ok(self.save_local_keys(keys).unwrap_or_else(|_| {
                Some(
                    "Keys were saved to S3, but the local credential cache could not be updated."
                        .into(),
                )
            }));
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
            fs::rename(&temporary, self.key_cache_path())?;
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

pub(crate) fn decode_keys(bytes: &[u8]) -> Result<Keys> {
    let keys: Keys = serde_json::from_slice(bytes)?;
    if keys.public_key.len() < 32
        || keys.secret_key.len() < 32
        || keys.public_key == keys.secret_key
    {
        return Err(
            "Invalid keys.json: two distinct keys of at least 32 characters are required.".into(),
        );
    }
    if let Some(projects) = &keys.projects {
        crate::projects::validate(projects)?;
    }
    Ok(keys)
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
