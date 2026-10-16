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
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Keys {
    pub ingest_key: String,
    pub read_token: String,
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
            let uri: axum::http::Uri = origin.parse()?;
            if !matches!(uri.scheme_str(), Some("http" | "https"))
                || uri.authority().is_none()
                || uri.path() != "/"
                || uri.query().is_some()
                || origin.ends_with('/')
                || origin.contains('@')
            {
                return Err("Allowed origins must be exact HTTP(S) origins without a trailing slash or path.".into());
            }
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
        Ok(Self {
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
        println!(
            "Rotated {kind} key. Read the new value from keys.json and restart the collector."
        );
        Ok(())
    }
}
