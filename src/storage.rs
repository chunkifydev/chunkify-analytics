//! Lossless event storage. Writes and reads share this field map; unknown
//! properties and values that do not fit a typed column stay in JSON.
use crate::{api::ApiError, context::event_context_v1, event};
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
// requires. Changing it changes the ClickHouse table definition.
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
    // Integrator-supplied identity and display labels.
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
// Column names and ClickHouse base types, before nullability.
const BASE: &[(&str, &str)] = &[
    ("event_id", "String"),
    ("view_id", "String"),
    ("video_id", "String"),
    ("viewer_id", "String"),
    ("event_name", "String"),
    ("sequence", "Int64"),
    ("occurred_at_ms", "Int64"),
    ("playing_time_ms", "Float64"),
    ("received_at_ms", "Int64"),
    ("event_remainder", "String"),
    ("context_remainder", "String"),
    ("context_format", "Int32"),
    ("context_browser_name", "String"),
    ("context_os_name", "String"),
    ("context_device_type", "String"),
];

pub static COLUMNS: LazyLock<Vec<(String, &'static str)>> = LazyLock::new(|| {
    let mut columns: Vec<_> = BASE
        .iter()
        .map(|(name, kind)| (name.to_string(), *kind))
        .collect();
    for (prefix, fields) in [("e", EVENT_FIELDS), ("b", BROWSER_FIELDS)] {
        columns.extend(fields.iter().map(|(name, kind)| {
            (
                format!("{prefix}_{name}"),
                match kind {
                    Text => "String",
                    Number => "Float64",
                    Integer => "Int64",
                    Boolean => "Bool",
                },
            )
        }));
    }
    columns.push(("project_id".into(), "String"));
    columns.push(("ingest_key_id".into(), "String"));
    columns
});
pub static COLUMN_NAMES: LazyLock<String> = LazyLock::new(|| {
    COLUMNS
        .iter()
        .map(|(name, _)| name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
});
// Keep integer JSON numbers outside the exact DOUBLE range and integral JSON
// floats (including negative zero) in JSON. This preserves their representation
// as well as their value without a per-property type bitmap.
fn scalar(value: &Value, kind: Kind) -> Option<Value> {
    match kind {
        Text => value.as_str().map(|v| Value::String(v.into())),
        Integer => value.as_i64().map(Value::from),
        Boolean => value.as_bool().map(Value::Bool),
        Number => {
            let number = value.as_f64()?;
            if !number.is_finite()
                || (value.is_f64() && number.fract() == 0.0)
                || (!value.is_f64() && number.abs() > 9_007_199_254_740_991.0)
            {
                None
            } else {
                serde_json::Number::from_f64(number).map(Value::Number)
            }
        }
    }
}
fn take(object: &mut Map<String, Value>, key: &str, kind: Kind) -> Value {
    match object.get(key).and_then(|value| scalar(value, kind)) {
        Some(value) => {
            object.remove(key);
            value
        }
        None => Value::Null,
    }
}
fn text(value: Option<&Value>) -> Value {
    value
        .and_then(Value::as_str)
        .map_or(Value::Null, |v| Value::String(v.into()))
}

/// One value per `COLUMNS` entry, in order. `context` is the context response
/// stored with the event; format 1 keeps only what differs from the browser context.
pub fn encode(
    event: &Value,
    context: Option<&Value>,
    received: i64,
    project: &str,
    key_id: Option<&str>,
) -> Result<Vec<Value>, ApiError> {
    let mut remainder = event.as_object().ok_or_else(ApiError::internal)?.clone();
    let mut values = Vec::with_capacity(COLUMNS.len());
    for key in ["event_id", "view_id", "video_id", "viewer_id", "event_name"] {
        values.push(take(&mut remainder, key, Text));
    }
    values.push(take(&mut remainder, "sequence", Integer));
    values.push(Value::from(event::timestamp(
        event["occurred_at"]
            .as_str()
            .ok_or_else(ApiError::internal)?,
    )?));
    // Query metrics retain the previous DOUBLE semantics. The original value
    // stays in JSON whenever converting it would lose information.
    values.push(
        event["playing_time_ms"]
            .as_f64()
            .and_then(serde_json::Number::from_f64)
            .map_or(Value::Null, Value::Number),
    );
    take(&mut remainder, "playing_time_ms", Number);
    values.push(Value::from(received));
    values.push(Value::Null); // event remainder, filled after extraction
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
    values.push(stored.map_or(Value::Null, |v| Value::String(v.to_string())));
    values.push(Value::from(format));
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
                .map_or(Value::Null, |browser| take(browser, key, kind)),
        );
    }
    // Retain an empty browser object to distinguish {} from a missing property.
    values[9] = Value::String(Value::Object(remainder).to_string());
    values.push(Value::String(project.into()));
    values.push(key_id.map_or(Value::Null, |id| Value::String(id.into())));
    Ok(values)
}

/// Typed access to one stored row, as returned by ClickHouse JSONCompactEachRow.
trait StoredRow {
    fn text(&self, index: usize) -> Result<Option<String>, ApiError>;
    fn integer(&self, index: usize) -> Result<Option<i64>, ApiError>;
    fn number(&self, index: usize) -> Result<Option<f64>, ApiError>;
    fn boolean(&self, index: usize) -> Result<Option<bool>, ApiError>;
}

impl StoredRow for [Value] {
    fn text(&self, index: usize) -> Result<Option<String>, ApiError> {
        match self.get(index) {
            Some(Value::Null) => Ok(None),
            Some(Value::String(value)) => Ok(Some(value.clone())),
            _ => Err(ApiError::internal()),
        }
    }
    fn integer(&self, index: usize) -> Result<Option<i64>, ApiError> {
        match self.get(index) {
            Some(Value::Null) => Ok(None),
            Some(value) => value.as_i64().map(Some).ok_or_else(ApiError::internal),
            None => Err(ApiError::internal()),
        }
    }
    fn number(&self, index: usize) -> Result<Option<f64>, ApiError> {
        match self.get(index) {
            Some(Value::Null) => Ok(None),
            Some(value) => value.as_f64().map(Some).ok_or_else(ApiError::internal),
            None => Err(ApiError::internal()),
        }
    }
    fn boolean(&self, index: usize) -> Result<Option<bool>, ApiError> {
        match self.get(index) {
            Some(Value::Null) => Ok(None),
            Some(Value::Bool(value)) => Ok(Some(*value)),
            Some(Value::Number(value)) => match value.as_u64() {
                Some(0) => Ok(Some(false)),
                Some(1) => Ok(Some(true)),
                _ => Err(ApiError::internal()),
            },
            _ => Err(ApiError::internal()),
        }
    }
}

fn put(
    row: &[Value],
    index: usize,
    object: &mut Map<String, Value>,
    key: &str,
    kind: Kind,
) -> Result<(), ApiError> {
    if object.contains_key(key) {
        return Ok(());
    }
    let value = match kind {
        Text => row.text(index)?.map(Value::String),
        Integer => row.integer(index)?.map(|v| json!(v)),
        Boolean => row.boolean(index)?.map(Value::Bool),
        Number => row.number(index)?.map(|v| {
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
pub fn decode(row: &[Value]) -> Result<(i64, Value), ApiError> {
    let id = row.integer(0)?.ok_or_else(ApiError::internal)?;
    let mut event: Value = serde_json::from_str(&row.text(10)?.ok_or_else(ApiError::internal)?)
        .map_err(|_| ApiError::internal())?;
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
        .text(11)?
        .map(|s| serde_json::from_str::<Value>(&s))
        .transpose()
        .map_err(|_| ApiError::internal())?;
    let context = match row.integer(12)?.ok_or_else(ApiError::internal)? {
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
    let received = row.integer(9)?.ok_or_else(ApiError::internal)?;
    let mut record =
        json!({"received_at": chrono::DateTime::from_timestamp_millis(received), "event": event});
    if let Some(context) = context {
        record["event_context"] = context;
    }
    record["project_id"] = json!(row.text(index)?.ok_or_else(ApiError::internal)?);
    record["ingest_key_id"] = json!(row.text(index + 1)?);
    if let Some(otlp) = row.text(index + 2)? {
        record["otlp"] = serde_json::from_str(&otlp).map_err(|_| ApiError::internal())?;
    }
    Ok((id, record))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(id: usize) -> Value {
        json!({"event_id": format!("event-{id}"), "view_id": "view", "video_id": "video",
            "viewer_id": null, "event_name": "chunkify.video.view_started", "sequence": id,
            "occurred_at": "2026-10-19T12:00:00.123456+02:00", "schema_version": 1,
            "playing_time_ms": 123, "startup_time_ms": 1.25,
            "browser_context": {"schema_version": 1, "browser_name": "Firefox", "pixel_ratio": 1.5}})
    }

    // ClickHouse returns typed columns as JSON scalars: the encoded values, with
    // the event order first and the original OTLP payload last.
    fn stored(values: Vec<Value>, otlp: Option<&Value>) -> Vec<Value> {
        std::iter::once(json!(7))
            .chain(values)
            .chain(std::iter::once(
                otlp.map_or(Value::Null, |v| Value::String(v.to_string())),
            ))
            .collect()
    }

    #[test]
    fn typed_storage_round_trips_unusual_values_and_historical_context() {
        let mut cases = 0;
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
                cases += 1;
                let mut e = event(cases);
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
                let context = match cases % 5 {
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
                assert_eq!(values.len(), COLUMNS.len());
                let otlp = json!({"log_record": {"body": "original"}, "resource_logs": {"custom": [1, null]}});
                let (id, record) = decode(&stored(values, Some(&otlp)))
                    .map_err(|e| e.message)
                    .unwrap();
                let mut expected = json!({"project_id": "default", "ingest_key_id": null, "event": e,
                    "received_at": chrono::DateTime::from_timestamp_millis(123456789), "otlp": otlp});
                if let Some(context) = context {
                    expected["event_context"] = context;
                }
                assert_eq!(id, 7);
                assert_eq!(record, expected);
            }
        }
    }
}
