use serde::{Deserialize, Serialize};
use std::{env, fs, io::Write, net::SocketAddr, path::PathBuf};
use uuid::Uuid;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub struct Config {
    pub data_dir: PathBuf,
    pub bind: SocketAddr,
    pub allowed_origins: Vec<String>,
    pub max_body_bytes: usize,
    pub max_otlp_records: usize,
    pub ingest_per_second: u32,
    pub retention_seconds: u64,
    pub retention_interval_seconds: u64,
    pub store_ip: bool,
    pub profile: bool,
    pub checkpoint_threshold_mib: u32,
    pub duckdb_threads: Option<usize>,
    pub batch: crate::writer::BatchConfig,
    pub spool: Option<crate::spool::SpoolConfig>,
    pub separate_reads: bool,
    pub trusted_proxies: Vec<ipnet::IpNet>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Keys {
    pub ingest_key: String,
    pub read_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub projects: Option<Vec<crate::projects::Project>>,
}

impl Config {
    pub fn lock(&self) -> Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(self.data_dir.join("collector.lock"))?;
        match file.try_lock() {
            Ok(()) => Ok(file),
            Err(fs::TryLockError::WouldBlock) => Err("Data directory is in use. Stop the collector and any administration command first.".into()),
            Err(fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub fn load() -> Result<Self> {
        let allowed_origins: Vec<String> = env::var("COLLECTOR_ALLOWED_ORIGINS")
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|origin| !origin.is_empty())
            .map(String::from)
            .collect();
        for origin in &allowed_origins {
            validate_origin(origin)?;
        }
        let max_body_bytes = env::var("COLLECTOR_MAX_BODY_BYTES")
            .unwrap_or_else(|_| "65536".into())
            .parse()?;
        if max_body_bytes == 0 {
            return Err("COLLECTOR_MAX_BODY_BYTES must be positive".into());
        }
        let max_otlp_records = env::var("COLLECTOR_MAX_OTLP_RECORDS")
            .unwrap_or_else(|_| "100".into())
            .parse()?;
        if max_otlp_records == 0 {
            return Err("COLLECTOR_MAX_OTLP_RECORDS must be positive".into());
        }
        let ingest_per_second = env::var("COLLECTOR_INGEST_PER_SECOND")
            .unwrap_or_else(|_| "100".into())
            .parse()?;
        if ingest_per_second == 0 {
            return Err("COLLECTOR_INGEST_PER_SECOND must be positive".into());
        }
        let retention_seconds = env::var("COLLECTOR_RETENTION_SECONDS")
            .unwrap_or_else(|_| "2592000".into())
            .parse::<u64>()
            .map_err(|_| "COLLECTOR_RETENTION_SECONDS must be a non-negative whole number")?;
        if retention_seconds > (i64::MAX / 1000) as u64 {
            return Err("COLLECTOR_RETENTION_SECONDS is too large".into());
        }
        let retention_interval_seconds = env::var("COLLECTOR_RETENTION_INTERVAL_SECONDS")
            .unwrap_or_else(|_| "3600".into())
            .parse::<u64>()
            .map_err(|_| "COLLECTOR_RETENTION_INTERVAL_SECONDS must be a positive whole number")?;
        if !(1..=86400).contains(&retention_interval_seconds) {
            return Err("COLLECTOR_RETENTION_INTERVAL_SECONDS must be between 1 and 86400".into());
        }
        let checkpoint_threshold_mib = env::var("COLLECTOR_CHECKPOINT_THRESHOLD_MIB")
            .unwrap_or_else(|_| "16".into())
            .parse::<u32>()
            .map_err(|_| "COLLECTOR_CHECKPOINT_THRESHOLD_MIB must be between 1 and 1024")?;
        if !(1..=1024).contains(&checkpoint_threshold_mib) {
            return Err("COLLECTOR_CHECKPOINT_THRESHOLD_MIB must be between 1 and 1024".into());
        }
        let duckdb_threads = env::var("COLLECTOR_DUCKDB_THREADS")
            .ok()
            .map(|value| -> Result<usize> {
                let count = value
                    .parse::<usize>()
                    .map_err(|_| "COLLECTOR_DUCKDB_THREADS must be between 1 and 256")?;
                if !(1..=256).contains(&count) {
                    return Err("COLLECTOR_DUCKDB_THREADS must be between 1 and 256".into());
                }
                Ok(count)
            })
            .transpose()?;
        Ok(Self {
            checkpoint_threshold_mib,
            duckdb_threads,
            batch: crate::writer::BatchConfig::load()?,
            spool: crate::spool::SpoolConfig::load()?,
            separate_reads: env::var("COLLECTOR_EXPERIMENTAL_READ_WORKER")
                .unwrap_or_else(|_| "false".into())
                .parse()
                .map_err(|_| "COLLECTOR_EXPERIMENTAL_READ_WORKER must be true or false")?,
            profile: env::var("COLLECTOR_PROFILE")
                .unwrap_or_else(|_| "false".into())
                .parse()
                .map_err(|_| "COLLECTOR_PROFILE must be true or false")?,
            trusted_proxies: crate::context::trusted_proxies(
                &env::var("COLLECTOR_TRUSTED_PROXIES").unwrap_or_default(),
            )?,
            store_ip: env::var("COLLECTOR_STORE_IP")
                .unwrap_or_else(|_| "true".into())
                .parse()
                .map_err(|_| "COLLECTOR_STORE_IP must be true or false")?,
            data_dir: env::var_os("COLLECTOR_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|| "data".into()),
            bind: env::var("COLLECTOR_BIND")
                .unwrap_or_else(|_| "127.0.0.1:4174".into())
                .parse()?,
            allowed_origins,
            max_body_bytes,
            max_otlp_records,
            ingest_per_second,
            retention_seconds,
            retention_interval_seconds,
        })
    }

    pub fn initialize(&self) -> Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.data_dir)?;
        let _lock = self.lock()?;
        let keys = Keys {
            projects: None,
            ingest_key: format!("cki_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
            read_token: format!("ckr_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
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

    pub fn keys(&self) -> Result<Keys> {
        let keys: Keys =
            serde_json::from_slice(&fs::read(self.data_dir.join("keys.json")).map_err(
                |_| "Cannot read keys.json. Run the init command with this data directory first.",
            )?)?;
        if keys.ingest_key.len() < 32
            || keys.read_token.len() < 32
            || keys.ingest_key == keys.read_token
        {
            return Err(
                "Invalid keys.json: two distinct keys of at least 32 characters are required."
                    .into(),
            );
        }
        if let Some(projects) = &keys.projects {
            crate::projects::validate(projects)?;
        }
        Ok(keys)
    }

    pub fn rotate_key(&self, kind: &str) -> Result<()> {
        let mut keys = self.keys()?;
        match kind {
            "ingest" => {
                keys.ingest_key =
                    format!("cki_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
            }
            "read" => {
                keys.read_token =
                    format!("ckr_{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
            }
            _ => return Err("Expected rotate-key ingest or rotate-key read".into()),
        }
        if let Some(projects) = &mut keys.projects {
            let project = projects
                .iter_mut()
                .find(|p| p.id == crate::projects::DEFAULT_PROJECT)
                .ok_or("Missing default project")?;
            let id = format!("primary-{kind}");
            let secret = if kind == "ingest" {
                &keys.ingest_key
            } else {
                &keys.read_token
            };
            let key = crate::projects::Credential {
                id: id.clone(),
                name: format!("Primary {kind}"),
                permission: crate::projects::Permission::parse(kind)?,
                hash: crate::projects::hash(secret),
            };
            project.keys.retain(|k| k.id != id);
            project.keys.push(key);
            crate::projects::validate(projects)?;
        }
        self.save_keys(&keys)?;
        println!(
            "Rotated {kind} key. Read the new value from keys.json and restart the collector."
        );
        Ok(())
    }
    pub(crate) fn save_keys(&self, keys: &Keys) -> Result<()> {
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
            #[cfg(unix)]
            fs::File::open(&self.data_dir)?.sync_all()?;
            Ok(())
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
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
