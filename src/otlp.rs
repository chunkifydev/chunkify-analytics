use crate::{
    api::ApiError,
    event::{self, IncomingEvent},
};
use serde_json::{Map, Value, json};

fn object(value: &Value) -> Result<&Map<String, Value>, ApiError> {
    value
        .as_object()
        .ok_or_else(|| ApiError::bad("Expected an OTLP message object"))
}

fn array<'a>(value: &'a Value, name: &str) -> Result<&'a [Value], ApiError> {
    object(value)?;
    match value.get(name) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(values)) => Ok(values),
        _ => Err(ApiError::bad(format!("Expected an array for {name}"))),
    }
}

fn attributes(values: &[Value]) -> Result<Map<String, Value>, ApiError> {
    let mut result = Map::new();
    for entry in values {
        let entry = object(entry)?;
        let key = match entry.get("key") {
            None | Some(Value::Null) => "",
            Some(Value::String(key)) => key,
            _ => return Err(ApiError::bad("OTLP attribute keys must be strings")),
        };
        let value = any_value(entry.get("value").unwrap_or(&Value::Null))?;
        if result.insert(key.into(), value).is_some() {
            return Err(ApiError::bad("Duplicate OTLP attribute key"));
        }
    }
    Ok(result)
}

fn any_value(value: &Value) -> Result<Value, ApiError> {
    if value.is_null() {
        return Ok(Value::Null);
    }
    let object = object(value)?;
    let fields: Vec<_> = [
        "stringValue",
        "boolValue",
        "intValue",
        "doubleValue",
        "arrayValue",
        "kvlistValue",
        "bytesValue",
    ]
    .into_iter()
    .filter(|name| object.get(*name).is_some_and(|v| !v.is_null()))
    .collect();
    let result = match fields.as_slice() {
        [] => Value::Null,
        ["stringValue"] if value["stringValue"].is_string() => value["stringValue"].clone(),
        ["boolValue"] if value["boolValue"].is_boolean() => value["boolValue"].clone(),
        ["intValue"] => {
            let integer = value["intValue"]
                .as_i64()
                .or_else(|| value["intValue"].as_str()?.parse().ok())
                .ok_or_else(|| ApiError::bad("Invalid OTLP intValue"))?;
            json!(integer)
        }
        ["doubleValue"] if value["doubleValue"].is_number() => value["doubleValue"].clone(),
        ["arrayValue"] => Value::Array(
            array(&value["arrayValue"], "values")?
                .iter()
                .map(any_value)
                .collect::<Result<_, _>>()?,
        ),
        ["kvlistValue"] => Value::Object(attributes(array(&value["kvlistValue"], "values")?)?),
        _ => return Err(ApiError::bad("Invalid or unsupported OTLP attribute value")),
    };
    Ok(result)
}

// Copy only envelope metadata. Cloning a resource/scope and then removing its
// children inside the log loop copies the entire batch once per event.
fn envelope_metadata(value: &Value, child_field: &str) -> Result<Map<String, Value>, ApiError> {
    Ok(object(value)?
        .iter()
        .filter(|(key, _)| key.as_str() != child_field)
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect())
}

pub fn decode(request: &Value, max_records: usize) -> Result<Vec<IncomingEvent>, ApiError> {
    let mut remaining = max_records;
    for resource in array(request, "resourceLogs")? {
        for scope in array(resource, "scopeLogs")? {
            remaining = remaining
                .checked_sub(array(scope, "logRecords")?.len())
                .ok_or_else(|| {
                    ApiError(
                        axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                        "OTLP record count exceeds limit".into(),
                    )
                })?;
        }
    }
    let mut events = Vec::new();
    for resource in array(request, "resourceLogs")? {
        let resource_context = envelope_metadata(resource, "scopeLogs")?;
        for scope in array(resource, "scopeLogs")? {
            let scope_context = envelope_metadata(scope, "logRecords")?;
            for log in array(scope, "logRecords")? {
                let mut event = Value::Object(attributes(array(log, "attributes")?)?);
                event::restore_otlp_nulls(&mut event);
                event::validate(&event)?;
                events.push(IncomingEvent { event, otlp: Some(json!({
                    "resource_logs": resource_context, "scope_logs": scope_context, "log_record": log,
                })) });
            }
        }
    }
    Ok(events)
}
