use serde::Serialize;
use std::{collections::BTreeMap, time::Instant};

#[derive(Default, Serialize)]
struct Timing {
    count: u64,
    total_ms: f64,
    max_ms: f64,
}

impl Timing {
    fn record(&mut self, milliseconds: f64) {
        self.count += 1;
        self.total_ms += milliseconds;
        self.max_ms = self.max_ms.max(milliseconds);
    }
}

#[derive(Default, Serialize)]
struct JobTiming {
    queue_wait: Timing,
    execution: Timing,
}

#[derive(Default, Serialize)]
struct Ingest {
    transactions: u64,
    requests: usize,
    total_bytes: usize,
    max_bytes: usize,
    max_requests: usize,
    received_records: usize,
    inserted_records: usize,
    max_records: usize,
}

#[derive(Serialize)]
struct DatabaseSettings {
    version: String,
    checkpoint_threshold: String,
    threads: i64,
    memory_limit: String,
}

// Fixed operation names and effective settings only. No SQL, keys, or event data.
#[derive(Serialize)]
pub struct Profile {
    r#type: &'static str,
    version: u8,
    #[serde(skip)]
    enabled: bool,
    jobs: BTreeMap<&'static str, JobTiming>,
    ingest_stages: BTreeMap<&'static str, Timing>,
    ingest: Ingest,
    #[serde(skip_serializing_if = "Option::is_none")]
    database: Option<DatabaseSettings>,
}

impl Profile {
    pub fn new(enabled: bool, connection: &duckdb::Connection) -> duckdb::Result<Self> {
        let database = if enabled {
            Some(connection.query_row(
                "SELECT version(), current_setting('checkpoint_threshold'), current_setting('threads'), current_setting('memory_limit')",
                [],
                |row| Ok(DatabaseSettings { version: row.get(0)?, checkpoint_threshold: row.get(1)?, threads: row.get(2)?, memory_limit: row.get(3)? }),
            )?)
        } else {
            None
        };
        Ok(Self {
            r#type: "collector_profile",
            version: 2,
            enabled,
            jobs: BTreeMap::new(),
            ingest_stages: BTreeMap::new(),
            ingest: Ingest::default(),
            database,
        })
    }

    pub fn start(&self) -> Option<Instant> {
        self.enabled.then(Instant::now)
    }

    pub fn queue(&mut self, name: &'static str, queued: Option<Instant>, started: Option<Instant>) {
        if let (Some(queued), Some(started)) = (queued, started) {
            self.jobs
                .entry(name)
                .or_default()
                .queue_wait
                .record((started - queued).as_secs_f64() * 1000.0);
        }
    }

    pub fn executed(&mut self, name: &'static str, started: Option<Instant>) {
        if let Some(started) = started {
            self.jobs
                .entry(name)
                .or_default()
                .execution
                .record(started.elapsed().as_secs_f64() * 1000.0);
        }
    }

    pub fn job(&mut self, name: &'static str, queued: Option<Instant>, started: Option<Instant>) {
        self.queue(name, queued, started);
        self.executed(name, started);
    }

    pub fn duration(&mut self, name: &'static str, elapsed: std::time::Duration) {
        if self.enabled {
            self.ingest_stages
                .entry(name)
                .or_default()
                .record(elapsed.as_secs_f64() * 1000.0);
        }
    }

    pub fn measure<T>(&mut self, name: &'static str, operation: impl FnOnce() -> T) -> T {
        let started = self.start();
        let result = operation();
        if let Some(started) = started {
            self.ingest_stages
                .entry(name)
                .or_default()
                .record(started.elapsed().as_secs_f64() * 1000.0);
        }
        result
    }

    pub fn committed(&mut self, requests: usize, received: usize, bytes: usize, inserted: usize) {
        if self.enabled {
            self.ingest.transactions += 1;
            self.ingest.requests += requests;
            self.ingest.max_requests = self.ingest.max_requests.max(requests);
            self.ingest.total_bytes += bytes;
            self.ingest.max_bytes = self.ingest.max_bytes.max(bytes);
            self.ingest.received_records += received;
            self.ingest.inserted_records += inserted;
            self.ingest.max_records = self.ingest.max_records.max(received);
        }
    }

    pub fn report(&self) {
        if self.enabled {
            eprintln!(
                "{}",
                serde_json::to_string(self).expect("serialize aggregate timings")
            );
        }
    }
}
