use crate::{api::ApiError, event};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use duckdb::types::Value as SqlValue;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub from: Option<String>,
    pub to: Option<String>,
    pub video_id: Option<String>,
    pub viewer_id: Option<String>,
    pub event_name: Option<String>,
    pub browser_name: Option<String>,
    pub os_name: Option<String>,
    pub device_type: Option<String>,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    video_id: Option<String>,
    viewer_id: Option<String>,
    event_name: Option<String>,
    browser_name: Option<String>,
    os_name: Option<String>,
    device_type: Option<String>,
}

impl Query {
    pub fn filters(&self) -> Result<Filters, ApiError> {
        let from_ms = self.from.as_deref().map(event::timestamp).transpose()?;
        let to_ms = self.to.as_deref().map(event::timestamp).transpose()?;
        if from_ms.zip(to_ms).is_some_and(|(from, to)| from >= to) {
            return Err(ApiError::bad("from must be before to"));
        }
        if self
            .event_name
            .as_deref()
            .is_some_and(|name| !event::EVENT_NAMES.contains(&name))
        {
            return Err(ApiError::bad("Unsupported event_name filter"));
        }
        for (name, value) in [
            ("browser_name", &self.browser_name),
            ("os_name", &self.os_name),
        ] {
            if value
                .as_ref()
                .is_some_and(|value| !(1..=64).contains(&value.chars().count()))
            {
                return Err(ApiError::bad(format!(
                    "{name} must contain between 1 and 64 characters"
                )));
            }
        }
        if self
            .device_type
            .as_deref()
            .is_some_and(|value| !["desktop", "mobile", "tablet", "unknown"].contains(&value))
        {
            return Err(ApiError::bad("Unsupported device_type filter"));
        }
        Ok(Filters {
            from_ms,
            to_ms,
            video_id: self.video_id.clone(),
            viewer_id: self.viewer_id.clone(),
            event_name: self.event_name.clone(),
            browser_name: self.browser_name.clone(),
            os_name: self.os_name.clone(),
            device_type: self.device_type.clone(),
        })
    }

    pub fn limit(&self) -> Result<usize, ApiError> {
        let limit = self.limit.unwrap_or(100);
        if !(1..=1000).contains(&limit) {
            return Err(ApiError::bad("limit must be between 1 and 1000"));
        }
        Ok(limit)
    }
}

impl Filters {
    pub fn sql(&self) -> (String, Vec<SqlValue>) {
        let mut conditions = vec!["TRUE"];
        let mut values = Vec::new();
        if let Some(value) = self.from_ms {
            conditions.push("occurred_at_ms >= ?");
            values.push(SqlValue::BigInt(value));
        }
        if let Some(value) = self.to_ms {
            conditions.push("occurred_at_ms < ?");
            values.push(SqlValue::BigInt(value));
        }
        if let Some(value) = &self.video_id {
            conditions.push("video_id = ?");
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = &self.viewer_id {
            conditions.push("viewer_id = ?");
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = &self.event_name {
            conditions.push("event_name = ?");
            values.push(SqlValue::Text(value.clone()));
        }
        // Compare only strings. Missing/null fields and other JSON primitives
        // are not normalized browser names or device categories.
        for (value, condition) in [
            (
                &self.browser_name,
                "json_type(event_context, '$.browser_name') = 'VARCHAR' AND json_extract_string(event_context, '$.browser_name') = ?",
            ),
            (
                &self.os_name,
                "json_type(event_context, '$.os_name') = 'VARCHAR' AND json_extract_string(event_context, '$.os_name') = ?",
            ),
            (
                &self.device_type,
                "json_type(event_context, '$.device_type') = 'VARCHAR' AND json_extract_string(event_context, '$.device_type') = ?",
            ),
        ] {
            if let Some(value) = value {
                conditions.push(condition);
                values.push(SqlValue::Text(value.clone()));
            }
        }
        (conditions.join(" AND "), values)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Cursor<T> {
    pub version: u8,
    pub kind: String,
    pub after: T,
    pub through: i64,
    pub filters: Filters,
}
impl<T: Serialize + DeserializeOwned> Cursor<T> {
    pub fn decode(encoded: &str, kind: &str, filters: &Filters) -> Result<Self, ApiError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ApiError::bad("Invalid cursor"))?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| ApiError::bad("Invalid cursor"))?;
        if cursor.version != 1
            || cursor.kind != kind
            || cursor.filters != *filters
            || cursor.through < 0
        {
            return Err(ApiError::bad("Cursor does not match this query"));
        }
        Ok(cursor)
    }
    pub fn encode(&self) -> Result<String, ApiError> {
        Ok(URL_SAFE_NO_PAD.encode(serde_json::to_vec(self).map_err(|_| ApiError::internal())?))
    }
}
