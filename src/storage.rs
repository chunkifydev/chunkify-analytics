//! Versioned, lossless event storage. Writers, migrations and reads share this
//! field map; unknown properties and values that do not fit stay in JSON.
use crate::{api::ApiError, context::event_context_v1, event};
use duckdb::{Connection, Row, appender_params_from_iter, types::Value as SqlValue};
use serde_json::{Map, Value, json};
use std::sync::LazyLock;

#[derive(Clone, Copy)]
enum Kind {
    Text,
    Number,
    Integer,
    Boolean,
}
use Kind::*;

// This is a storage schema, independent of which fields today's validator
// requires. Changing it requires a new database version and migration.
const EVENT_FIELDS: &[(&str, Kind)] = &[
    ("schema_version", Integer),
    ("occurred_at", Text),
    ("sdk_name", Text),
    ("sdk_version", Text),
    ("connector_name", Text),
    ("connector_version", Text),
    ("player_name", Text),
    ("player_version", Text),
    ("delivery_hostname", Text),
    ("environment", Text),
    ("reason", Text),
    ("startup_outcome", Text),
    ("error_category", Text),
    ("error_fatal", Boolean),
    ("error_playhead_seconds", Number),
    ("rebuffer_count", Number),
    ("rebuffer_duration_ms", Number),
    ("seek_count", Number),
    ("seek_duration_ms", Number),
    ("maximum_playhead_seconds", Number),
    ("media_duration_seconds", Number),
    ("startup_time_ms", Number),
    ("first_video_frame_time_ms", Number),
    ("rebuffer_ratio", Number),
    ("furthest_progress_ratio", Number),
    ("media_width_px", Number),
    ("media_height_px", Number),
    ("player_width_px", Number),
    ("player_height_px", Number),
    ("dropped_video_frames", Number),
    ("total_video_frames", Number),
    ("selected_rendition_width", Number),
    ("selected_rendition_height", Number),
    ("selected_rendition_bitrate_bps", Number),
    ("selected_rendition_codec", Text),
    ("selected_rendition_frame_rate", Number),
    ("rendition_switch_count", Number),
    ("delivery_request_count", Number),
    ("delivery_transferred_bytes", Number),
    ("delivery_throughput_bps", Number),
    ("playhead_seconds", Number),
    ("from_state", Text),
    ("to_state", Text),
    ("previous_state_duration_ms", Number),
    ("previous_rendition_width", Number),
    ("previous_rendition_height", Number),
    ("previous_rendition_bitrate_bps", Number),
    ("previous_rendition_frame_rate", Number),
    ("previous_rendition_codec", Text),
    ("seek_started_at", Text),
    ("last_seeked_at", Text),
    ("playback_resumed_at", Text),
    ("state_before_seek", Text),
    ("state_after_seek", Text),
    ("from_playhead_seconds", Number),
    ("to_playhead_seconds", Number),
    ("seek_resume_wait_ms", Number),
    ("seek_interaction_duration_ms", Number),
    ("seek_outcome", Text),
    // Storage version 6.
    ("user_id", Text),
    ("video_title", Text),
];
const BROWSER_FIELDS: &[(&str, Kind)] = &[
    ("schema_version", Integer),
    ("user_agent", Text),
    ("browser_name", Text),
    ("browser_version", Text),
    ("os_name", Text),
    ("os_version", Text),
    ("device_type", Text),
    ("device_family", Text),
    ("language", Text),
    ("language_prefix", Text),
    ("timezone", Text),
    ("timezone_offset_minutes", Number),
    ("screen_width_px", Number),
    ("screen_height_px", Number),
    ("viewport_width_px", Number),
    ("viewport_height_px", Number),
    ("pixel_ratio", Number),
];
const BASE: &[(&str, &str)] = &[
    ("event_id", "VARCHAR"),
    ("view_id", "VARCHAR"),
    ("video_id", "VARCHAR"),
    ("viewer_id", "VARCHAR"),
    ("event_name", "VARCHAR"),
    ("sequence", "BIGINT"),
    ("occurred_at_ms", "BIGINT"),
    ("playing_time_ms", "DOUBLE"),
    ("received_at_ms", "BIGINT"),
    ("event_remainder", "VARCHAR"),
    ("context_remainder", "VARCHAR"),
    ("context_format", "INTEGER"),
    ("context_browser_name", "VARCHAR"),
    ("context_os_name", "VARCHAR"),
    ("context_device_type", "VARCHAR"),
];

static COLUMNS: LazyLock<Vec<(String, &'static str)>> = LazyLock::new(|| {
    let mut columns: Vec<_> = BASE
        .iter()
        .map(|(name, kind)| (name.to_string(), *kind))
        .collect();
    for (prefix, fields) in [("e", EVENT_FIELDS), ("b", BROWSER_FIELDS)] {
        columns.extend(fields.iter().map(|(name, kind)| {
            (
                format!("{prefix}_{name}"),
                match kind {
                    Text => "VARCHAR",
                    Number => "DOUBLE",
                    Integer => "BIGINT",
                    Boolean => "BOOLEAN",
                },
            )
        }));
    }
    columns.push(("project_id".into(), "VARCHAR"));
    columns.push(("ingest_key_id".into(), "VARCHAR"));
    columns
});
pub static COLUMN_NAMES: LazyLock<String> = LazyLock::new(|| {
    COLUMNS
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
});
pub static INSERT_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT INTO events ({}) VALUES ({}) ON CONFLICT(project_id, event_id) DO NOTHING",
        *COLUMN_NAMES,
        vec!["?"; COLUMNS.len()].join(", ")
    )
});
pub static BULK_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "INSERT INTO events ({0}) SELECT {0} FROM collector_write_stage ORDER BY ordinal
     ON CONFLICT(project_id, event_id) DO NOTHING RETURNING project_id, event_id",
        *COLUMN_NAMES
    )
});

fn definitions() -> String {
    COLUMNS
        .iter()
        .map(|(name, kind)| format!("{name} {kind}"))
        .collect::<Vec<_>>()
        .join(", ")
}
pub fn create_parquet_stage(connection: &Connection) -> duckdb::Result<()> {
    connection.execute_batch(&format!(
        "CREATE TEMP TABLE collector_parquet_stage (ingest_id BIGINT, {}, otlp_payload VARCHAR)",
        definitions()
    ))
}

pub fn parquet_page_sql(conditions: &str, descending: bool) -> String {
    let direction = if descending { "DESC" } else { "ASC" };
    // Context and OTLP payload live in the same row, so repeated IDs never multiply a join.
    format!("SELECT ingest_id, {0}, otlp_payload FROM collector_parquet_events
        WHERE {conditions} AND ingest_id > ? AND ingest_id <= ? ORDER BY ingest_id {direction} LIMIT ?", *COLUMN_NAMES)
}
pub fn create_stage(connection: &Connection) -> duckdb::Result<()> {
    connection.execute_batch(&format!(
        "CREATE TEMP TABLE collector_write_stage (ordinal BIGINT, {})",
        definitions()
    ))
}
fn create_events(connection: &Connection, table: &str) -> duckdb::Result<()> {
    let definitions = COLUMNS
        .iter()
        .map(|(name, kind)| {
            let required = matches!(
                name.as_str(),
                "event_id"
                    | "project_id"
                    | "view_id"
                    | "video_id"
                    | "event_name"
                    | "sequence"
                    | "occurred_at_ms"
                    | "received_at_ms"
                    | "event_remainder"
                    | "context_format"
            );
            format!("{name} {kind}{}", if required { " NOT NULL" } else { "" })
        })
        .collect::<Vec<_>>()
        .join(", ");
    connection.execute_batch(&format!(
        "CREATE TABLE {table} (
        ingest_id BIGINT PRIMARY KEY DEFAULT nextval('event_order'), {},
        UNIQUE(project_id, event_id), CHECK(context_format IN (0, 1)))",
        definitions
    ))
}

// Keep integer JSON numbers outside the exact DOUBLE range and integral JSON
// floats (including negative zero) in JSON. This preserves their representation
// as well as their value without a per-property type bitmap.
fn scalar(value: &Value, kind: Kind) -> Option<SqlValue> {
    match kind {
        Text => value.as_str().map(|v| SqlValue::Text(v.into())),
        Integer => value.as_i64().map(SqlValue::BigInt),
        Boolean => value.as_bool().map(SqlValue::Boolean),
        Number => {
            let number = value.as_f64()?;
            if !number.is_finite()
                || (value.is_f64() && number.fract() == 0.0)
                || (!value.is_f64() && number.abs() > 9_007_199_254_740_991.0)
            {
                None
            } else {
                Some(SqlValue::Double(number))
            }
        }
    }
}
fn take(object: &mut Map<String, Value>, key: &str, kind: Kind) -> SqlValue {
    match object.get(key).and_then(|value| scalar(value, kind)) {
        Some(value) => {
            object.remove(key);
            value
        }
        None => SqlValue::Null,
    }
}
fn text(value: Option<&Value>) -> SqlValue {
    value
        .and_then(Value::as_str)
        .map_or(SqlValue::Null, |v| SqlValue::Text(v.into()))
}

/// `context` is the actual historical response, not a request to derive it.
/// Keeping a full-context fallback preserves old overrides and absent fields.
pub fn encode(
    event: &Value,
    context: Option<&Value>,
    received: i64,
    project: &str,
    key_id: Option<&str>,
) -> Result<Vec<SqlValue>, ApiError> {
    let mut remainder = event.as_object().ok_or_else(ApiError::internal)?.clone();
    let mut values = Vec::with_capacity(COLUMNS.len());
    for key in ["event_id", "view_id", "video_id", "viewer_id", "event_name"] {
        values.push(take(&mut remainder, key, Text));
    }
    values.push(take(&mut remainder, "sequence", Integer));
    values.push(SqlValue::BigInt(event::timestamp(
        event["occurred_at"]
            .as_str()
            .ok_or_else(ApiError::internal)?,
    )?));
    // Query metrics retain the previous DOUBLE semantics. The original value
    // stays in JSON whenever converting it would lose information.
    values.push(
        event["playing_time_ms"]
            .as_f64()
            .map_or(SqlValue::Null, SqlValue::Double),
    );
    take(&mut remainder, "playing_time_ms", Number);
    values.push(SqlValue::BigInt(received));
    values.push(SqlValue::Null); // event remainder, filled after extraction
    let base = event_context_v1(event, None);
    let (format, stored) = match context.and_then(Value::as_object) {
        Some(actual)
            if base
                .as_ref()
                .and_then(Value::as_object)
                .is_none_or(|b| b.keys().all(|key| actual.contains_key(key))) =>
        {
            let mut delta = actual.clone();
            if let Some(base) = base.as_ref().and_then(Value::as_object) {
                for (key, value) in base {
                    if delta.get(key) == Some(value) {
                        delta.remove(key);
                    }
                }
            }
            (1, Some(Value::Object(delta)))
        }
        _ => (0, context.cloned()),
    };
    values.push(stored.map_or(SqlValue::Null, |v| SqlValue::Text(v.to_string())));
    values.push(SqlValue::Int(format));
    for key in ["browser_name", "os_name", "device_type"] {
        values.push(text(context.and_then(|c| c.get(key))));
    }
    for &(key, kind) in EVENT_FIELDS {
        values.push(take(&mut remainder, key, kind));
    }
    for &(key, kind) in BROWSER_FIELDS {
        values.push(
            remainder
                .get_mut("browser_context")
                .and_then(Value::as_object_mut)
                .map_or(SqlValue::Null, |browser| take(browser, key, kind)),
        );
    }
    // Retain an empty browser object to distinguish {} from a missing property.
    values[9] = SqlValue::Text(Value::Object(remainder).to_string());
    values.push(SqlValue::Text(project.into()));
    values.push(key_id.map_or(SqlValue::Null, |id| SqlValue::Text(id.into())));
    Ok(values)
}

fn put(
    row: &Row<'_>,
    index: usize,
    object: &mut Map<String, Value>,
    key: &str,
    kind: Kind,
) -> duckdb::Result<()> {
    if object.contains_key(key) {
        return Ok(());
    }
    let value = match kind {
        Text => row.get::<_, Option<String>>(index)?.map(Value::String),
        Integer => row.get::<_, Option<i64>>(index)?.map(|v| json!(v)),
        Boolean => row.get::<_, Option<bool>>(index)?.map(Value::Bool),
        Number => row.get::<_, Option<f64>>(index)?.map(|v| {
            if v.fract() == 0.0 && v.abs() <= 9_007_199_254_740_991.0 {
                json!(v as i64)
            } else {
                json!(v)
            }
        }),
    };
    if let Some(value) = value {
        object.insert(key.into(), value);
    }
    Ok(())
}

/// SELECT ingest_id, all storage columns, original OTLP payload, in that order.
pub fn decode(row: &Row<'_>) -> Result<(i64, Value), ApiError> {
    let id = row.get::<_, i64>(0)?;
    let mut event: Value =
        serde_json::from_str(&row.get::<_, String>(10)?).map_err(|_| ApiError::internal())?;
    let object = event.as_object_mut().ok_or_else(ApiError::internal)?;
    for (index, key) in ["event_id", "view_id", "video_id", "viewer_id", "event_name"]
        .iter()
        .enumerate()
    {
        put(row, index + 1, object, key, Text)?;
    }
    put(row, 6, object, "sequence", Integer)?;
    put(row, 8, object, "playing_time_ms", Number)?;
    let mut index = BASE.len() + 1;
    for &(key, kind) in EVENT_FIELDS {
        put(row, index, object, key, kind)?;
        index += 1;
    }
    for &(key, kind) in BROWSER_FIELDS {
        if let Some(browser) = object
            .get_mut("browser_context")
            .and_then(Value::as_object_mut)
        {
            put(row, index, browser, key, kind)?;
        }
        index += 1;
    }
    let stored = row
        .get::<_, Option<String>>(11)?
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()
        .map_err(|_| ApiError::internal())?;
    let context = match row.get::<_, i32>(12)? {
        0 => stored,
        1 => {
            let mut base = event_context_v1(&event, None)
                .and_then(|v| v.as_object().cloned())
                .unwrap_or_default();
            base.extend(
                stored
                    .and_then(|v| v.as_object().cloned())
                    .ok_or_else(ApiError::internal)?,
            );
            Some(Value::Object(base))
        }
        _ => return Err(ApiError::internal()),
    };
    let received: i64 = row.get(9)?;
    let mut record =
        json!({"received_at": chrono::DateTime::from_timestamp_millis(received), "event": event});
    if let Some(context) = context {
        record["event_context"] = context;
    }
    record["project_id"] = json!(row.get::<_, String>(index)?);
    record["ingest_key_id"] = json!(row.get::<_, Option<String>>(index + 1)?);
    if let Some(otlp) = row.get::<_, Option<String>>(index + 2)? {
        record["otlp"] = serde_json::from_str(&otlp).map_err(|_| ApiError::internal())?;
    }
    Ok((id, record))
}

pub fn page_sql(conditions: &str, descending: bool) -> String {
    let direction = if descending { "DESC" } else { "ASC" };
    // Bound the wide rows before joining OTLP, and decode typed values directly
    // instead of serializing all columns to JSON inside DuckDB.
    format!("WITH page AS MATERIALIZED (SELECT ingest_id, {0} FROM events
        WHERE {conditions} AND ingest_id > ? AND ingest_id <= ? ORDER BY ingest_id {direction} LIMIT ?)
        SELECT page.*, event_otlp.payload FROM page LEFT JOIN event_otlp USING(project_id, event_id) ORDER BY page.ingest_id {direction}", *COLUMN_NAMES)
}

pub fn export_sql(conditions: &str, descending: bool) -> String {
    let direction = if descending { "DESC" } else { "ASC" };
    format!("SELECT events.ingest_id, {}, event_otlp.payload FROM events LEFT JOIN event_otlp USING(project_id,event_id)
        WHERE {conditions} AND ingest_id > ? AND ingest_id <= ? ORDER BY ingest_id {direction} LIMIT ?",*COLUMN_NAMES)
}

fn migrate_v4(connection: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    create_events(connection, "events_v4")?;
    let mut after = 0_i64;
    loop {
        let rows = {
            let mut statement = connection.prepare("SELECT ingest_id, received_at_ms, payload, event_context FROM events WHERE ingest_id > ? ORDER BY ingest_id LIMIT 100")?;
            statement
                .query_map([after], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, Option<String>>(3)?,
                    ))
                })?
                .collect::<duckdb::Result<Vec<_>>>()?
        };
        if rows.is_empty() {
            break;
        }
        let mut appender = connection.appender("events_v4")?;
        for (id, received, payload, context) in rows {
            let event = serde_json::from_str(&payload)?;
            let context = context
                .map(|s| serde_json::from_str::<Value>(&s))
                .transpose()?;
            let values = encode(&event, context.as_ref(), received, "default", None)
                .map_err(|error| error.message)?;
            appender.append_row(appender_params_from_iter(
                std::iter::once(SqlValue::BigInt(id)).chain(values),
            ))?;
            after = id;
        }
        appender.flush()?;
    }
    connection.execute_batch("DROP TABLE events; ALTER TABLE events_v4 RENAME TO events; UPDATE collector_schema SET version = 4;")?;
    Ok(())
}

fn migrate_v5(connection: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    create_events(connection, "events_v5")?;
    let has_project: bool = connection.query_row(
        "SELECT count(*) > 0 FROM pragma_table_info('events') WHERE name = 'project_id'",
        [],
        |r| r.get(0),
    )?;
    // Copy by name: version 4 tables predate later storage columns, which stay NULL.
    let selection = if has_project {
        "*"
    } else {
        "*, 'default' AS project_id, CAST(NULL AS VARCHAR) AS ingest_key_id"
    };
    connection.execute_batch(&format!("INSERT INTO events_v5 BY NAME SELECT {selection} FROM events;
        DROP TABLE events; ALTER TABLE events_v5 RENAME TO events;
        CREATE TABLE event_otlp_v5(project_id VARCHAR NOT NULL, event_id VARCHAR NOT NULL, payload VARCHAR NOT NULL, PRIMARY KEY(project_id, event_id));
        INSERT INTO event_otlp_v5 SELECT 'default', event_id, payload FROM event_otlp;
        DROP TABLE event_otlp; ALTER TABLE event_otlp_v5 RENAME TO event_otlp;
        UPDATE collector_schema SET version = 5;"))?;
    Ok(())
}

/// SQL that copies `user_id` and `video_title` from `event_remainder` into
/// their storage version 6 columns. Collectors before version 6 kept these
/// optional properties in the remainder. Only values today's validator accepts
/// are copied, so every column value can also be used as a filter. The
/// remainder stays byte-for-byte as stored; decoding the same value from both
/// places is idempotent. The substring test skips JSON parsing for rows that
/// never mention the field.
pub fn backfill_v6_columns_sql(table: &str) -> String {
    ["user_id", "video_title"]
        .iter()
        .map(|field| {
            format!(
                "UPDATE {table} SET e_{field} = json_extract_string(event_remainder, '$.{field}')
                WHERE e_{field} IS NULL AND contains(event_remainder, '\"{field}\"')
                    AND json_type(event_remainder, '$.{field}') = 'VARCHAR'
                    AND length(json_extract_string(event_remainder, '$.{field}')) BETWEEN 1 AND {};",
                event::MAX_LABEL_CHARS
            )
        })
        .collect()
}

/// Adds `e_user_id` and `e_video_title`, filled from stored remainders.
fn migrate_v6(connection: &Connection) -> Result<(), Box<dyn std::error::Error>> {
    create_events(connection, "events_v6")?;
    connection.execute_batch(&format!(
        "INSERT INTO events_v6 BY NAME SELECT * FROM events;
        DROP TABLE events; ALTER TABLE events_v6 RENAME TO events;
        {}
        UPDATE collector_schema SET version = 6;",
        backfill_v6_columns_sql("events")
    ))?;
    Ok(())
}

pub fn initialize(connection: &mut Connection) -> Result<(), Box<dyn std::error::Error>> {
    let tx = connection.transaction()?;
    tx.execute_batch("CREATE TABLE IF NOT EXISTS collector_schema (version INTEGER NOT NULL);
            INSERT INTO collector_schema SELECT 1 WHERE NOT EXISTS (SELECT 1 FROM collector_schema);")?;
    let version: i32 = tx.query_row("SELECT version FROM collector_schema", [], |r| r.get(0))?;
    if !(1..=6).contains(&version) {
        return Err("Unsupported database schema version".into());
    }
    tx.execute_batch(
        "CREATE SEQUENCE IF NOT EXISTS event_order START 1;
            CREATE TABLE IF NOT EXISTS events (
                ingest_id BIGINT PRIMARY KEY DEFAULT nextval('event_order'),
                event_id VARCHAR UNIQUE NOT NULL,
                view_id VARCHAR NOT NULL,
                video_id VARCHAR NOT NULL,
                viewer_id VARCHAR,
                event_name VARCHAR NOT NULL,
                sequence BIGINT NOT NULL,
                occurred_at_ms BIGINT NOT NULL,
                playing_time_ms DOUBLE,
                received_at_ms BIGINT NOT NULL,
                payload VARCHAR NOT NULL
            );
            CREATE TABLE IF NOT EXISTS event_otlp (
                event_id VARCHAR PRIMARY KEY,
                payload VARCHAR NOT NULL
            );",
    )?;
    if version < 3 {
        // ADD COLUMN fails during WAL replay in the pinned DuckDB version.
        // Rebuild atomically using CREATE/RENAME, preserving IDs and sequence.
        tx.execute_batch(
            "CREATE TABLE events_v3 (
                    ingest_id BIGINT PRIMARY KEY DEFAULT nextval('event_order'),
                    event_id VARCHAR UNIQUE NOT NULL,
                    view_id VARCHAR NOT NULL,
                    video_id VARCHAR NOT NULL,
                    viewer_id VARCHAR,
                    event_name VARCHAR NOT NULL,
                    sequence BIGINT NOT NULL,
                    occurred_at_ms BIGINT NOT NULL,
                    playing_time_ms DOUBLE,
                    received_at_ms BIGINT NOT NULL,
                    payload VARCHAR NOT NULL,
                    event_context VARCHAR
                );
                INSERT INTO events_v3 SELECT *, NULL FROM events;
                DROP TABLE events;
                ALTER TABLE events_v3 RENAME TO events;
                UPDATE collector_schema SET version = 3;",
        )?;
        // Backfill only browser context already present in the raw payload.
        // Bound migration memory; old requests never receive an inferred IP.
        let mut after = 0_i64;
        loop {
            let rows = {
                let mut statement = tx.prepare("SELECT ingest_id, payload FROM events WHERE ingest_id > ? ORDER BY ingest_id LIMIT 100")?;
                statement
                    .query_map([after], |row| {
                        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                    })?
                    .collect::<duckdb::Result<Vec<_>>>()?
            };
            if rows.is_empty() {
                break;
            }
            for (id, payload) in rows {
                after = id;
                let event: Value = serde_json::from_str(&payload)?;
                if let Some(context) = event_context_v1(&event, None) {
                    tx.execute(
                        "UPDATE events SET event_context = ? WHERE ingest_id = ?",
                        duckdb::params![context.to_string(), id],
                    )?;
                }
            }
        }
    }
    if version < 4 {
        migrate_v4(&tx)?;
    }
    if version < 5 {
        migrate_v5(&tx)?;
    }
    if version < 6 {
        migrate_v6(&tx)?;
    }
    tx.commit()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::{params, params_from_iter};

    fn event(id: usize) -> Value {
        json!({"event_id": format!("event-{id}"), "view_id": "view", "video_id": "video",
            "viewer_id": null, "event_name": "chunkify.video.view_started", "sequence": id,
            "occurred_at": "2026-10-19T12:00:00.123456+02:00", "schema_version": 1,
            "playing_time_ms": 123, "startup_time_ms": 1.25,
            "browser_context": {"schema_version": 1, "browser_name": "Firefox", "pixel_ratio": 1.5}})
    }
    fn read(connection: &Connection) -> Vec<(i64, Value)> {
        let mut statement = connection.prepare(&page_sql("TRUE", false)).unwrap();
        let mut rows = statement.query(params![0_i64, i64::MAX, 1000_i64]).unwrap();
        let mut result = Vec::new();
        while let Some(row) = rows.next().unwrap() {
            result.push(decode(row).map_err(|e| e.message).unwrap());
        }
        result
    }
    #[test]
    fn typed_storage_round_trips_unusual_values_and_historical_context_after_checkpoint() {
        let path =
            std::env::temp_dir().join(format!("collector-storage-{}.duckdb", uuid::Uuid::new_v4()));
        let mut connection = Connection::open(&path).unwrap();
        initialize(&mut connection).unwrap();
        let mut cases = Vec::new();
        for browser in [
            None,
            Some(json!({})),
            Some(Value::Null),
            Some(json!([1, 2])),
            Some(
                json!({"schema_version": 1, "browser_name": 42, "os_name": null,
                "device_type": {"custom": true}, "screen_width_px": "wide", "extra": [null, "é"]}),
            ),
            Some(json!({"schema_version": 1, "browser_name": "Firefox", "os_name": "Linux"})),
        ] {
            for number in [
                json!(0),
                json!(42),
                json!(1.25),
                json!(-0.0),
                json!(1.0),
                json!(9_007_199_254_740_993_u64),
                json!(u64::MAX),
                json!(i64::MIN),
                json!(1e100),
            ] {
                let mut e = event(cases.len() + 1);
                if let Some(browser) = browser.clone() {
                    e["browser_context"] = browser;
                } else {
                    e.as_object_mut().unwrap().remove("browser_context");
                }
                e["playing_time_ms"] = number.clone();
                e["seek_duration_ms"] = number;
                e["selected_rendition_codec"] = Value::Null;
                e["error_code"] = json!(18446744073709551615_u64);
                e["custom"] = json!({"array": [false, null, {"text": "café 🎥"}]});
                let context = match cases.len() % 5 {
                    0 => None,
                    1 => Some(json!({})),
                    2 => Some(json!({"schema_version": 1, "browser_name": "historical override"})),
                    _ => event_context_v1(
                        &e,
                        Some(&json!({"country": "GB", "city": "London", "schema_version": 1})),
                    ),
                };
                let values = encode(&e, context.as_ref(), 123456789, "default", None)
                    .map_err(|e| e.message)
                    .unwrap();
                connection
                    .execute(&INSERT_SQL, params_from_iter(&values))
                    .unwrap();
                let otlp = json!({"log_record": {"body": "original", "attributes": [{"key": "null", "value": {}}]}, "resource_logs": {"custom": [1, null]}});
                connection
                    .execute(
                        "INSERT INTO event_otlp VALUES ('default', ?, ?)",
                        params![e["event_id"].as_str().unwrap(), otlp.to_string()],
                    )
                    .unwrap();
                let mut expected = json!({"project_id": "default", "ingest_key_id": null, "event": e, "received_at": chrono::DateTime::from_timestamp_millis(123456789), "otlp": otlp});
                if let Some(context) = context {
                    expected["event_context"] = context;
                }
                cases.push(expected);
            }
        }
        connection.execute_batch("CHECKPOINT").unwrap();
        drop(connection);
        let mut connection = Connection::open(&path).unwrap();
        initialize(&mut connection).unwrap();
        assert_eq!(
            read(&connection)
                .into_iter()
                .map(|(_, v)| v)
                .collect::<Vec<_>>(),
            cases
        );
        drop(connection);
        std::fs::remove_file(path).unwrap();
    }

    fn legacy(connection: &Connection, version: i32) {
        connection.execute_batch(&format!("CREATE TABLE collector_schema(version INTEGER); INSERT INTO collector_schema VALUES ({version});
            CREATE SEQUENCE event_order START 10;
            CREATE TABLE events(ingest_id BIGINT PRIMARY KEY DEFAULT nextval('event_order'),
                event_id VARCHAR UNIQUE NOT NULL, view_id VARCHAR NOT NULL, video_id VARCHAR NOT NULL,
                viewer_id VARCHAR, event_name VARCHAR NOT NULL, sequence BIGINT NOT NULL,
                occurred_at_ms BIGINT NOT NULL, playing_time_ms DOUBLE, received_at_ms BIGINT NOT NULL, payload VARCHAR NOT NULL{});
            CREATE TABLE event_otlp(event_id VARCHAR PRIMARY KEY, payload VARCHAR NOT NULL);",
            if version == 3 {", event_context VARCHAR"} else {""})).unwrap();
    }
    #[test]
    fn migrations_preserve_ids_receipts_otlp_context_and_sequence() {
        for version in 1..=3 {
            let mut connection = Connection::open_in_memory().unwrap();
            legacy(&connection, version);
            let e = event(1);
            let context =
                event_context_v1(&e, Some(&json!({"country": "GB", "city": "London"}))).unwrap();
            connection.execute(&format!("INSERT INTO events VALUES(3, 'event-1', 'view', 'video', NULL, 'chunkify.video.view_started', 1, ?, 123, 123456789, ?{})", if version == 3 {", ?"} else {""}),
                params_from_iter([SqlValue::BigInt(event::timestamp(e["occurred_at"].as_str().unwrap()).map_err(|e| e.message).unwrap()), SqlValue::Text(e.to_string())].into_iter()
                    .chain((version == 3).then(|| SqlValue::Text(context.to_string()))))).unwrap();
            connection
                .execute(
                    "INSERT INTO event_otlp VALUES('event-1', ?)",
                    [json!({"original": true}).to_string()],
                )
                .unwrap();
            initialize(&mut connection).unwrap();
            let records = read(&connection);
            assert_eq!(records[0].0, 3);
            assert_eq!(records[0].1["event"], e);
            assert_eq!(
                records[0].1["event_context"],
                if version == 3 {
                    context
                } else {
                    event_context_v1(&e, None).unwrap()
                }
            );
            assert_eq!(records[0].1["otlp"], json!({"original": true}));
            assert_eq!(
                records[0].1["received_at"],
                json!(chrono::DateTime::from_timestamp_millis(123456789))
            );
            let next = event(2);
            connection
                .execute(
                    &INSERT_SQL,
                    params_from_iter(
                        encode(&next, None, 123456790, "default", None)
                            .map_err(|e| e.message)
                            .unwrap(),
                    ),
                )
                .unwrap();
            assert_eq!(read(&connection)[1].0, 10);
            initialize(&mut connection).unwrap();
            assert_eq!(read(&connection)[0], records[0]);
        }
    }
    #[test]
    fn version_6_copies_stored_user_and_title_into_columns() {
        let mut connection = Connection::open_in_memory().unwrap();
        let new_columns = ["e_user_id", "e_video_title"];
        let old: Vec<_> = COLUMNS
            .iter()
            .filter(|(name, _)| !new_columns.contains(&name.as_str()))
            .collect();
        let definitions = old
            .iter()
            .map(|(name, kind)| format!("{name} {kind}"))
            .collect::<Vec<_>>()
            .join(", ");
        connection.execute_batch(&format!("CREATE TABLE collector_schema(version INTEGER); INSERT INTO collector_schema VALUES (5);
            CREATE SEQUENCE event_order START 10;
            CREATE TABLE events(ingest_id BIGINT PRIMARY KEY DEFAULT nextval('event_order'), {definitions}, UNIQUE(project_id, event_id));
            CREATE TABLE event_otlp(project_id VARCHAR NOT NULL, event_id VARCHAR NOT NULL, payload VARCHAR NOT NULL, PRIMARY KEY(project_id, event_id));")).unwrap();
        // A version 5 collector kept these optional properties in the remainder.
        let mut stored = event(1);
        let values = encode(&stored, None, 123, "default", None)
            .map_err(|e| e.message)
            .unwrap();
        let values: Vec<_> = COLUMNS
            .iter()
            .zip(values)
            .filter(|((name, _), _)| !new_columns.contains(&name.as_str()))
            .map(|(_, value)| value)
            .collect();
        let names = old
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>();
        connection
            .execute(
                &format!(
                    "INSERT INTO events ({}) VALUES ({})",
                    names.join(", "),
                    vec!["?"; names.len()].join(", ")
                ),
                params_from_iter(values),
            )
            .unwrap();
        stored["user_id"] = json!("user-1");
        stored["video_title"] = json!("Pilot");
        let remainder: String = connection
            .query_row("SELECT event_remainder FROM events", [], |r| r.get(0))
            .unwrap();
        let mut remainder: Value = serde_json::from_str(&remainder).unwrap();
        remainder["user_id"] = json!("user-1");
        remainder["video_title"] = json!("Pilot");
        connection
            .execute(
                "UPDATE events SET event_remainder = ?",
                [remainder.to_string()],
            )
            .unwrap();

        initialize(&mut connection).unwrap();
        let version: i32 = connection
            .query_row("SELECT version FROM collector_schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, 6);
        let columns: (Option<String>, Option<String>) = connection
            .query_row("SELECT e_user_id, e_video_title FROM events", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(columns, (Some("user-1".into()), Some("Pilot".into())));
        let records = read(&connection);
        assert_eq!(records[0].0, 10);
        assert_eq!(records[0].1["event"], stored);
        // New writes use the version 6 layout and keep the sequence.
        let mut next = event(2);
        next["user_id"] = json!("user-2");
        connection
            .execute(
                &INSERT_SQL,
                params_from_iter(
                    encode(&next, None, 124, "default", None)
                        .map_err(|e| e.message)
                        .unwrap(),
                ),
            )
            .unwrap();
        let user: String = connection
            .query_row(
                "SELECT e_user_id FROM events WHERE ingest_id = 11",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(user, "user-2");
        assert_eq!(read(&connection)[1].1["event"], next);
    }
    #[test]
    fn migration_crosses_page_boundaries_and_preserves_context_absence() {
        let mut connection = Connection::open_in_memory().unwrap();
        legacy(&connection, 3);
        let mut expected = Vec::new();
        for index in 1..=205 {
            let event = event(index);
            let context = if index % 2 == 0 {
                event_context_v1(&event, None)
            } else {
                None
            };
            connection.execute("INSERT INTO events(event_id, view_id, video_id, viewer_id, event_name, sequence, occurred_at_ms, playing_time_ms, received_at_ms, payload, event_context) VALUES (?, 'view', 'video', NULL, 'chunkify.video.view_started', ?, ?, 123, 123456789, ?, ?)",
                params![event["event_id"].as_str().unwrap(), index as i64,
                    event::timestamp(event["occurred_at"].as_str().unwrap()).map_err(|e| e.message).unwrap(),
                    event.to_string(), context.as_ref().map(Value::to_string)]).unwrap();
            expected.push((event, context));
        }
        initialize(&mut connection).unwrap();
        let records = read(&connection);
        assert_eq!(records.len(), expected.len());
        for (index, ((id, record), (event, context))) in records.iter().zip(expected).enumerate() {
            assert_eq!(*id, 10 + index as i64);
            assert_eq!(record["event"], event);
            assert_eq!(record.get("event_context"), context.as_ref());
        }
        assert_eq!(
            connection
                .query_row("SELECT nextval('event_order')", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            215
        );
    }

    #[test]
    fn failed_migration_rolls_back_and_future_versions_are_untouched() {
        let mut connection = Connection::open_in_memory().unwrap();
        legacy(&connection, 3);
        for id in 1..=150 {
            let event = event(id);
            connection.execute("INSERT INTO events VALUES (?, ?, 'view', 'video', NULL, 'chunkify.video.view_started', ?, 0, 123, 0, ?, NULL)",
                params![id as i64, event["event_id"].as_str().unwrap(), id as i64, event.to_string()]).unwrap();
        }
        connection.execute_batch("INSERT INTO events VALUES (151, 'broken', 'view', 'video', NULL, 'event', 1, 0, 0, 0, 'invalid json', NULL)").unwrap();
        assert!(initialize(&mut connection).is_err());
        assert_eq!(
            connection
                .query_row("SELECT version FROM collector_schema", [], |r| r
                    .get::<_, i32>(0))
                .unwrap(),
            3
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT payload FROM events WHERE event_id = 'broken'",
                    [],
                    |r| r.get::<_, String>(0)
                )
                .unwrap(),
            "invalid json"
        );
        assert_eq!(
            connection
                .query_row("SELECT COUNT(*) FROM events", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            151
        );
        assert_eq!(
            connection
                .query_row(
                    "SELECT COUNT(*) FROM information_schema.tables WHERE table_name = 'events_v4'",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
        connection
            .execute_batch("UPDATE collector_schema SET version = 99")
            .unwrap();
        assert!(initialize(&mut connection).is_err());
        assert_eq!(
            connection
                .query_row("SELECT version FROM collector_schema", [], |r| r
                    .get::<_, i32>(0))
                .unwrap(),
            99
        );
    }
}
