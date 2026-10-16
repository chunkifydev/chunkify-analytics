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
        Ok(Filters {
            from_ms,
            to_ms,
            video_id: self.video_id.clone(),
            viewer_id: self.viewer_id.clone(),
            event_name: self.event_name.clone(),
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
