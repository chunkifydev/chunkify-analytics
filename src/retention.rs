//! Receipt-time expiration policy, loaded with the administrator's project settings.
use crate::projects::Project;
use duckdb::{Connection, params_from_iter, types::Value};

pub const MAX_SECONDS: u64 = (i64::MAX / 1000) as u64;

#[derive(Clone)]
pub struct Policy {
    default_seconds: u64,
    overrides: Vec<(String, u64)>,
}

impl Policy {
    // Configuration and project validation bound durations before construction.
    pub fn new(default_seconds: u64, projects: &[Project]) -> Self {
        Self {
            default_seconds,
            overrides: projects
                .iter()
                .filter_map(|project| {
                    project
                        .settings
                        .retention_seconds
                        .map(|seconds| (project.id.clone(), seconds))
                })
                .collect(),
        }
    }

    pub fn enabled(&self) -> bool {
        self.default_seconds > 0 || self.overrides.iter().any(|(_, seconds)| *seconds > 0)
    }

    pub fn project_cutoff(&self, project: &str, now_ms: i64) -> i64 {
        let seconds = self
            .overrides
            .iter()
            .find(|(id, _)| id == project)
            .map_or(self.default_seconds, |(_, seconds)| *seconds);
        cutoff(now_ms, seconds)
    }

    pub fn purge(&self, connection: &mut Connection, now_ms: i64) -> duckdb::Result<usize> {
        let default_cutoff = cutoff(now_ms, self.default_seconds);
        let (expired, mut values) = if self.overrides.is_empty() {
            (
                "SELECT project_id, event_id FROM events WHERE received_at_ms < ?".into(),
                Vec::new(),
            )
        } else {
            let rows = std::iter::repeat_n("(?, ?)", self.overrides.len())
                .collect::<Vec<_>>()
                .join(", ");
            let values = self
                .overrides
                .iter()
                .flat_map(|(id, seconds)| {
                    [
                        Value::Text(id.clone()),
                        Value::BigInt(cutoff(now_ms, *seconds)),
                    ]
                })
                .collect::<Vec<_>>();
            // Join all overrides in one scan instead of scanning once per project.
            (
                format!(
                    "SELECT events.project_id, events.event_id FROM events
                LEFT JOIN (VALUES {rows}) AS retention(project_id, cutoff_ms) USING (project_id)
                WHERE received_at_ms < COALESCE(retention.cutoff_ms, ?)"
                ),
                values,
            )
        };
        values.push(Value::BigInt(default_cutoff));
        let tx = connection.transaction()?;
        tx.execute(
            &format!("DELETE FROM event_otlp WHERE (project_id, event_id) IN ({expired})"),
            params_from_iter(&values),
        )?;
        let removed = tx.execute(
            &format!("DELETE FROM events WHERE (project_id, event_id) IN ({expired})"),
            params_from_iter(&values),
        )?;
        tx.commit()?;
        Ok(removed)
    }
}

fn cutoff(now_ms: i64, seconds: u64) -> i64 {
    if seconds == 0 {
        // No stored timestamp can precede this value, so expiration is disabled.
        i64::MIN
    } else {
        now_ms.saturating_sub((seconds * 1000) as i64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::projects::Settings;

    fn project(id: &str, seconds: Option<u64>) -> Project {
        Project {
            id: id.into(),
            name: id.into(),
            keys: Vec::new(),
            settings: Settings {
                retention_seconds: seconds,
                ..Settings::default()
            },
        }
    }

    #[test]
    fn expiration_respects_project_overrides_and_exact_receipt_boundary() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE events(project_id VARCHAR, event_id VARCHAR, received_at_ms BIGINT);
            CREATE TABLE event_otlp(project_id VARCHAR, event_id VARCHAR);
            INSERT INTO events VALUES ('inherit', 'same', 8999), ('inherit', 'boundary', 9000),
                ('long', 'same', 8999), ('short', 'same', 9499), ('short', 'boundary', 9500),
                ('keep', 'same', -5000), ('unlisted', 'same', 8999);
            INSERT INTO event_otlp SELECT project_id, event_id FROM events;",
        )
        .unwrap();
        let policy = Policy::new(
            1,
            &[
                project("inherit", None),
                project("long", Some(60)),
                project("short", Some(1)),
                project("keep", Some(0)),
            ],
        );
        assert_eq!(policy.purge(&mut db, 10500).unwrap(), 4);
        let remaining: Vec<String> = db
            .prepare("SELECT project_id FROM events ORDER BY project_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .map(Result::unwrap)
            .collect();
        assert_eq!(remaining, ["keep", "long", "short"]);
        assert_eq!(
            db.query_row("SELECT count(*) FROM event_otlp", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            3
        );
        assert_eq!(policy.purge(&mut db, 10500).unwrap(), 0);
    }

    #[test]
    fn disabled_default_still_allows_project_expiration_and_large_durations() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE events(project_id VARCHAR, event_id VARCHAR, received_at_ms BIGINT);
            CREATE TABLE event_otlp(project_id VARCHAR, event_id VARCHAR);
            INSERT INTO events VALUES ('default', 'same', 0), ('short', 'same', 0), ('long', 'same', 0);").unwrap();
        let disabled = Policy::new(0, &[]);
        assert!(!disabled.enabled());
        assert_eq!(disabled.purge(&mut db, 10000).unwrap(), 0);
        let policy = Policy::new(
            0,
            &[
                project("short", Some(1)),
                project("long", Some(MAX_SECONDS)),
            ],
        );
        assert!(policy.enabled());
        assert_eq!(policy.purge(&mut db, 10000).unwrap(), 1);
        assert_eq!(
            db.query_row("SELECT count(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            2
        );
    }

    #[test]
    fn failed_event_deletion_rolls_back_otlp_deletion() {
        let mut db = Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE VIEW events AS SELECT 'default' AS project_id, 'same' AS event_id, 0 AS received_at_ms;
            CREATE TABLE event_otlp(project_id VARCHAR, event_id VARCHAR);
            INSERT INTO event_otlp VALUES ('default', 'same');").unwrap();
        assert!(Policy::new(1, &[]).purge(&mut db, 10000).is_err());
        assert_eq!(
            db.query_row("SELECT count(*) FROM event_otlp", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }
}
