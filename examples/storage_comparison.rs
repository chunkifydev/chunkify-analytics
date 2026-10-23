//! Isolated comparison of experimental and production layouts in temporary databases.
#![allow(dead_code)]
use duckdb::{Connection, appender_params_from_iter, params, types::Value as SqlValue};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, HashSet},
    error::Error,
    fs::{self, File},
    io::{BufRead, BufReader},
    path::Path,
    time::Instant,
};

// Reuse the actual validator and context derivation, without starting HTTP workers.
mod api {
    #[derive(Debug)]
    pub struct ApiError(pub (), pub String);
    impl From<duckdb::Error> for ApiError {
        fn from(e: duckdb::Error) -> Self {
            Self((), e.to_string())
        }
    }
    impl ApiError {
        pub fn internal() -> Self {
            Self((), "Storage error".into())
        }
        pub fn bad(message: impl Into<String>) -> Self {
            Self((), message.into())
        }
    }
}
#[path = "../src/context.rs"]
mod context;
#[path = "../src/event.rs"]
mod event;
#[path = "../src/storage.rs"]
mod storage;

type Result<T> = std::result::Result<T, Box<dyn Error>>;
const COMMON: [&str; 7] = [
    "event_id",
    "view_id",
    "video_id",
    "viewer_id",
    "event_name",
    "sequence",
    "playing_time_ms",
];
const BASE_COLUMNS: &str = "event_id, view_id, video_id, viewer_id, event_name, sequence, occurred_at_ms, playing_time_ms, received_at_ms, payload, event_context";
const BASE_DDL: &str = "event_id VARCHAR UNIQUE NOT NULL, view_id VARCHAR NOT NULL, video_id VARCHAR NOT NULL, viewer_id VARCHAR, event_name VARCHAR NOT NULL, sequence BIGINT NOT NULL, occurred_at_ms BIGINT NOT NULL, playing_time_ms DOUBLE, received_at_ms BIGINT NOT NULL, payload VARCHAR NOT NULL, event_context VARCHAR";

#[derive(Deserialize)]
struct Record {
    event: Value,
    otlp: Option<Value>,
    request_context: Value,
    received_at_ms: i64,
}
#[derive(Clone, Copy, PartialEq, Serialize, Debug)]
enum Kind {
    Text,
    Integer,
    Double,
    Boolean,
    Json,
}
impl Kind {
    fn sql(self) -> &'static str {
        match self {
            Self::Text => "VARCHAR",
            Self::Integer => "BIGINT",
            Self::Double => "DOUBLE",
            Self::Boolean => "BOOLEAN",
            Self::Json => unreachable!(),
        }
    }
}
#[derive(Serialize)]
struct Field {
    path: Vec<String>,
    kind: Kind,
}

fn records(path: &str) -> Result<impl Iterator<Item = Result<Record>>> {
    Ok(BufReader::new(File::open(path)?)
        .lines()
        .map(|line| Ok(serde_json::from_str(&line?)?)))
}
fn kind(v: &Value) -> Option<Kind> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(_) => Kind::Boolean,
        Value::String(_) => Kind::Text,
        Value::Number(n) if n.as_i64().is_some() => Kind::Integer,
        Value::Number(_) => Kind::Double,
        _ => Kind::Json,
    })
}
fn discover(fixture: &str) -> Result<Vec<Field>> {
    let mut found = BTreeMap::<Vec<String>, Kind>::new();
    for record in records(fixture)? {
        let record = record?;
        event::validate(&record.event).map_err(|e| e.1)?;
        let mut observe = |path: Vec<String>, value: &Value| {
            if let Some(new) = kind(value) {
                found
                    .entry(path)
                    .and_modify(|old| {
                        if *old != new {
                            *old = if matches!(
                                (*old, new),
                                (Kind::Integer, Kind::Double) | (Kind::Double, Kind::Integer)
                            ) {
                                Kind::Double
                            } else {
                                Kind::Json
                            };
                        }
                    })
                    .or_insert(new);
            }
        };
        for (key, value) in record.event.as_object().ok_or("Expected event object")? {
            if COMMON.contains(&key.as_str()) {
                continue;
            }
            if key == "browser_context" && value.is_object() {
                for (key, value) in value.as_object().unwrap() {
                    observe(vec!["browser_context".into(), key.clone()], value);
                }
            } else {
                observe(vec![key.clone()], value);
            }
        }
    }
    Ok(found
        .into_iter()
        .filter(|(_, k)| *k != Kind::Json)
        .map(|(path, kind)| Field { path, kind })
        .collect())
}
fn take(value: &mut Value, field: &Field) -> SqlValue {
    let object = if field.path.len() == 1 {
        value.as_object_mut()
    } else {
        value.get_mut(&field.path[0]).and_then(Value::as_object_mut)
    };
    let Some(object) = object else {
        return SqlValue::Null;
    };
    let key = field.path.last().unwrap();
    let Some(v) = object.get(key) else {
        return SqlValue::Null;
    };
    let extracted = match field.kind {
        Kind::Text => v.as_str().map(|v| SqlValue::Text(v.into())),
        Kind::Boolean => v.as_bool().map(SqlValue::Boolean),
        Kind::Integer => v.as_i64().map(SqlValue::BigInt),
        // Preserve integers outside the exactly representable double range in
        // the JSON remainder rather than silently rounding an unknown property.
        Kind::Double if v.is_i64() && v.as_i64().unwrap().unsigned_abs() > (1_u64 << 53) => None,
        Kind::Double if v.is_u64() && v.as_u64().unwrap() > (1_u64 << 53) => None,
        Kind::Double => v.as_f64().map(SqlValue::Double),
        Kind::Json => None,
    };
    if let Some(extracted) = extracted {
        object.remove(key);
        extracted
    } else {
        SqlValue::Null
    }
}
fn set(value: &mut Value, field: &Field, v: Value) {
    if v.is_null() {
        return;
    }
    let object = if field.path.len() == 1 {
        value.as_object_mut().unwrap()
    } else {
        value[&field.path[0]]
            .as_object_mut()
            .expect("nested presence preserved in remainder")
    };
    object.insert(field.path.last().unwrap().clone(), v);
}
fn text(v: &Value) -> SqlValue {
    v.as_str()
        .map_or(SqlValue::Null, |v| SqlValue::Text(v.into()))
}
fn prepare(record: &Record, variant: &str, fields: &[Field]) -> Result<Vec<SqlValue>> {
    let e = &record.event;
    let ctx = if variant == "typed_context" {
        Some(record.request_context.clone())
    } else {
        context::event_context(e, Some(&record.request_context))
    };
    let mut values = vec![
        text(&e["event_id"]),
        text(&e["view_id"]),
        text(&e["video_id"]),
        text(&e["viewer_id"]),
        text(&e["event_name"]),
        SqlValue::BigInt(e["sequence"].as_i64().ok_or("sequence")?),
        SqlValue::BigInt(
            event::timestamp(e["occurred_at"].as_str().ok_or("timestamp")?).map_err(|e| e.1)?,
        ),
        e["playing_time_ms"]
            .as_f64()
            .map_or(SqlValue::Null, SqlValue::Double),
        SqlValue::BigInt(record.received_at_ms),
    ];
    let mut payload = e.clone();
    let mut scalars = Vec::new();
    if variant != "current" {
        for key in COMMON {
            if payload.get(key).is_some_and(|v| !v.is_null()) {
                payload.as_object_mut().unwrap().remove(key);
            }
        }
        for field in fields {
            scalars.push(take(&mut payload, field));
        }
    }
    values.push(SqlValue::Text(payload.to_string()));
    values.push(ctx.map_or(SqlValue::Null, |v| SqlValue::Text(v.to_string())));
    values.extend(scalars);
    Ok(values)
}
fn equivalent(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => {
            x == y || {
                // Only equate integer/float encodings when the integer is exactly
                // representable. No tolerance is used for floating point values.
                let safe = |n: &serde_json::Number| {
                    n.as_i64().is_none_or(|i| i.unsigned_abs() <= (1_u64 << 53))
                        && n.as_u64().is_none_or(|i| i <= (1_u64 << 53))
                };
                safe(x) && safe(y) && x.as_f64() == y.as_f64()
            }
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).is_some_and(|other| equivalent(v, other)))
        }
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(a, b)| equivalent(a, b))
        }
        _ => a == b,
    }
}
fn select(fields: &[Field]) -> String {
    let common = if fields.is_empty() {
        "'{}'"
    } else {
        "CAST(to_json(struct_pack(event_id := e.event_id, view_id := e.view_id, video_id := e.video_id,
        viewer_id := e.viewer_id, event_name := e.event_name, sequence := e.sequence, playing_time_ms := e.playing_time_ms)) AS VARCHAR)"
    };
    let scalar = if fields.is_empty() {
        "'{}'".into()
    } else {
        format!(
            "CAST(to_json(struct_pack({})) AS VARCHAR)",
            fields
                .iter()
                .enumerate()
                .map(|(i, _)| format!("c{i} := c{i}"))
                .collect::<Vec<_>>()
                .join(",")
        )
    };
    format!("SELECT e.ingest_id, e.payload, e.event_context, o.payload, e.received_at_ms, {common}, {scalar}
        FROM events e LEFT JOIN event_otlp o USING(event_id)
        WHERE e.ingest_id > ? ORDER BY e.ingest_id LIMIT ?")
}
fn read_page(
    connection: &Connection,
    sql: &str,
    after: i64,
    limit: i64,
    variant: &str,
    fields: &[Field],
) -> Result<Vec<(i64, Value)>> {
    if variant == "production" {
        let mut statement = connection.prepare(&storage::page_sql("TRUE", false))?;
        let mut rows = statement.query(params![after, i64::MAX, limit])?;
        let mut result = Vec::new();
        while let Some(row) = rows.next()? {
            let (id, record) = storage::decode(row).map_err(|e| e.1)?;
            let received = chrono::DateTime::parse_from_rfc3339(
                record["received_at"].as_str().ok_or("missing receipt")?,
            )?
            .timestamp_millis();
            result.push((id, json!({"event": record["event"], "event_context": record["event_context"], "otlp": record["otlp"], "received_at_ms": received})));
        }
        return Ok(result);
    }
    let mut statement = connection.prepare(sql)?;
    let mut rows = statement.query(params![after, limit])?;
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let mut e: Value = serde_json::from_str(&row.get::<_, String>(1)?)?;
        let stored_context = row
            .get::<_, Option<String>>(2)?
            .map(|s| serde_json::from_str::<Value>(&s))
            .transpose()?;
        let otlp = row
            .get::<_, Option<String>>(3)?
            .map(|s| serde_json::from_str::<Value>(&s))
            .transpose()?;
        if variant != "current" {
            let common: Value = serde_json::from_str(&row.get::<_, String>(5)?)?;
            for key in COMMON {
                if !common[key].is_null() {
                    e.as_object_mut()
                        .unwrap()
                        .insert(key.into(), common[key].clone());
                }
            }
            let scalar: Value = serde_json::from_str(&row.get::<_, String>(6)?)?;
            for (i, field) in fields.iter().enumerate() {
                set(&mut e, field, scalar[format!("c{i}")].clone());
            }
        }
        let ctx = if variant == "typed_context" {
            context::event_context(&e, stored_context.as_ref())
        } else {
            stored_context
        };
        result.push((id, json!({"event": e, "event_context": ctx, "otlp": otlp, "received_at_ms": row.get::<_, i64>(4)?})));
    }
    Ok(result)
}
fn measure(mut f: impl FnMut() -> Result<Value>) -> Result<Value> {
    let expected = f()?;
    let mut samples = Vec::new();
    for _ in 0..7 {
        let t = Instant::now();
        let result = f()?;
        samples.push(t.elapsed().as_secs_f64() * 1000.0);
        if !equivalent(&result, &expected) {
            return Err("query result changed".into());
        }
    }
    samples.sort_by(f64::total_cmp);
    Ok(
        json!({"median_ms": samples[3], "max_ms": samples[6], "samples_ms": samples, "result": expected}),
    )
}
fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 5 {
        return Err("Usage: storage_comparison FIXTURE NEW_DB current|typed|typed_context|production native|mixed BATCH".into());
    }
    let (fixture, path, variant, format) = (&args[0], &args[1], &args[2], &args[3]);
    if Path::new(path).exists() {
        return Err("Refusing to replace an existing database".into());
    }
    if !["current", "typed", "typed_context", "production"].contains(&variant.as_str())
        || !["native", "mixed"].contains(&format.as_str())
    {
        return Err("Invalid variant/format".into());
    }
    let batch_size: usize = args[4].parse()?;
    if !(1..=10000).contains(&batch_size) {
        return Err("Invalid batch size".into());
    }
    let fields = if variant == "current" || variant == "production" {
        Vec::new()
    } else {
        discover(fixture)?
    };
    let mut connection = Connection::open(path)?;
    connection.execute_batch(
        "SET threads=2; SET memory_limit='512 MiB'; SET checkpoint_threshold='16 MiB'",
    )?;
    let engine: String = connection.query_row("SELECT version()", [], |r| r.get(0))?;
    let suffix = fields
        .iter()
        .enumerate()
        .map(|(i, f)| format!(", c{i} {}", f.kind.sql()))
        .collect::<String>();
    let columns = if variant == "production" {
        storage::COLUMN_NAMES.to_string()
    } else {
        format!(
            "{BASE_COLUMNS}{}",
            (0..fields.len())
                .map(|i| format!(", c{i}"))
                .collect::<String>()
        )
    };
    // Same v3 base schema, dedup indexes and OTLP table as the collector. The
    // scalar columns and JSON remainder are the experimental difference.
    if variant == "production" {
        storage::initialize(&mut connection)?;
        connection.execute_batch(&format!("CREATE TEMP TABLE stage AS SELECT 0::BIGINT AS ordinal, {columns} FROM events WHERE false"))?;
    } else {
        connection.execute_batch(&format!("CREATE TABLE collector_schema(version INTEGER NOT NULL); INSERT INTO collector_schema VALUES(3);
        CREATE SEQUENCE event_order START 1;
        CREATE TABLE events(ingest_id BIGINT PRIMARY KEY DEFAULT nextval('event_order'), {BASE_DDL}{suffix});
        CREATE TABLE event_otlp(event_id VARCHAR PRIMARY KEY, payload VARCHAR NOT NULL);
        CREATE TEMP TABLE stage AS SELECT 0::BIGINT AS ordinal, {columns} FROM events WHERE false"))?;
    }
    let start = Instant::now();
    let mut input = records(fixture)?;
    let mut count = 0usize;
    let mut otlp_count = 0usize;
    let mut insert_batches_ms = Vec::new();
    loop {
        let batch: Vec<Record> = input.by_ref().take(batch_size).collect::<Result<_>>()?;
        if batch.is_empty() {
            break;
        }
        let t = Instant::now();
        let prepared = batch
            .iter()
            .map(|r| {
                if variant == "production" {
                    storage::encode(
                        &r.event,
                        context::event_context(&r.event, Some(&r.request_context)).as_ref(),
                        r.received_at_ms,
                        "default",
                        None,
                    )
                    .map_err(|e| e.1.into())
                } else {
                    prepare(r, variant, &fields)
                }
            })
            .collect::<Result<Vec<_>>>()?;
        let tx = connection.transaction()?;
        {
            let mut appender = tx.appender_to_catalog_and_db("stage", "temp", "main")?;
            for (ordinal, row) in prepared.iter().enumerate() {
                let ordinal = SqlValue::BigInt(ordinal as i64);
                appender.append_row(appender_params_from_iter(
                    std::iter::once(&ordinal).chain(row.iter()),
                ))?;
            }
            appender.flush()?;
        }
        let conflict = if variant == "production" {
            "project_id, event_id"
        } else {
            "event_id"
        };
        let inserted: HashSet<String> = tx.prepare(&format!("INSERT INTO events({columns}) SELECT {columns} FROM stage ORDER BY ordinal ON CONFLICT({conflict}) DO NOTHING RETURNING event_id"))?
            .query_map([], |r| r.get(0))?.collect::<duckdb::Result<_>>()?;
        if inserted.len() != batch.len() {
            return Err("Unexpected duplicate/missing input event".into());
        }
        if format == "mixed" {
            let mut appender = tx.appender("event_otlp")?;
            for r in &batch {
                if let Some(otlp) = &r.otlp {
                    if variant == "production" {
                        appender.append_row(params![
                            "default",
                            r.event["event_id"].as_str().unwrap(),
                            otlp.to_string()
                        ])?;
                    } else {
                        appender.append_row(params![
                            r.event["event_id"].as_str().unwrap(),
                            otlp.to_string()
                        ])?;
                    }
                    otlp_count += 1;
                }
            }
            appender.flush()?;
        }
        tx.execute("DELETE FROM stage", [])?;
        tx.commit()?;
        count += batch.len();
        insert_batches_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let insert_seconds = start.elapsed().as_secs_f64();
    let checkpoint_start = Instant::now();
    connection.execute_batch("CHECKPOINT")?;
    let checkpoint_seconds = checkpoint_start.elapsed().as_secs_f64();
    drop(connection);
    let bytes = fs::metadata(path)?.len();
    let connection = Connection::open(path)?;
    connection.execute_batch("SET threads=2; SET memory_limit='512 MiB'")?;
    let sql = select(&fields);
    let verify_start = Instant::now();
    let mut expected = records(fixture)?;
    let mut after = 0;
    let mut verified = 0;
    loop {
        let page = read_page(&connection, &sql, after, 1000, variant, &fields)?;
        if page.is_empty() {
            break;
        }
        for (id, actual) in page {
            let r = expected.next().ok_or("Unexpected extra stored event")??;
            let wanted = json!({"event_context": context::event_context(&r.event, Some(&r.request_context)),
                "event": r.event, "otlp": if format == "mixed" { r.otlp } else { None }, "received_at_ms": r.received_at_ms});
            if !equivalent(&actual, &wanted) {
                return Err(format!(
                    "Round-trip mismatch at event {id}: actual={actual}, expected={wanted}"
                )
                .into());
            }
            after = id;
            verified += 1;
        }
    }
    if expected.next().is_some() || verified != count {
        return Err("Missing stored records".into());
    }
    let verify_seconds = verify_start.elapsed().as_secs_f64();
    let summary_sql = "WITH started AS (SELECT view_id,video_id,viewer_id FROM events WHERE event_name='chunkify.video.view_started'),
        latest AS (SELECT view_id,video_id,viewer_id,playing_time_ms FROM events
        WHERE event_name IN ('chunkify.video.view_ended','chunkify.video.view_snapshot')
        QUALIFY ROW_NUMBER() OVER(PARTITION BY view_id,video_id,viewer_id ORDER BY (event_name='chunkify.video.view_ended') DESC, sequence DESC, occurred_at_ms DESC, ingest_id DESC)=1)
        SELECT COUNT(*),COUNT(DISTINCT s.viewer_id),COALESCE(SUM(l.playing_time_ms),0)
        FROM started s LEFT JOIN latest l ON s.view_id=l.view_id AND s.video_id=l.video_id AND s.viewer_id IS NOT DISTINCT FROM l.viewer_id";
    let summary = measure(|| {
        Ok(connection.query_row(summary_sql, [], |r| Ok(json!({"views": r.get::<_, i64>(0)?, "unique_viewers": r.get::<_, i64>(1)?, "playing_time_ms": r.get::<_, f64>(2)?})))?)
    })?;
    let qoe_expression = if variant == "production" {
        "COALESCE(CAST(json_extract(event_remainder,'$.rebuffer_duration_ms') AS DOUBLE), e_rebuffer_duration_ms)".into()
    } else if let Some(i) = fields
        .iter()
        .position(|f| f.path == ["rebuffer_duration_ms"])
    {
        format!("c{i}")
    } else {
        "CAST(json_extract(payload,'$.rebuffer_duration_ms') AS DOUBLE)".into()
    };
    let qoe = measure(|| {
        Ok(connection.query_row(&format!("SELECT COUNT(*), COALESCE(SUM({qoe_expression}),0) FROM events WHERE event_name='chunkify.video.view_ended'"), [], |r| Ok(json!({"views":r.get::<_,i64>(0)?, "rebuffer_duration_ms":r.get::<_,f64>(1)?})))?)
    })?;
    let event_page = measure(|| {
        let page = read_page(&connection, &sql, (count / 2) as i64, 100, variant, &fields)?;
        let encoded = serde_json::to_vec(&page)?;
        Ok(json!({"events": page.len(), "serialized_bytes": encoded.len()}))
    })?;
    let column_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('events')",
        [],
        |r| r.get(0),
    )?;
    let segments: Vec<Value> = connection.prepare("SELECT column_name, compression, COUNT(*) FROM pragma_storage_info('events') GROUP BY ALL ORDER BY 1,2")?
        .query_map([], |r| Ok(json!({"column":r.get::<_,String>(0)?, "compression":r.get::<_,String>(1)?, "segments":r.get::<_,i64>(2)?})))?.collect::<duckdb::Result<_>>()?;
    println!(
        "{}",
        json!({"engine":engine, "database_bytes":bytes, "bytes_per_event":bytes as f64/count as f64,
        "insert_seconds":insert_seconds,"events_per_second":count as f64/insert_seconds,
        "checkpoint_seconds":checkpoint_seconds, "verify_seconds":verify_seconds,"verified_events":verified,
        "otlp_records":otlp_count,"columns":column_count,"typed_fields":fields,"compression_segments":segments,
        "insert_batches_ms":insert_batches_ms,"queries":{"summary":summary,"qoe":qoe,"event_page":event_page}})
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn extraction_preserves_missing_null_nested_and_unsafe_integers() {
        let original = json!({"null":null,"browser_context":{"label":"écran","custom":[null,{}]},"big":9007199254740993_i64});
        let mut remaining = original.clone();
        for (path, kind) in [
            (vec!["null"], Kind::Text),
            (vec!["missing"], Kind::Text),
            (vec!["browser_context", "label"], Kind::Text),
            (vec!["big"], Kind::Double),
        ] {
            let f = Field {
                path: path.into_iter().map(String::from).collect(),
                kind,
            };
            let v = take(&mut remaining, &f);
            if let SqlValue::Text(v) = v {
                set(&mut remaining, &f, json!(v));
            }
        }
        assert_eq!(remaining, original);
        assert!(!equivalent(
            &json!(9007199254740993_i64),
            &json!(9007199254740992.0)
        ));
        assert!(equivalent(&json!(1), &json!(1.0)));
        assert!(!equivalent(&json!({"a":null}), &json!({})));
    }
}
