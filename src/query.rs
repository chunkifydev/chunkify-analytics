use crate::{api::ApiError, event};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
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

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoSort {
    #[default]
    VideoId,
    Views,
    TotalPlayingTimeMs,
    UniqueViewers,
    PlaybackScore,
}

impl VideoSort {
    pub fn column(self) -> &'static str {
        match self {
            Self::VideoId => "video_id",
            Self::Views => "views",
            Self::TotalPlayingTimeMs => "total_playing_time_ms",
            Self::UniqueViewers => "unique_viewers",
            Self::PlaybackScore => "score",
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    #[serde(skip)]
    pub export: Option<std::sync::Arc<crate::exports::Sink>>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub video_id: Option<String>,
    pub viewer_id: Option<String>,
    pub user_id: Option<String>,
    pub view_id: Option<String>,
    pub event_name: Option<String>,
    pub browser_name: Option<String>,
    pub os_name: Option<String>,
    pub device_type: Option<String>,
    pub continent: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
    pub page_origin: Option<String>,
    pub page_path: Option<String>,
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

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewSort {
    #[default]
    StartedAt,
    VideoId,
    ViewId,
    DeviceType,
    PlayingTimeMs,
    StartupTimeMs,
    RebufferDurationMs,
    PlaybackScore,
}
impl ViewSort {
    pub fn column(self) -> &'static str {
        match self {
            Self::StartedAt => "occurred_at_ms",
            Self::VideoId => "video_id",
            Self::ViewId => "view_id",
            Self::DeviceType => "context_device_type",
            Self::PlayingTimeMs => "playing_time_ms",
            Self::StartupTimeMs => "m_startup_time_ms",
            Self::RebufferDurationMs => "m_rebuffer_duration_ms",
            Self::PlaybackScore => "playback_score",
        }
    }
    pub fn is_text(self) -> bool {
        matches!(self, Self::VideoId | Self::ViewId | Self::DeviceType)
    }
}

pub struct ViewsQuery {
    pub sort_by: ViewSort,
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
        let sort_by = fields
            .remove("sort_by")
            .map(serde_json::from_value)
            .transpose()
            .map_err(serde::de::Error::custom)?
            .unwrap_or_default();
        let query = serde_json::from_value(serde_json::Value::Object(fields))
            .map_err(serde::de::Error::custom)?;
        Ok(Self {
            sort_by,
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
    PageOrigin,
    PagePath,
    VideoId,
    ViewerId,
    UserId,
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
    PageOrigin,
    PagePath,
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

#[derive(Deserialize, Serialize)]
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
    pub user_id: Option<String>,
    pub view_id: Option<String>,
    pub browser_name: Option<String>,
    pub os_name: Option<String>,
    pub device_type: Option<String>,
    pub continent: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
    pub page_origin: Option<String>,
    pub page_path: Option<String>,
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
            user_id: self.user_id,
            view_id: self.view_id,
            browser_name: self.browser_name,
            os_name: self.os_name,
            device_type: self.device_type,
            continent: self.continent,
            country: self.country,
            city: self.city,
            page_origin: self.page_origin,
            page_path: self.page_path,
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

#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Filters {
    from_ms: Option<i64>,
    to_ms: Option<i64>,
    video_id: Option<String>,
    viewer_id: Option<String>,
    #[serde(default)]
    user_id: Option<String>,
    #[serde(default)]
    view_id: Option<String>,
    event_name: Option<String>,
    browser_name: Option<String>,
    os_name: Option<String>,
    device_type: Option<String>,
    continent: Option<String>,
    country: Option<String>,
    city: Option<String>,
    page_origin: Option<String>,
    page_path: Option<String>,
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
        for (name, value) in [
            ("page_origin", &self.page_origin),
            ("page_path", &self.page_path),
        ] {
            if value
                .as_ref()
                .is_some_and(|value| !(1..=4096).contains(&value.chars().count()))
            {
                return Err(ApiError::bad(format!(
                    "{name} must contain between 1 and 4096 characters"
                )));
            }
        }
        if self
            .user_id
            .as_ref()
            .is_some_and(|value| !(1..=event::MAX_LABEL_CHARS).contains(&value.chars().count()))
        {
            return Err(ApiError::bad(format!(
                "user_id must contain between 1 and {} characters",
                event::MAX_LABEL_CHARS
            )));
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
            user_id: self.user_id.clone(),
            view_id: self.view_id.clone(),
            event_name: self.event_name.clone(),
            browser_name: self.browser_name.clone(),
            os_name: self.os_name.clone(),
            device_type: self.device_type.clone(),
            continent: self.continent.clone(),
            country: self.country.clone(),
            city: self.city.clone(),
            page_origin: self.page_origin.clone(),
            page_path: self.page_path.clone(),
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
    /// SQL conditions with bound values. Column names match both the events
    /// table and the view cohort, where context values are extracted columns.
    pub fn clickhouse(&self, params: &mut crate::clickhouse::Params) -> String {
        let mut conditions = vec!["1".to_owned()];
        if let Some(value) = self.from_ms {
            conditions.push(format!("occurred_at_ms >= {}", params.int(value)));
        }
        if let Some(value) = self.to_ms {
            conditions.push(format!("occurred_at_ms < {}", params.int(value)));
        }
        for (value, column) in [
            (&self.video_id, "video_id"),
            (&self.viewer_id, "viewer_id"),
            (&self.user_id, "e_user_id"),
            (&self.view_id, "view_id"),
            (&self.event_name, "event_name"),
            (&self.browser_name, "context_browser_name"),
            (&self.os_name, "context_os_name"),
            (&self.device_type, "context_device_type"),
            (&self.continent, "ctx_continent"),
            (&self.country, "ctx_country"),
            (&self.city, "ctx_city"),
            (&self.page_origin, "ctx_page_origin"),
            (&self.page_path, "ctx_page_path"),
        ] {
            if let Some(value) = value {
                conditions.push(format!("{column} = {}", params.text(value)));
            }
        }
        conditions.join(" AND ")
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The filters hourly rollups can answer: a date range, a video, and at
    /// most one first-start context value. Viewer, user, view and event
    /// filters always read events.
    pub fn rollup_scope(&self) -> Option<RollupScope> {
        if self.viewer_id.is_some()
            || self.user_id.is_some()
            || self.view_id.is_some()
            || self.event_name.is_some()
        {
            return None;
        }
        let mut dimensions = [
            (&self.browser_name, "context_browser_name"),
            (&self.os_name, "context_os_name"),
            (&self.device_type, "context_device_type"),
            (&self.continent, "ctx_continent"),
            (&self.country, "ctx_country"),
            (&self.city, "ctx_city"),
            (&self.page_origin, "ctx_page_origin"),
            (&self.page_path, "ctx_page_path"),
        ]
        .into_iter()
        .filter_map(|(value, column)| value.as_ref().map(|value| (column, value.clone())));
        let dimension = dimensions.next();
        if dimensions.next().is_some() {
            return None;
        }
        Some(RollupScope {
            from_ms: self.from_ms,
            to_ms: self.to_ms,
            video_id: self.video_id.clone(),
            dimension,
        })
    }
}

/// Filters a report can read from hourly rollups. See [`Filters::rollup_scope`].
pub struct RollupScope {
    pub from_ms: Option<i64>,
    pub to_ms: Option<i64>,
    pub video_id: Option<String>,
    /// A first-start context column and the value it must equal.
    pub dimension: Option<(&'static str, String)>,
}

impl Dimension {
    /// Cohort column for this dimension.
    pub fn column(self) -> &'static str {
        match self {
            Self::BrowserName => "context_browser_name",
            Self::OsName => "context_os_name",
            Self::DeviceType => "context_device_type",
            Self::Continent => "ctx_continent",
            Self::Country => "ctx_country",
            Self::City => "ctx_city",
            Self::PageOrigin => "ctx_page_origin",
            Self::PagePath => "ctx_page_path",
            Self::VideoId => "video_id",
            Self::ViewerId => "viewer_id",
            Self::UserId => "e_user_id",
        }
    }
}

impl BreakdownBy {
    pub fn column(self) -> &'static str {
        match self {
            Self::BrowserName => Dimension::BrowserName.column(),
            Self::OsName => Dimension::OsName.column(),
            Self::DeviceType => Dimension::DeviceType.column(),
            Self::Continent => Dimension::Continent.column(),
            Self::Country => Dimension::Country.column(),
            Self::City => Dimension::City.column(),
            Self::PageOrigin => Dimension::PageOrigin.column(),
            Self::PagePath => Dimension::PagePath.column(),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GroupPosition {
    pub views: i64,
    pub value: Option<String>,
}

impl GroupPosition {
    pub fn continuation(&self, column: &str, params: &mut crate::clickhouse::Params) -> String {
        let views = params.int(self.views);
        let tie = match &self.value {
            None => format!("{column} IS NOT NULL"),
            Some(value) => format!("{column} > {}", params.text(value)),
        };
        format!("(count() < {views} OR (count() = {views} AND {tie}))")
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
    /// The rollup refresh run a lagged first page read; later pages read it too.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollup: Option<i64>,
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
