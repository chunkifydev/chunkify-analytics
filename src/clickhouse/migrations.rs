//! Schema migrations. The numbered SQL files under `migrations/` are the only
//! definition of the tables: `init` and `migrate` apply them in order, and
//! `serve` only checks that the database is at this release's version.
//!
//! The state table has golang-migrate's layout. The row with the highest
//! `sequence` holds the current version, and `dirty` marks a migration that
//! started and did not finish. ClickHouse DDL is not transactional, so every
//! statement must be safe to run again (`IF NOT EXISTS`, `IF EXISTS`): an
//! interrupted migration is retried from its first statement.
use super::{CONTROL_TIMEOUT, Client, Params};
use crate::api::ApiError;
use serde_json::Value;

/// Version, name and SQL of every migration, in order. Released files never
/// change: a schema change is a new file, registered here.
const MIGRATIONS: &[(i64, &str, &str)] =
    &[(1, "init", include_str!("migrations/0001_init.up.sql"))];

pub const UNREACHABLE: &str =
    "Cannot reach ClickHouse; check clickhouse.url, credentials and permissions";
pub const NOT_INITIALIZED: &str = "This ClickHouse database is not initialized; run init first";
pub const PENDING: &str = "The ClickHouse database needs schema migrations; run `chunkify-analytics migrate`, then start again";
pub const DIRTY: &str =
    "A ClickHouse schema migration did not finish; run `chunkify-analytics migrate` again";
pub const NEWER: &str = "The ClickHouse schema is newer than this release supports; upgrade chunkify-analytics before using this database";
pub const INCOMPLETE: &str = "The ClickHouse database is still missing tables after every migration ran; check that this ClickHouse server finished creating them, then run `chunkify-analytics migrate` again. Missing:";
pub const OVERLAP: &str = "Another `chunkify-analytics migrate` ran at the same time; when it has stopped, run `chunkify-analytics migrate` again";

/// The version this release needs.
pub fn latest() -> i64 {
    MIGRATIONS.last().map_or(0, |(version, _, _)| *version)
}

/// The tables of a migrated database, as the generated `schema.sql` lists
/// them. A recorded version does not prove they exist: a restore can bring
/// the version back without them.
fn expected_tables() -> Vec<&'static str> {
    include_str!("schema.sql")
        .lines()
        .filter_map(|line| {
            line.strip_prefix("CREATE TABLE ")
                .or_else(|| line.strip_prefix("CREATE MATERIALIZED VIEW "))
        })
        .filter_map(|rest| rest.split_whitespace().next())
        .map(|name| name.trim_matches('`'))
        .collect()
}

/// The statements of a migration file. A statement ends at a line that ends
/// with `;`, and lines that start with `--` are comments.
fn statements(sql: &str) -> Vec<String> {
    let mut statements = Vec::new();
    let mut current = String::new();
    for line in sql.lines() {
        let line = line.trim_end();
        if line.trim_start().starts_with("--") {
            continue;
        }
        match line.strip_suffix(';') {
            Some(end) => {
                current.push_str(end);
                statements.push(std::mem::take(&mut current).trim().to_owned());
            }
            None => {
                current.push_str(line);
                current.push('\n');
            }
        }
    }
    // Text after the last `;` is a statement too; a test rejects such files.
    if !current.trim().is_empty() {
        statements.push(current.trim().to_owned());
    }
    statements.retain(|statement| !statement.is_empty());
    statements
}

/// In a Replicated database, plain MergeTree tables keep their data on one
/// node; replicate every table so all servers share the same data.
fn replicate(statement: &str) -> String {
    statement
        .replace("ENGINE = MergeTree", "ENGINE = ReplicatedMergeTree")
        .replace(
            "ENGINE = AggregatingMergeTree",
            "ENGINE = ReplicatedAggregatingMergeTree",
        )
}

fn unreachable(_: ApiError) -> String {
    UNREACHABLE.into()
}

/// The schema version a database is at.
struct State {
    version: i64,
    dirty: bool,
    sequence: u64,
}

impl Client {
    /// The database's engine, or `None` when the database does not exist.
    async fn database_engine(&self) -> Result<Option<String>, ApiError> {
        let mut params = Params::default();
        let sql = format!(
            "SELECT engine FROM system.databases WHERE name = {} FORMAT TSV",
            params.text(&self.config.database)
        );
        // Without a default database: the configured one may not exist.
        let engine = self
            .send(sql, &params, &[], false)
            .await?
            .text()
            .await
            .map_err(|_| ApiError::unavailable())?;
        Ok(Some(engine.trim().to_owned()).filter(|engine| !engine.is_empty()))
    }

    /// Whether the configured database has this table. Works without the
    /// database, which may not exist.
    pub(super) async fn table_exists(&self, table: &str) -> Result<bool, ApiError> {
        let mut params = Params::default();
        let sql = format!(
            "SELECT count() FROM system.tables WHERE database = {} AND name = {} FORMAT TSV",
            params.text(&self.config.database),
            params.text(table)
        );
        let count = self
            .send(sql, &params, &[], false)
            .await?
            .text()
            .await
            .map_err(|_| ApiError::unavailable())?;
        Ok(count.trim() != "0")
    }

    /// The recorded schema version, or `None` before any migration started.
    /// Never creates anything.
    async fn schema_state(&self) -> Result<Option<State>, ApiError> {
        if !self.table_exists("schema_migrations").await? {
            return Ok(None);
        }
        let rows = self
            .all(
                "SELECT version, dirty, sequence FROM schema_migrations ORDER BY sequence DESC LIMIT 1",
                &Params::default(),
            )
            .await?;
        let state = |row: &Vec<Value>| {
            Some(State {
                version: row.first()?.as_i64()?,
                dirty: row.get(1)?.as_u64()? != 0,
                sequence: row.get(2)?.as_u64()?,
            })
        };
        rows.first()
            .map(|row| state(row).ok_or_else(ApiError::internal))
            .transpose()
    }

    /// The tables of this release that the database does not have.
    async fn missing_tables(&self) -> Result<Vec<&'static str>, ApiError> {
        let mut params = Params::default();
        let sql = format!(
            "SELECT name FROM system.tables WHERE database = {} FORMAT TSV",
            params.text(&self.config.database)
        );
        let names = self
            .send(sql, &params, &[], false)
            .await?
            .text()
            .await
            .map_err(|_| ApiError::unavailable())?;
        let existing: std::collections::HashSet<&str> = names.lines().collect();
        Ok(expected_tables()
            .into_iter()
            .filter(|table| !existing.contains(table))
            .collect())
    }

    /// Whether this release can use the database as it is. `serve` calls this
    /// at startup and changes nothing.
    pub async fn check_schema(&self) -> Result<(), String> {
        match self.schema_state().await.map_err(unreachable)? {
            Some(state) if state.version > latest() => Err(NEWER.into()),
            Some(state) if state.dirty => Err(DIRTY.into()),
            Some(state) if state.version == latest() => {
                // The version alone is not enough after a partial restore.
                match self.missing_tables().await.map_err(unreachable)? {
                    missing if missing.is_empty() => Ok(()),
                    _ => Err(PENDING.into()),
                }
            }
            _ => Err(PENDING.into()),
        }
    }

    async fn record(&self, version: i64, dirty: bool, sequence: u64) -> Result<(), String> {
        let mut params = Params::default();
        let sql = format!(
            "INSERT INTO schema_migrations (version, dirty, sequence) SELECT {}, {}, {}",
            params.int(version),
            params.int(dirty.into()),
            params.int(sequence as i64)
        );
        self.execute_with(sql, &params, &[("async_insert", "0")])
            .await
            .map_err(unreachable)
    }

    /// Run one migration statement and wait for its mutations. Unlike other
    /// requests, a failure reports ClickHouse's message: migrations hold no
    /// event data, and the operator needs the reason.
    async fn run(&self, statement: String) -> Result<(), String> {
        let response = self
            .request(
                statement,
                &Params::default(),
                &[("mutations_sync", "2")],
                true,
            )
            .send()
            .await
            .map_err(|_| UNREACHABLE.to_owned())?;
        let failed = !response.status().is_success();
        let body = response.text().await.map_err(|_| UNREACHABLE.to_owned())?;
        if failed || body.contains("DB::Exception") {
            return Err(body.lines().next().unwrap_or("unknown error").to_owned());
        }
        Ok(())
    }

    /// Whether another run recorded a row after this run's dirty marker at
    /// `started`, not counting this run's own row at `finished`, and the
    /// highest sequence recorded.
    async fn overlapped(&self, started: u64, finished: u64) -> Result<(bool, u64), String> {
        let mut params = Params::default();
        let sql = format!(
            "SELECT countIf(sequence > {} AND sequence != {}), max(sequence) FROM schema_migrations",
            params.int(started as i64),
            params.int(finished as i64)
        );
        let row = self.one(&sql, &params).await.map_err(unreachable)?;
        let number = |index: usize| row.get(index).and_then(Value::as_u64);
        match (number(0), number(1)) {
            (Some(later), Some(highest)) => Ok((later > 0, highest)),
            _ => Err(UNREACHABLE.into()),
        }
    }

    /// Apply every pending migration and return the names of those applied.
    /// When tables are still missing afterwards, for example after a restore
    /// that kept the version but not every table, apply all migrations again
    /// from the first to recreate them. Only `init` creates a missing
    /// database. Safe to run again after a failure. Run it from one place at
    /// a time: nothing locks the database,
    /// but runs that overlap leave the migration marked unfinished, so it is
    /// repeated from its start by the next run.
    pub async fn migrate(&self, create_database: bool) -> Result<Vec<String>, String> {
        let control = self.with_timeout(CONTROL_TIMEOUT);
        // Migrations can rewrite data, so their statements have no time limit.
        let unlimited = Self {
            timeout: None,
            ..self.clone()
        };
        let mut engine = control.database_engine().await.map_err(unreachable)?;
        if engine.is_none() {
            if !create_database {
                return Err(NOT_INITIALIZED.into());
            }
            control
                .send(
                    format!("CREATE DATABASE IF NOT EXISTS {}", self.config.database),
                    &Params::default(),
                    &[],
                    false,
                )
                .await
                .map_err(unreachable)?
                .text()
                .await
                .map_err(|_| UNREACHABLE.to_owned())?;
            engine = control.database_engine().await.map_err(unreachable)?;
        }
        let replicated = engine.as_deref() == Some("Replicated");
        let prepared = |statement: &str| {
            if replicated {
                replicate(statement)
            } else {
                statement.to_owned()
            }
        };
        let state = control
            .schema_state()
            .await
            .map_err(unreachable)?
            .unwrap_or(State {
                version: 0,
                dirty: false,
                sequence: 0,
            });
        if state.version > latest() {
            return Err(NEWER.into());
        }
        control
            .run(prepared(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    version Int64, dirty UInt8, sequence UInt64
                ) ENGINE = MergeTree ORDER BY sequence",
            ))
            .await?;
        // Later rows win, whatever the clocks of the machines that wrote them.
        let mut sequence = state.sequence;
        let mut next = |above: u64| {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_nanos() as u64);
            sequence = now.max(sequence.max(above) + 1);
            sequence
        };
        let mut applied = Vec::new();
        for repair in [false, true] {
            if repair {
                // Every migration was recorded, and tables are missing.
                if control
                    .missing_tables()
                    .await
                    .map_err(unreachable)?
                    .is_empty()
                {
                    break;
                }
                applied.clear();
            }
            for (version, name, sql) in MIGRATIONS {
                let pending = repair
                    || *version > state.version
                    || (*version == state.version && state.dirty);
                if !pending {
                    continue;
                }
                let label = format!("{version:04}_{name}");
                let started = next(0);
                control.record(*version, true, started).await?;
                for statement in statements(sql) {
                    unlimited.run(prepared(&statement)).await.map_err(|error| {
                        format!("Migration {label} failed: {error}. Fix the cause, then run `chunkify-analytics migrate` again")
                    })?;
                }
                // Another run may have repeated statements while these ran, for
                // example emptying a table this run was filling. Every run records
                // its dirty row before its first statement, so of two overlapping
                // runs at least the one with the lower row sees the other's.
                let mut overlap = control.overlapped(started, started).await?.0;
                if !overlap {
                    let finished = next(0);
                    control.record(*version, false, finished).await?;
                    // The other run may have started since the check.
                    overlap = control.overlapped(started, finished).await?.0;
                }
                if overlap {
                    // Leave the migration dirty above every row, including a
                    // clean row the other run records meanwhile.
                    for _ in 0..5 {
                        let highest = control.overlapped(started, started).await?.1;
                        let dirty = next(highest);
                        control.record(*version, true, dirty).await?;
                        if control.overlapped(started, started).await?.1 == dirty {
                            break;
                        }
                    }
                    return Err(OVERLAP.into());
                }
                applied.push(label);
            }
        }
        let missing = control.missing_tables().await.map_err(unreachable)?;
        if !missing.is_empty() {
            return Err(format!("{INCOMPLETE} {}", missing.join(", ")));
        }
        Ok(applied)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::clickhouse::{Store, testing};
    use std::sync::Arc;

    /// A client for a new database name, a way to start a server on it, and
    /// the guard that drops the database.
    fn database() -> (Client, crate::config::Config, testing::Disposable) {
        let config = testing::config("retention.seconds = 0");
        let guard = testing::Disposable(config.clickhouse.clone());
        let client = Client::new(config.clickhouse.clone()).unwrap();
        (client, config, guard)
    }

    async fn start(config: &crate::config::Config) -> Result<(), String> {
        let keys = crate::config::Keys {
            projects: vec![crate::projects::primary_project(
                "test-public",
                "test-secret",
            )],
        };
        let registry = Arc::new(crate::projects::Registry::new(&keys, config).unwrap());
        Store::open(config, registry)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }

    #[tokio::test]
    async fn servers_start_only_at_this_release_s_schema_version() {
        let (client, config, _guard) = database();
        // Neither a server nor `migrate` creates a database, for example
        // under a mistyped name.
        assert_eq!(start(&config).await.unwrap_err(), PENDING);
        assert_eq!(client.migrate(false).await.unwrap_err(), NOT_INITIALIZED);
        assert_eq!(client.database_engine().await.unwrap(), None);

        let all: Vec<String> = MIGRATIONS
            .iter()
            .map(|(version, name, _)| format!("{version:04}_{name}"))
            .collect();
        assert_eq!(client.migrate(true).await.unwrap(), all);
        start(&config).await.unwrap();
        // Nothing is pending the second time.
        assert!(client.migrate(false).await.unwrap().is_empty());

        // A migration that stopped halfway is retried from its start.
        let record = |version: i64, dirty: u8| {
            client.run(format!(
                "INSERT INTO schema_migrations
                SELECT {version}, {dirty}, max(sequence) + 1 FROM schema_migrations"
            ))
        };
        record(latest(), 1).await.unwrap();
        assert_eq!(start(&config).await.unwrap_err(), DIRTY);
        assert_eq!(client.migrate(false).await.unwrap(), all[all.len() - 1..]);
        start(&config).await.unwrap();

        // A database behind this release needs `migrate`.
        record(latest() - 1, 0).await.unwrap();
        assert_eq!(start(&config).await.unwrap_err(), PENDING);
        assert_eq!(client.migrate(false).await.unwrap(), all[all.len() - 1..]);
        start(&config).await.unwrap();

        // A later release's database is left alone.
        record(latest() + 1, 0).await.unwrap();
        assert_eq!(start(&config).await.unwrap_err(), NEWER);
        assert_eq!(client.migrate(false).await.unwrap_err(), NEWER);
    }

    #[tokio::test]
    async fn tables_missing_behind_a_recorded_version_are_recreated() {
        let (client, config, _guard) = database();
        let all: Vec<String> = MIGRATIONS
            .iter()
            .map(|(version, name, _)| format!("{version:04}_{name}"))
            .collect();
        client.migrate(true).await.unwrap();
        assert!(client.missing_tables().await.unwrap().is_empty());
        // A restore can bring back the version without every table.
        for table in ["view_states_insert", "rollup"] {
            client.run(format!("DROP TABLE {table}")).await.unwrap();
        }
        assert_eq!(
            client.missing_tables().await.unwrap(),
            ["rollup", "view_states_insert"]
        );
        assert_eq!(start(&config).await.unwrap_err(), PENDING);
        assert_eq!(client.migrate(false).await.unwrap(), all);
        start(&config).await.unwrap();
        assert!(client.migrate(false).await.unwrap().is_empty());
    }

    #[test]
    fn expected_tables_come_from_the_schema_file() {
        let tables = expected_tables();
        for table in ["events", "registry", "rollup", "schema_migrations"] {
            assert!(tables.contains(&table), "{table}");
        }
        // The materialized view counts: without it no view state is stored.
        assert!(tables.contains(&"view_states_insert"));
        assert!(tables.iter().all(|table| !table.contains('`')));
    }

    #[tokio::test]
    async fn overlapping_runs_leave_the_migration_to_repeat() {
        let (client, config, _guard) = database();
        // Both runs start before either finishes: each needs many requests.
        let runs = tokio::join!(client.migrate(true), client.migrate(true));
        let runs = [runs.0, runs.1];
        for run in &runs {
            if let Err(error) = run {
                assert_eq!(error, OVERLAP);
            }
        }
        // A run that saw the other never leaves the schema marked complete.
        if runs.iter().any(Result::is_err) {
            assert_eq!(start(&config).await.unwrap_err(), DIRTY);
        }
        client.migrate(false).await.unwrap();
        start(&config).await.unwrap();

        // Rows after a run's dirty marker are another run's, except its own
        // completion.
        let state = client.schema_state().await.unwrap().unwrap();
        let top = state.sequence;
        assert_eq!(client.overlapped(top, top).await.unwrap(), (false, top));
        assert_eq!(client.overlapped(top - 1, top).await.unwrap(), (false, top));
        assert_eq!(
            client.overlapped(top - 1, top - 1).await.unwrap(),
            (true, top)
        );
        assert_eq!(client.overlapped(0, top).await.unwrap(), (true, top));
    }

    #[test]
    fn statements_end_at_semicolon_lines_and_skip_comments() {
        let sql = "-- A comment; with a semicolon;\nCREATE TABLE a (\n    b String -- not a comment line\n);\n\n  -- indented comment\nDROP TABLE IF EXISTS c;\n";
        assert_eq!(
            statements(sql),
            [
                "CREATE TABLE a (\n    b String -- not a comment line\n)",
                "DROP TABLE IF EXISTS c"
            ]
        );
        assert_eq!(statements("SELECT 1"), ["SELECT 1"]);
    }

    #[test]
    fn replicated_databases_get_replicated_engines() {
        assert_eq!(
            replicate("CREATE TABLE a (b String) ENGINE = MergeTree ORDER BY b"),
            "CREATE TABLE a (b String) ENGINE = ReplicatedMergeTree ORDER BY b"
        );
        assert_eq!(
            replicate("CREATE TABLE a ENGINE = AggregatingMergeTree ORDER BY b"),
            "CREATE TABLE a ENGINE = ReplicatedAggregatingMergeTree ORDER BY b"
        );
    }

    /// Every file under `migrations/` is registered under its own number and
    /// name, numbers have no gaps, and every statement ends with `;`.
    #[test]
    fn migration_files_are_registered_in_order() {
        let directory =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/clickhouse/migrations");
        let mut files: Vec<String> = std::fs::read_dir(directory)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        let registered: Vec<String> = MIGRATIONS
            .iter()
            .map(|(version, name, _)| format!("{version:04}_{name}.up.sql"))
            .collect();
        assert_eq!(files, registered);
        for (index, (version, name, sql)) in MIGRATIONS.iter().enumerate() {
            assert_eq!(*version, index as i64 + 1, "{name}");
            assert!(!statements(sql).is_empty(), "{name} has no statements");
            let last = sql
                .lines()
                .rfind(|line| !line.trim().is_empty() && !line.trim_start().starts_with("--"));
            assert!(
                last.is_some_and(|line| line.trim_end().ends_with(';')),
                "{name} must end its last statement with `;`"
            );
        }
    }
}
