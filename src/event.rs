use crate::api::ApiError;
use serde_json::{Map, Value};

pub const EVENT_NAMES: [&str; 9] = [
    "chunkify.video.view_started",
    "chunkify.video.view_snapshot",
    "chunkify.video.playback_error",
    "chunkify.video.view_ended",
    "chunkify.video.playback_state_changed",
    "chunkify.video.rendition_changed",
    "chunkify.video.seek_completed",
    "chunkify.video.subtitle_changed",
    "chunkify.video.playback_rate_changed",
];
const STATES: [&str; 9] = [
    "startup",
    "resuming",
    "playing",
    "paused",
    "buffering",
    "seeking",
    "error",
    "ended",
    "stopped",
];

/// Maximum Unicode characters in `user_id` and `video_title`.
pub const MAX_LABEL_CHARS: usize = 256;
/// Maximum custom dimension keys on one view.
pub const MAX_CUSTOM_KEYS: usize = 10;
/// Maximum characters in a custom dimension key.
const MAX_CUSTOM_KEY_CHARS: usize = 40;
/// Maximum characters in a subtitle language tag (BCP 47, `und` when unknown).
pub const MAX_LANGUAGE_CHARS: usize = 35;
/// Accepted playback rates. Reports round rates to two decimals, so the
/// lowest rate is the smallest one that rounding keeps above zero.
pub const MIN_PLAYBACK_RATE: f64 = 0.01;
pub const MAX_PLAYBACK_RATE: f64 = 16.0;
/// Maximum entries in each cumulative per-state playing time array.
const MAX_STATE_ENTRIES: usize = 32;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct IncomingEvent {
    pub event: Value,
    pub otlp: Option<Value>,
}

pub fn timestamp(value: &str) -> Result<i64, ApiError> {
    chrono::DateTime::parse_from_rfc3339(value)
        .map(|date| date.timestamp_millis())
        .map_err(|_| ApiError::bad("Expected an RFC3339 timestamp with a timezone"))
}

fn number(value: &Value) -> bool {
    value.as_f64().is_some_and(|v| v.is_finite() && v >= 0.0)
}
fn nullable_number(value: &Value) -> bool {
    value.is_null() || number(value)
}
fn nullable_string(value: &Value) -> bool {
    value.is_null() || value.is_string()
}
fn one_of(value: &Value, allowed: &[&str]) -> bool {
    value.as_str().is_some_and(|v| allowed.contains(&v))
}
/// A custom dimension key: 1 to 40 characters of `a-z`, `0-9` and `_`.
pub fn custom_key(key: &str) -> bool {
    (1..=MAX_CUSTOM_KEY_CHARS).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_')
}
/// A custom dimension text value, or its filter: 1 to 256 characters.
pub fn custom_text(value: &str) -> bool {
    (1..=MAX_LABEL_CHARS).contains(&value.chars().count())
}
/// Custom dimensions: at most `MAX_CUSTOM_KEYS` keys, each with a text,
/// finite number or boolean value.
fn custom(value: &Value) -> bool {
    value.as_object().is_some_and(|map| {
        map.len() <= MAX_CUSTOM_KEYS
            && map.iter().all(|(key, value)| {
                custom_key(key)
                    && match value {
                        Value::String(text) => custom_text(text),
                        Value::Number(number) => number.as_f64().is_some_and(f64::is_finite),
                        Value::Bool(_) => true,
                        _ => false,
                    }
            })
    })
}
/// A subtitle language tag: at most `MAX_LANGUAGE_CHARS` characters, not blank.
pub fn language_tag(value: &str) -> bool {
    value.chars().count() <= MAX_LANGUAGE_CHARS && !value.trim().is_empty()
}
/// A subtitle language tag, or `null` when subtitles are off.
fn subtitle_language(value: &Value) -> bool {
    value.is_null() || value.as_str().is_some_and(language_tag)
}
fn rate(value: &Value) -> bool {
    value
        .as_f64()
        .is_some_and(|v| (MIN_PLAYBACK_RATE..=MAX_PLAYBACK_RATE).contains(&v))
}
/// Cumulative playing time per state: at most `MAX_STATE_ENTRIES` objects,
/// each with its state under `key` and a measured `playing_time_ms`.
fn state_times(value: &Value, key: &str, state: fn(&Value) -> bool) -> bool {
    value.is_null()
        || value.as_array().is_some_and(|entries| {
            entries.len() <= MAX_STATE_ENTRIES
                && entries.iter().all(|entry| {
                    entry.as_object().is_some_and(|e| {
                        e.get(key).is_some_and(state)
                            && e.get("playing_time_ms").is_some_and(number)
                    })
                })
        })
}
fn date(value: &Value) -> bool {
    value.as_str().is_some_and(|v| timestamp(v).is_ok())
}
fn field(
    object: &Map<String, Value>,
    name: &str,
    check: impl FnOnce(&Value) -> bool,
) -> Result<(), ApiError> {
    if object.get(name).is_some_and(check) {
        Ok(())
    } else {
        Err(ApiError::bad(format!("Invalid or missing field: {name}")))
    }
}

pub fn validate(event: &Value) -> Result<(), ApiError> {
    let object = event
        .as_object()
        .ok_or_else(|| ApiError::bad("Expected a Chunkify event object"))?;
    field(object, "schema_version", |v| v == 1)?;
    field(object, "event_name", |v| one_of(v, &EVENT_NAMES))?;
    for key in ["event_id", "view_id"] {
        field(object, key, |v| {
            v.as_str().is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        })?;
    }
    for key in ["video_id", "sdk_name", "sdk_version", "connector_name"] {
        field(object, key, |v| {
            v.as_str().is_some_and(|s| !s.trim().is_empty())
        })?;
    }
    field(object, "sequence", |v| {
        v.as_u64()
            .is_some_and(|n| (1..=9_007_199_254_740_991).contains(&n))
    })?;
    field(object, "occurred_at", date)?;
    if object.contains_key("visibility_state") {
        field(object, "visibility_state", |v| {
            v.is_null() || one_of(v, &["visible", "hidden"])
        })?;
    }
    for key in [
        "connector_version",
        "player_name",
        "player_version",
        "delivery_hostname",
        "environment",
        "viewer_id",
    ] {
        field(object, key, nullable_string)?;
    }
    // Optional integrator-supplied identity and display fields. Older SDKs omit them.
    for key in ["user_id", "video_title"] {
        if object.contains_key(key) {
            field(object, key, |v| {
                v.is_null()
                    || v.as_str()
                        .is_some_and(|s| (1..=MAX_LABEL_CHARS).contains(&s.chars().count()))
            })?;
        }
    }
    field(object, "reason", |v| {
        v.is_null() || one_of(v, &["ended", "destroyed", "source_changed", "page_exit"])
    })?;
    field(object, "startup_outcome", |v| {
        v.is_null() || one_of(v, &["started", "failed_before_start", "ended_before_start"])
    })?;
    field(object, "error_category", |v| {
        v.is_null()
            || one_of(
                v,
                &["aborted", "network", "decode", "unsupported", "unknown"],
            )
    })?;
    field(object, "error_code", |v| {
        v.is_null() || v.is_string() || v.is_number()
    })?;
    field(object, "error_fatal", |v| v.is_null() || v.is_boolean())?;
    field(object, "error_playhead_seconds", nullable_number)?;

    let name = event["event_name"].as_str().unwrap();
    let full_metrics = EVENT_NAMES[..4].contains(&name);
    if object.contains_key("played_ranges") {
        field(object, "played_ranges", valid_played_ranges)?;
    }
    if object.contains_key("playhead_seconds") {
        field(object, "playhead_seconds", nullable_number)?;
    }
    for key in [
        "playing_time_ms",
        "rebuffer_count",
        "rebuffer_duration_ms",
        "seek_count",
        "seek_duration_ms",
        "maximum_playhead_seconds",
    ] {
        if full_metrics || object.contains_key(key) {
            field(object, key, number)?;
        }
    }
    for key in [
        "media_duration_seconds",
        "startup_time_ms",
        "first_video_frame_time_ms",
        "rebuffer_ratio",
        "furthest_progress_ratio",
        "media_width_px",
        "media_height_px",
        "player_width_px",
        "player_height_px",
        "dropped_video_frames",
        "total_video_frames",
        "selected_rendition_width",
        "selected_rendition_height",
        "selected_rendition_bitrate_bps",
        "rendition_switch_count",
        "delivery_request_count",
        "delivery_transferred_bytes",
        "delivery_throughput_bps",
    ] {
        if full_metrics || object.contains_key(key) {
            field(object, key, nullable_number)?;
        }
    }
    if full_metrics || object.contains_key("selected_rendition_codec") {
        field(object, "selected_rendition_codec", nullable_string)?;
    }
    if full_metrics || object.contains_key("rendition_playing_time") {
        field(object, "rendition_playing_time", |v| {
            v.is_null()
                || v.as_array().is_some_and(|entries| {
                    entries.iter().all(|entry| {
                        entry.as_object().is_some_and(|r| {
                            ["width", "height", "bitrate_bps", "frame_rate"]
                                .iter()
                                .all(|key| r.get(*key).is_some_and(nullable_number))
                                && r.get("codec").is_some_and(nullable_string)
                                && r.get("playing_time_ms").is_some_and(number)
                        })
                    })
                })
        })?;
    }
    // Integrator-defined dimensions, set once when the view starts. Reports
    // read them only there, so other events reject them instead of ignoring them.
    if object.contains_key("custom") {
        if name != "chunkify.video.view_started" {
            return Err(ApiError::bad("custom is accepted only on view_started"));
        }
        field(object, "custom", custom)?;
    }
    // Subtitle and playback rate state. Optional: older SDKs omit it.
    if object.contains_key("subtitle_language") {
        field(object, "subtitle_language", subtitle_language)?;
    }
    if object.contains_key("playback_rate") {
        field(object, "playback_rate", |v| v.is_null() || rate(v))?;
    }
    if object.contains_key("subtitle_playing_time") {
        field(object, "subtitle_playing_time", |v| {
            state_times(v, "language", subtitle_language)
        })?;
    }
    if object.contains_key("playback_rate_playing_time") {
        field(object, "playback_rate_playing_time", |v| {
            state_times(v, "rate", rate)
        })?;
    }
    match name {
        "chunkify.video.subtitle_changed" => {
            field(object, "playhead_seconds", nullable_number)?;
            for key in ["previous_subtitle_language", "subtitle_language"] {
                field(object, key, subtitle_language)?;
            }
        }
        "chunkify.video.playback_rate_changed" => {
            field(object, "playhead_seconds", nullable_number)?;
            for key in ["previous_playback_rate", "playback_rate"] {
                field(object, key, |v| v.is_null() || rate(v))?;
            }
        }
        "chunkify.video.playback_state_changed" => {
            field(object, "playhead_seconds", nullable_number)?;
            for key in ["from_state", "to_state"] {
                field(object, key, |v| one_of(v, &STATES))?;
            }
            field(object, "previous_state_duration_ms", number)?;
        }
        "chunkify.video.rendition_changed" => {
            for key in [
                "playhead_seconds",
                "player_width_px",
                "player_height_px",
                "previous_rendition_width",
                "previous_rendition_height",
                "previous_rendition_bitrate_bps",
                "previous_rendition_frame_rate",
                "selected_rendition_width",
                "selected_rendition_height",
                "selected_rendition_bitrate_bps",
                "selected_rendition_frame_rate",
            ] {
                field(object, key, nullable_number)?;
            }
            for key in ["previous_rendition_codec", "selected_rendition_codec"] {
                field(object, key, nullable_string)?;
            }
        }
        "chunkify.video.seek_completed" => {
            field(object, "seek_started_at", date)?;
            for key in ["last_seeked_at", "playback_resumed_at"] {
                field(object, key, |v| v.is_null() || date(v))?;
            }
            for key in ["state_before_seek", "state_after_seek"] {
                field(object, key, |v| one_of(v, &STATES))?;
            }
            for key in [
                "from_playhead_seconds",
                "to_playhead_seconds",
                "seek_resume_wait_ms",
            ] {
                field(object, key, nullable_number)?;
            }
            field(object, "seek_interaction_duration_ms", number)?;
            field(object, "seek_outcome", |v| {
                one_of(v, &["resumed", "paused", "interrupted"])
            })?;
        }
        _ => {}
    }
    Ok(())
}

pub(crate) fn valid_played_ranges(value: &Value) -> bool {
    if value.is_null() {
        return true;
    }
    let Some(ranges) = value.as_array() else {
        return false;
    };
    if ranges.len() > 128 {
        return false;
    }
    let mut previous_end = -1.0;
    for range in ranges {
        let (Some(start), Some(end)) = (
            range["start_seconds"].as_f64(),
            range["end_seconds"].as_f64(),
        ) else {
            return false;
        };
        if !start.is_finite()
            || !end.is_finite()
            || start < 0.0
            || end <= start
            || start <= previous_end
        {
            return false;
        }
        previous_end = end;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::validate;
    use serde_json::{Value, json};

    fn change(fields: Value) -> Value {
        let mut event = json!({"schema_version": 1, "event_name": "chunkify.video.subtitle_changed",
            "event_id": "00000000-0000-4000-8000-000000000001",
            "view_id": "00000000-0000-4000-8000-000000000002", "video_id": "video",
            "sdk_name": "sdk", "sdk_version": "1", "connector_name": "chunkify", "sequence": 2,
            "occurred_at": "2026-01-01T00:00:00Z", "connector_version": null, "player_name": null,
            "player_version": null, "delivery_hostname": null, "environment": null,
            "viewer_id": null, "reason": null, "startup_outcome": null, "error_category": null,
            "error_code": null, "error_fatal": null, "error_playhead_seconds": null,
            "playhead_seconds": 3, "previous_subtitle_language": null, "subtitle_language": "fr"});
        for (key, value) in fields.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    }

    fn rate_change(fields: Value) -> Value {
        let mut event = change(json!({"event_name": "chunkify.video.playback_rate_changed",
            "previous_playback_rate": 1, "playback_rate": 1.5}));
        let object = event.as_object_mut().unwrap();
        object.remove("previous_subtitle_language");
        object.remove("subtitle_language");
        for (key, value) in fields.as_object().unwrap() {
            event[key] = value.clone();
        }
        event
    }

    #[test]
    fn change_events_require_their_states() {
        assert!(validate(&change(json!({}))).is_ok());
        assert!(validate(&change(json!({"previous_subtitle_language": "und"}))).is_ok());
        assert!(validate(&change(json!({"subtitle_language": null}))).is_ok());
        let mut missing = change(json!({}));
        missing.as_object_mut().unwrap().remove("subtitle_language");
        assert!(validate(&missing).is_err());
        assert!(validate(&change(json!({"subtitle_language": ""}))).is_err());
        assert!(validate(&change(json!({"subtitle_language": "x".repeat(36)}))).is_err());
        assert!(validate(&change(json!({"subtitle_language": "x".repeat(35)}))).is_ok());
        assert!(validate(&change(json!({"subtitle_language": 1}))).is_err());
        assert!(validate(&change(json!({"subtitle_language": "  "}))).is_err());

        assert!(validate(&rate_change(json!({}))).is_ok());
        assert!(validate(&rate_change(json!({"playback_rate": 16}))).is_ok());
        assert!(validate(&rate_change(json!({"playback_rate": 0.01}))).is_ok());
        for rate in [json!(0), json!(0.004), json!(-1), json!(16.01), json!("2")] {
            assert!(validate(&rate_change(json!({"playback_rate": rate}))).is_err());
        }
        let mut missing = rate_change(json!({}));
        missing
            .as_object_mut()
            .unwrap()
            .remove("previous_playback_rate");
        assert!(validate(&missing).is_err());
    }

    #[test]
    fn state_fields_are_optional_and_bounded_on_every_event() {
        // Older SDKs omit every new field.
        let mut old = change(json!({"event_name": "chunkify.video.seek_completed",
            "seek_started_at": "2026-01-01T00:00:00Z", "last_seeked_at": null,
            "playback_resumed_at": null, "state_before_seek": "playing",
            "state_after_seek": "playing", "from_playhead_seconds": 1, "to_playhead_seconds": 2,
            "seek_resume_wait_ms": null, "seek_interaction_duration_ms": 10,
            "seek_outcome": "resumed"}));
        let object = old.as_object_mut().unwrap();
        object.remove("previous_subtitle_language");
        object.remove("subtitle_language");
        assert!(validate(&old).is_ok());
        let with = |fields: Value| {
            let mut event = old.clone();
            for (key, value) in fields.as_object().unwrap() {
                event[key] = value.clone();
            }
            validate(&event)
        };
        assert!(with(json!({"subtitle_language": null, "playback_rate": null})).is_ok());
        assert!(with(json!({"playback_rate": 0})).is_err());
        let entry =
            |language: Value, time: Value| json!({"language": language, "playing_time_ms": time});
        assert!(
            with(json!({"subtitle_playing_time": [entry(json!("fr"), json!(90000)), entry(Value::Null, json!(0))]}))
                .is_ok()
        );
        assert!(
            with(json!({"subtitle_playing_time": null, "playback_rate_playing_time": null}))
                .is_ok()
        );
        assert!(with(json!({"subtitle_playing_time": [entry(json!(""), json!(1))]})).is_err());
        assert!(with(json!({"subtitle_playing_time": [entry(json!("fr"), json!(-1))]})).is_err());
        assert!(with(json!({"subtitle_playing_time": [{"language": "fr"}]})).is_err());
        assert!(with(json!({"subtitle_playing_time": {"language": "fr"}})).is_err());
        let rates = |count: usize| {
            json!({"playback_rate_playing_time": (0..count)
                .map(|n| json!({"rate": 1.0 + n as f64 / 100.0, "playing_time_ms": 1}))
                .collect::<Vec<_>>()})
        };
        assert!(with(rates(32)).is_ok());
        assert!(with(rates(33)).is_err());
        assert!(
            with(json!({"playback_rate_playing_time": [{"rate": 17, "playing_time_ms": 1}]}))
                .is_err()
        );
        assert!(
            with(json!({"playback_rate_playing_time": [{"rate": null, "playing_time_ms": 1}]}))
                .is_err()
        );
    }

    #[test]
    fn custom_dimensions_are_bounded_named_scalars() {
        let start = |custom: Value| {
            if super::custom(&custom) {
                Ok(())
            } else {
                Err(())
            }
        };
        // Reports read custom values from view starts only.
        assert!(validate(&change(json!({"custom": {"plan": "premium"}}))).is_err());
        assert!(
            start(json!({"plan": "premium", "seats": 5, "ratio": 0.5, "trial": false})).is_ok()
        );
        assert!(start(json!({})).is_ok());
        assert!(start(json!({"a".repeat(40): "x", "b_2": "y".repeat(256)})).is_ok());
        let ten: serde_json::Map<_, _> = (0..10).map(|n| (format!("k{n}"), json!(n))).collect();
        assert!(start(Value::Object(ten.clone())).is_ok());
        let mut eleven = ten;
        eleven.insert("k10".into(), json!(10));
        assert!(start(Value::Object(eleven)).is_err());
        for invalid in [
            json!({"Plan": "x"}),
            json!({"plan-name": "x"}),
            json!({"custom.plan": "x"}),
            json!({"a".repeat(41): "x"}),
            json!({"": "x"}),
            json!({"plan": ""}),
            json!({"plan": "y".repeat(257)}),
            json!({"plan": null}),
            json!({"plan": ["x"]}),
            json!({"plan": {"x": 1}}),
            json!(["plan"]),
            json!("plan"),
            Value::Null,
        ] {
            assert!(start(invalid.clone()).is_err(), "{invalid}");
        }
    }
}
