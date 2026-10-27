use crate::api::ApiError;
use serde_json::{Map, Value};

pub const EVENT_NAMES: [&str; 7] = [
    "chunkify.video.view_started",
    "chunkify.video.view_snapshot",
    "chunkify.video.playback_error",
    "chunkify.video.view_ended",
    "chunkify.video.playback_state_changed",
    "chunkify.video.rendition_changed",
    "chunkify.video.seek_completed",
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
    field(object, "sdk_name", |v| v == "@chunkify/analytics")?;
    for key in ["event_id", "view_id"] {
        field(object, key, |v| {
            v.as_str().is_some_and(|s| uuid::Uuid::parse_str(s).is_ok())
        })?;
    }
    for key in ["video_id", "sdk_version", "connector_name"] {
        field(object, key, |v| {
            v.as_str().is_some_and(|s| !s.trim().is_empty())
        })?;
    }
    field(object, "sequence", |v| {
        v.as_u64()
            .is_some_and(|n| (1..=9_007_199_254_740_991).contains(&n))
    })?;
    field(object, "occurred_at", date)?;
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
    match name {
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
