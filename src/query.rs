use crate::{api::ApiError, event};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use duckdb::types::Value as SqlValue;
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    #[default]
    Asc,
    Desc,
}

impl SortOrder {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Asc => "asc",
            Self::Desc => "desc",
        }
    }
}

#[derive(Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoSort {
    #[default]
    VideoId,
    Views,
    TotalPlayingTimeMs,
}

impl VideoSort {
    pub fn column(self) -> &'static str {
        match self {
            Self::VideoId => "video_id",
            Self::Views => "views",
            Self::TotalPlayingTimeMs => "total_playing_time_ms",
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    #[serde(skip)]
    pub export: Option<std::sync::Arc<crate::exports::Sink>>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub video_id: Option<String>,
    pub viewer_id: Option<String>,
    pub view_id: Option<String>,
    pub event_name: Option<String>,
    pub browser_name: Option<String>,
    pub os_name: Option<String>,
    pub device_type: Option<String>,
    pub continent: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
    #[serde(default, deserialize_with = "optional_limit")]
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub order: Option<SortOrder>,
    pub sort_by: Option<VideoSort>,
    pub group_by: Option<BreakdownBy>,
}

fn optional_limit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<usize>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Number(usize),
        Text(String),
    }
    Option::<Input>::deserialize(d)?
        .map(|v| match v {
            Input::Number(v) => Ok(v),
            Input::Text(v) => v.parse().map_err(serde::de::Error::custom),
        })
        .transpose()
}

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum MetricKind {
    #[default]
    Summary,
    Quality,
    Engagement,
}

pub struct ViewsQuery {
    pub has_fatal_error: Option<bool>,
    pub query: Query,
}
impl<'de> Deserialize<'de> for ViewsQuery {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let mut fields = serde_json::Map::<String, serde_json::Value>::deserialize(d)?;
        let has_fatal_error = fields
            .remove("has_fatal_error")
            .map(|v| {
                v.as_bool()
                    .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
                    .ok_or_else(|| {
                        serde::de::Error::custom("has_fatal_error must be true or false")
                    })
            })
            .transpose()?;
        let query = serde_json::from_value(serde_json::Value::Object(fields))
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            has_fatal_error,
            query,
        })
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    BrowserName,
    OsName,
    DeviceType,
    Continent,
    Country,
    City,
    VideoId,
    ViewerId,
}

impl Dimension {
    pub fn column(self) -> &'static str {
        match self {
            Self::BrowserName => "context_browser_name",
            Self::OsName => "context_os_name",
            Self::DeviceType => "context_device_type",
            Self::Continent => {
                "CASE WHEN json_type(context_remainder, '$.continent') = 'VARCHAR' THEN json_extract_string(context_remainder, '$.continent') END"
            }
            Self::Country => {
                "CASE WHEN json_type(context_remainder, '$.country') = 'VARCHAR' THEN json_extract_string(context_remainder, '$.country') END"
            }
            Self::City => {
                "CASE WHEN json_type(context_remainder, '$.city') = 'VARCHAR' THEN json_extract_string(context_remainder, '$.city') END"
            }
            Self::VideoId => "video_id",
            Self::ViewerId => "viewer_id",
        }
    }
}

impl MetricKind {
    pub fn name(self) -> &'static str {
        match self {
            Self::Summary => "summary",
            Self::Quality => "quality",
            Self::Engagement => "engagement",
        }
    }
}

pub const MAX_TIME_BUCKETS: usize = 1000;

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BreakdownBy {
    BrowserName,
    OsName,
    DeviceType,
    Continent,
    Country,
    City,
}

impl BreakdownBy {
    pub fn column(self) -> &'static str {
        match self {
            Self::BrowserName => "context_browser_name",
            Self::OsName => "context_os_name",
            Self::DeviceType => "context_device_type",
            Self::Continent => Dimension::Continent.column(),
            Self::Country => Dimension::Country.column(),
            Self::City => Dimension::City.column(),
        }
    }
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TimeInterval {
    Hour,
    Day,
}

impl TimeInterval {
    pub fn milliseconds(self) -> i64 {
        match self {
            Self::Hour => 3_600_000,
            Self::Day => 86_400_000,
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimeseriesQuery {
    #[serde(skip)]
    pub export: Option<std::sync::Arc<crate::exports::Sink>>,
    #[serde(default)]
    pub kind: MetricKind,
    pub from: String,
    pub to: String,
    pub interval: TimeInterval,
    pub video_id: Option<String>,
    pub viewer_id: Option<String>,
    pub view_id: Option<String>,
    pub browser_name: Option<String>,
    pub os_name: Option<String>,
    pub device_type: Option<String>,
    pub continent: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
}

pub struct TimeseriesWindow {
    pub filters: Filters,
    pub interval: TimeInterval,
    pub from_ms: i64,
    pub to_ms: i64,
    pub first_bucket_ms: i64,
    pub bucket_count: usize,
}

impl TimeseriesQuery {
    pub fn validate(self) -> Result<TimeseriesWindow, ApiError> {
        self.window(true)
    }
    pub fn validate_export(self) -> Result<TimeseriesWindow, ApiError> {
        self.window(false)
    }
    fn window(self, bounded: bool) -> Result<TimeseriesWindow, ApiError> {
        let filters = Query {
            from: Some(self.from),
            to: Some(self.to),
            video_id: self.video_id,
            viewer_id: self.viewer_id,
            view_id: self.view_id,
            browser_name: self.browser_name,
            os_name: self.os_name,
            device_type: self.device_type,
            continent: self.continent,
            country: self.country,
            city: self.city,
            ..Query::default()
        }
        .filters()?;
        let from_ms = filters.from_ms.expect("required from");
        let to_ms = filters.to_ms.expect("required to");
        let width = self.interval.milliseconds();
        let first_bucket_ms = from_ms.div_euclid(width) * width;
        let count = (to_ms - 1).div_euclid(width) - from_ms.div_euclid(width) + 1;
        if bounded && count > MAX_TIME_BUCKETS as i64 {
            return Err(ApiError::bad(
                "Time series supports at most 1000 buckets. Use a shorter date range or the day interval.",
            ));
        }
        Ok(TimeseriesWindow {
            filters,
            interval: self.interval,
            from_ms,
            to_ms,
            first_bucket_ms,
            bucket_count: count as usize,
        })
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    video_id: Option<String>,
    viewer_id: Option<String>,
    #[serde(default)]
    view_id: Option<String>,
    event_name: Option<String>,
    browser_name: Option<String>,
    os_name: Option<String>,
    device_type: Option<String>,
    continent: Option<String>,
    country: Option<String>,
    city: Option<String>,
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
        if self
            .continent
            .as_deref()
            .is_some_and(|value| !["AF", "AN", "AS", "EU", "NA", "OC", "SA"].contains(&value))
        {
            return Err(ApiError::bad("Unsupported continent filter"));
        }
        if self.country.as_deref().is_some_and(|value| {
            value.len() != 2 || !value.bytes().all(|byte| byte.is_ascii_uppercase())
        }) {
            return Err(ApiError::bad(
                "country must be a two-letter uppercase country code",
            ));
        }
        if self
            .city
            .as_deref()
            .is_some_and(|value| !(1..=256).contains(&value.chars().count()))
        {
            return Err(ApiError::bad(
                "city must contain between 1 and 256 characters",
            ));
        }
        if self
            .view_id
            .as_ref()
            .is_some_and(|value| !(1..=4096).contains(&value.chars().count()))
        {
            return Err(ApiError::bad(
                "view_id must contain between 1 and 4096 characters",
            ));
        }
        Ok(Filters {
            from_ms,
            to_ms,
            video_id: self.video_id.clone(),
            viewer_id: self.viewer_id.clone(),
            view_id: self.view_id.clone(),
            event_name: self.event_name.clone(),
            browser_name: self.browser_name.clone(),
            os_name: self.os_name.clone(),
            device_type: self.device_type.clone(),
            continent: self.continent.clone(),
            country: self.country.clone(),
            city: self.city.clone(),
        })
    }

    pub fn limit(&self) -> Result<usize, ApiError> {
        if self.export.is_some() {
            return Ok(i64::MAX as usize - 1);
        }
        let limit = self.limit.unwrap_or(100);
        if !(1..=1000).contains(&limit) {
            return Err(ApiError::bad("limit must be between 1 and 1000"));
        }
        Ok(limit)
    }
}

impl Filters {
    pub fn sql(&self) -> (String, Vec<SqlValue>) {
        let mut conditions = vec!["TRUE".to_owned()];
        let mut values = Vec::new();
        if let Some(value) = self.from_ms {
            conditions.push("occurred_at_ms >= ?".into());
            values.push(SqlValue::BigInt(value));
        }
        if let Some(value) = self.to_ms {
            conditions.push("occurred_at_ms < ?".into());
            values.push(SqlValue::BigInt(value));
        }
        if let Some(value) = &self.video_id {
            conditions.push("video_id = ?".into());
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = &self.viewer_id {
            conditions.push("viewer_id = ?".into());
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = &self.view_id {
            conditions.push("view_id = ?".into());
            values.push(SqlValue::Text(value.clone()));
        }
        if let Some(value) = &self.event_name {
            conditions.push("event_name = ?".into());
            values.push(SqlValue::Text(value.clone()));
        }
        // Compare only strings. Missing/null fields and other JSON primitives
        // are not normalized browser, device or location values.
        for (value, column) in [
            (&self.browser_name, Dimension::BrowserName.column()),
            (&self.os_name, Dimension::OsName.column()),
            (&self.device_type, Dimension::DeviceType.column()),
            (&self.continent, Dimension::Continent.column()),
            (&self.country, Dimension::Country.column()),
            (&self.city, Dimension::City.column()),
        ] {
            if let Some(value) = value {
                conditions.push(format!("{column} = ?"));
                values.push(SqlValue::Text(value.clone()));
            }
        }
        (conditions.join(" AND "), values)
    }
}

#[derive(Serialize, Deserialize)]
pub struct Cursor<T> {
    pub version: u8,
    #[serde(default = "crate::projects::default_project")]
    pub project_id: String,
    pub kind: String,
    pub after: T,
    pub through: i64,
    pub filters: Filters,
}
impl<T: Serialize + DeserializeOwned> Cursor<T> {
    pub fn decode(
        encoded: &str,
        kind: &str,
        filters: &Filters,
        project: &str,
    ) -> Result<Self, ApiError> {
        let bytes = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| ApiError::bad("Invalid cursor"))?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| ApiError::bad("Invalid cursor"))?;
        if cursor.project_id != project
            || cursor.version != 1
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
