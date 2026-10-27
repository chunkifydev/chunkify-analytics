//! Immutable event batches in S3-compatible storage. DuckDB writes and reads Parquet;
//! the small S3 client stores installation keys and removes expired objects.
use crate::{api::ApiError, backend::S3Config, retention::Policy};
use duckdb::Connection;
use s3::{Bucket, Region, creds::Credentials};
use std::{
    path::Path,
    sync::atomic::{AtomicI64, Ordering},
    time::Duration,
};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

pub fn literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

impl S3Config {
    fn client(&self) -> Result<Box<Bucket>> {
        let credentials = Credentials::new(
            self.access_key_id.as_deref(),
            self.secret_access_key.as_deref(),
            None,
            self.session_token.as_deref(),
            None,
        )
        .map_err(|_| "Cannot load S3 credentials")?;
        let endpoint = self
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("s3.{}.amazonaws.com", self.region));
        let region = Region::Custom {
            region: self.region.clone(),
            endpoint: format!(
                "{}://{endpoint}",
                if self.use_ssl { "https" } else { "http" }
            ),
        };
        let bucket = Bucket::new(&self.bucket, region, credentials)
            .map_err(|_| "Cannot configure S3 client")?;
        let mut bucket = if self.url_style == "path" {
            bucket.with_path_style()
        } else {
            bucket
        };
        bucket.set_request_timeout(Some(Duration::from_secs(30)));
        Ok(bucket)
    }

    pub fn read_keys(&self) -> Result<Option<Vec<u8>>> {
        let response = self
            .client()?
            .get_object(format!("{}keys.json", self.prefix))
            .map_err(|_| "Cannot read installation keys from S3")?;
        match response.status_code() {
            200 => Ok(Some(response.as_slice().to_vec())),
            404 => Ok(None),
            _ => Err("Cannot read installation keys from S3".into()),
        }
    }

    pub fn write_keys(&self, bytes: &[u8]) -> Result<()> {
        let response = self
            .client()?
            .put_object(format!("{}keys.json", self.prefix), bytes)
            .map_err(|_| "Cannot save installation keys to S3")?;
        if !(200..300).contains(&response.status_code()) {
            return Err("Cannot save installation keys to S3".into());
        }
        Ok(())
    }

    fn uri(&self, key: &str) -> String {
        format!("s3://{}/{key}", self.bucket)
    }
}

pub struct RemoteStore {
    config: S3Config,
    client: Box<Bucket>,
    next_id: AtomicI64,
}

struct EventFile {
    key: String,
    project: String,
    max_received: i64,
    rows: usize,
}

impl RemoteStore {
    pub fn open(config: S3Config, connection: &Connection, directory: &Path) -> Result<Self> {
        let extensions = directory.join("extensions");
        std::fs::create_dir_all(&extensions)?;
        connection
            .execute_batch(&format!(
                "SET extension_directory = {}; INSTALL httpfs; LOAD httpfs;",
                literal(&extensions.to_string_lossy())
            ))
            .map_err(|error| format!("Cannot load DuckDB httpfs extension: {error}"))?;
        let mut options = vec![
            "TYPE s3".to_owned(),
            format!("REGION {}", literal(&config.region)),
            format!("URL_STYLE {}", literal(&config.url_style)),
            format!("USE_SSL {}", config.use_ssl),
            format!("SCOPE {}", literal(&config.uri(&config.prefix))),
        ];
        if let Some(endpoint) = &config.endpoint {
            options.push(format!("ENDPOINT {}", literal(endpoint)));
        }
        if let Some(key) = &config.access_key_id {
            options.push(format!("KEY_ID {}", literal(key)));
            options.push(format!(
                "SECRET {}",
                literal(
                    config
                        .secret_access_key
                        .as_deref()
                        .expect("validated credentials")
                )
            ));
            if let Some(token) = &config.session_token {
                options.push(format!("SESSION_TOKEN {}", literal(token)));
            }
        } else {
            connection
                .execute_batch("INSTALL aws; LOAD aws;")
                .map_err(|_| "Cannot load DuckDB AWS credential provider")?;
            options.push("PROVIDER credential_chain".into());
            options.push("REFRESH auto".into());
        }
        // Never return DuckDB's SQL diagnostics here: CREATE SECRET contains credentials.
        connection
            .execute_batch(&format!(
                "CREATE SECRET collector_s3 ({});",
                options.join(", ")
            ))
            .map_err(|_| "Cannot configure DuckDB S3 credentials")?;
        crate::storage::create_parquet_stage(connection)?;
        let store = Self {
            client: config.client()?,
            config,
            next_id: AtomicI64::new(0),
        };
        let files = store
            .files(None)
            .map_err(|_| "Cannot list S3 event files")?;
        let previous: i64 = if files.is_empty() {
            0
        } else {
            connection
                .query_row(
                    &format!(
                        "SELECT COALESCE(MAX(ingest_id), 0) FROM {}",
                        store.scan(&files)
                    ),
                    [],
                    |row| row.get(0),
                )
                .map_err(|_| "Cannot read S3 event ordering")?
        };
        store.next_id.store(
            previous
                .max(chrono::Utc::now().timestamp_micros())
                .checked_add(1)
                .ok_or("S3 event ordering exhausted")?,
            Ordering::Relaxed,
        );
        Ok(store)
    }

    pub fn next_id(&self) -> std::result::Result<i64, ApiError> {
        self.next_id
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| ApiError::internal())
    }

    fn files(&self, project: Option<&str>) -> std::result::Result<Vec<EventFile>, ApiError> {
        let prefix = match project {
            Some(project) => format!("{}events/project_id={project}/", self.config.prefix),
            None => format!("{}events/", self.config.prefix),
        };
        let pages = self
            .client
            .list(prefix, None)
            .map_err(|_| ApiError::internal())?;
        Ok(pages
            .into_iter()
            .flat_map(|page| page.contents)
            .filter_map(|object| self.parse_file(object.key))
            .collect())
    }

    fn parse_file(&self, key: String) -> Option<EventFile> {
        let relative = key.strip_prefix(&format!("{}events/", self.config.prefix))?;
        let parts: Vec<_> = relative.split('/').collect();
        if parts.len() != 3 {
            return None;
        }
        let project = parts[0].strip_prefix("project_id=")?.to_owned();
        chrono::NaiveDate::parse_from_str(parts[1].strip_prefix("received_date=")?, "%Y-%m-%d")
            .ok()?;
        let mut name = parts[2]
            .strip_prefix("batch-")?
            .strip_suffix(".parquet")?
            .splitn(3, '-');
        let max_received = name.next()?.parse().ok()?;
        let rows = name.next()?.parse().ok()?;
        uuid::Uuid::parse_str(name.next()?).ok()?;
        Some(EventFile {
            key,
            project,
            max_received,
            rows,
        })
    }

    fn scan(&self, files: &[EventFile]) -> String {
        let paths: Vec<_> = files
            .iter()
            .map(|file| literal(&self.config.uri(&file.key)))
            .collect();
        format!(
            "read_parquet([{}], hive_partitioning = false)",
            paths.join(",")
        )
    }

    pub fn prepare_read(
        &self,
        connection: &Connection,
        project: &str,
        policy: &Policy,
    ) -> std::result::Result<(), ApiError> {
        let files = self.files(Some(project))?;
        let source = if files.is_empty() {
            "(SELECT * FROM collector_parquet_stage WHERE FALSE)".into()
        } else {
            self.scan(&files)
        };
        let cutoff = policy.project_cutoff(project, chrono::Utc::now().timestamp_millis());
        connection.execute_batch(&format!("CREATE OR REPLACE TEMP VIEW collector_parquet_events AS SELECT * FROM {source}
            WHERE project_id = {} AND received_at_ms >= {cutoff};
            CREATE OR REPLACE TEMP VIEW events AS SELECT ingest_id, {} FROM collector_parquet_events;", literal(project), *crate::storage::COLUMN_NAMES))?;
        Ok(())
    }

    pub fn publish(&self, connection: &Connection) -> std::result::Result<(), ApiError> {
        let groups = connection
            .prepare(
                "SELECT project_id, epoch_ms(received_at_ms)::DATE::VARCHAR,
            MAX(received_at_ms), COUNT(*) FROM collector_parquet_stage GROUP BY 1, 2",
            )?
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, usize>(3)?,
                ))
            })?
            .collect::<duckdb::Result<Vec<_>>>()?;
        for (project, date, received, count) in groups {
            let key = format!(
                "{}events/project_id={project}/received_date={date}/batch-{received}-{count}-{}.parquet",
                self.config.prefix,
                uuid::Uuid::new_v4()
            );
            connection.execute_batch(&format!("COPY (SELECT * FROM collector_parquet_stage WHERE project_id = {} AND epoch_ms(received_at_ms)::DATE = DATE {})
                TO {} (FORMAT parquet, COMPRESSION zstd);", literal(&project), literal(&date), literal(&self.config.uri(&key))))?;
        }
        Ok(())
    }

    pub fn purge(&self, policy: &Policy, now: i64) -> std::result::Result<usize, ApiError> {
        let mut removed = 0;
        for file in self.files(None)? {
            if file.max_received < policy.project_cutoff(&file.project, now) {
                let response = self
                    .client
                    .delete_object(&file.key)
                    .map_err(|_| ApiError::internal())?;
                if !(200..300).contains(&response.status_code()) {
                    return Err(ApiError::internal());
                }
                removed += file.rows;
            }
        }
        Ok(removed)
    }
}
