use crate::{api::ApiError, event};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::BTreeMap;

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

/// Query parameters. Custom dimension filters (`custom.<key>`) have dynamic
/// names, so they are collected in one map whose deserializer rejects every
/// other unknown or repeated parameter.
#[derive(Default, Deserialize, Serialize)]
pub struct Query {
    #[serde(skip)]
    pub export: Option<std::sync::Arc<crate::exports::Sink>>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub video_id: Option<String>,
    pub viewer_id: Option<String>,
    pub user_id: Option<String>,
    pub video_title: Option<String>,
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
    pub subtitle_language: Option<String>,
    #[serde(default, deserialize_with = "optional_rate")]
    pub playback_rate: Option<String>,
    #[serde(default, deserialize_with = "optional_limit")]
    pub limit: Option<usize>,
    pub cursor: Option<String>,
    pub order: Option<SortOrder>,
    pub sort_by: Option<VideoSort>,
    pub group_by: Option<BreakdownBy>,
    /// `custom.<key>` filters. Any other unknown parameter is rejected here.
    #[serde(flatten, deserialize_with = "custom_filters")]
    pub custom: BTreeMap<String, String>,
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

/// A first-start value reports count views by: a stored column, one custom
/// key's value, or the custom keys in use. Written as its API name, such as
/// `country` or `custom.plan`.
#[derive(Clone, PartialEq)]
pub enum Dimension {
    /// API name and cohort column.
    Column(&'static str, &'static str),
    /// One custom key, validated like event keys.
    Custom(CustomKey),
    /// The custom keys set on views (`custom_keys`).
    CustomKeys,
}

/// A custom dimension key, only built from a validated name, so it is safe as
/// an SQL literal.
#[derive(Clone, PartialEq)]
pub struct CustomKey(String);

/// How reports can use a stored dimension.
#[derive(Clone, Copy, PartialEq)]
enum Grouping {
    /// Group by it from hourly rollups: few distinct values.
    Hourly,
    /// Group by it from per-view rows: many distinct values.
    PerView,
    /// Filter and list values only.
    FilterOnly,
}

/// Stored dimensions: API name, cohort column, and how reports group by it.
const DIMENSIONS: &[(&str, &str, Grouping)] = &[
    ("browser_name", "context_browser_name", Grouping::Hourly),
    ("os_name", "context_os_name", Grouping::Hourly),
    ("device_type", "context_device_type", Grouping::Hourly),
    ("continent", "ctx_continent", Grouping::Hourly),
    ("country", "ctx_country", Grouping::Hourly),
    ("city", "ctx_city", Grouping::Hourly),
    ("page_origin", "ctx_page_origin", Grouping::Hourly),
    ("page_path", "ctx_page_path", Grouping::Hourly),
    ("subtitle_language", "e_subtitle_language", Grouping::Hourly),
    ("playback_rate", "d_playback_rate", Grouping::Hourly),
    ("video_title", "e_video_title", Grouping::PerView),
    ("video_id", "video_id", Grouping::FilterOnly),
    ("viewer_id", "viewer_id", Grouping::FilterOnly),
    ("user_id", "e_user_id", Grouping::FilterOnly),
];

/// The cohort columns hourly rollups break down by.
pub fn hourly_columns() -> impl Iterator<Item = &'static str> {
    DIMENSIONS
        .iter()
        .filter(|(_, _, grouping)| *grouping == Grouping::Hourly)
        .map(|(_, column, _)| *column)
}

impl Dimension {
    /// A dimension by API name; `breakdown` accepts only the ones reports
    /// can group by (stored context, video titles and custom keys).
    fn parse(name: &str, breakdown: bool) -> Option<Self> {
        if let Some(key) = name.strip_prefix("custom.") {
            return event::custom_key(key).then(|| Self::Custom(CustomKey(key.to_owned())));
        }
        if name == "custom_keys" {
            return (!breakdown).then_some(Self::CustomKeys);
        }
        DIMENSIONS
            .iter()
            .find(|(api, _, grouping)| {
                *api == name && (*grouping != Grouping::FilterOnly || !breakdown)
            })
            .map(|(api, column, _)| Self::Column(api, column))
    }

    pub fn name(&self) -> String {
        match self {
            Self::Column(name, _) => (*name).to_owned(),
            Self::Custom(CustomKey(key)) => format!("custom.{key}"),
            Self::CustomKeys => "custom_keys".into(),
        }
    }

    /// The SQL value of this dimension on cohort and per-view rows. Custom
    /// keys match `[a-z0-9_]`, so they are safe as literals; views without
    /// the key form a null group.
    pub fn expression(&self) -> String {
        match self {
            Self::Column(_, column) => (*column).to_owned(),
            Self::Custom(CustomKey(key)) => {
                format!("if(mapContains(c_custom, '{key}'), c_custom['{key}'], NULL)")
            }
            Self::CustomKeys => "arrayJoin(mapKeys(c_custom))".into(),
        }
    }

    /// The first-start column the cohort must carry for this dimension.
    pub fn start_column(&self) -> &'static str {
        match self {
            Self::Column(_, column) => column,
            Self::Custom(_) | Self::CustomKeys => "c_custom",
        }
    }

    /// Whether hourly rollups hold this dimension.
    pub fn in_rollups(&self) -> bool {
        matches!(self, Self::Column(_, column) if hourly_columns().any(|hourly| hourly == *column))
    }
}

impl Serialize for Dimension {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.name())
    }
}
impl<'de> Deserialize<'de> for Dimension {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        Self::parse(&name, false)
            .ok_or_else(|| serde::de::Error::custom(format!("Unsupported dimension: {name}")))
    }
}

/// A dimension reports can group by: stored context, video title or a custom key.
#[derive(Clone, PartialEq)]
pub struct BreakdownBy(pub Dimension);

impl Serialize for BreakdownBy {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        self.0.serialize(s)
    }
}
impl<'de> Deserialize<'de> for BreakdownBy {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let name = String::deserialize(d)?;
        Dimension::parse(&name, true)
            .map(Self)
            .ok_or_else(|| serde::de::Error::custom(format!("Unsupported group_by: {name}")))
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
    pub video_title: Option<String>,
    pub view_id: Option<String>,
    pub browser_name: Option<String>,
    pub os_name: Option<String>,
    pub device_type: Option<String>,
    pub continent: Option<String>,
    pub country: Option<String>,
    pub city: Option<String>,
    pub page_origin: Option<String>,
    pub page_path: Option<String>,
    pub subtitle_language: Option<String>,
    #[serde(default, deserialize_with = "optional_rate")]
    pub playback_rate: Option<String>,
    /// `custom.<key>` filters, validated like [`Query::custom`].
    #[serde(flatten, deserialize_with = "custom_filters")]
    pub custom: BTreeMap<String, String>,
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
            video_title: self.video_title,
            view_id: self.view_id,
            browser_name: self.browser_name,
            os_name: self.os_name,
            device_type: self.device_type,
            continent: self.continent,
            country: self.country,
            city: self.city,
            page_origin: self.page_origin,
            page_path: self.page_path,
            subtitle_language: self.subtitle_language,
            playback_rate: self.playback_rate,
            custom: self.custom,
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
    video_title: Option<String>,
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
    subtitle_language: Option<String>,
    playback_rate: Option<String>,
    /// Custom dimension filters by key, without the `custom.` prefix.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    custom: BTreeMap<String, String>,
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
            .subtitle_language
            .as_deref()
            .is_some_and(|value| !event::language_tag(value))
        {
            return Err(ApiError::bad(format!(
                "subtitle_language must contain between 1 and {} characters and not be blank",
                event::MAX_LANGUAGE_CHARS
            )));
        }
        for (name, value) in [
            ("user_id", &self.user_id),
            ("video_title", &self.video_title),
        ] {
            if value
                .as_ref()
                .is_some_and(|value| !(1..=event::MAX_LABEL_CHARS).contains(&value.chars().count()))
            {
                return Err(ApiError::bad(format!(
                    "{name} must contain between 1 and {} characters",
                    event::MAX_LABEL_CHARS
                )));
            }
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
        // Keys were checked when the query was read; queries built in code
        // are checked here too.
        let mut custom = BTreeMap::new();
        for (name, value) in &self.custom {
            let key = name
                .strip_prefix("custom.")
                .filter(|key| event::custom_key(key))
                .ok_or_else(|| ApiError::bad(format!("Unsupported query parameter: {name}")))?;
            if !event::custom_text(value) {
                return Err(ApiError::bad(format!(
                    "{name} must contain between 1 and {} characters",
                    event::MAX_LABEL_CHARS
                )));
            }
            custom.insert(key.to_owned(), value.clone());
        }
        if custom.len() > event::MAX_CUSTOM_KEYS {
            return Err(ApiError::bad(format!(
                "At most {} custom filters are allowed",
                event::MAX_CUSTOM_KEYS
            )));
        }
        Ok(Filters {
            from_ms,
            to_ms,
            video_id: self.video_id.clone(),
            viewer_id: self.viewer_id.clone(),
            user_id: self.user_id.clone(),
            video_title: self.video_title.clone(),
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
            subtitle_language: self.subtitle_language.clone(),
            playback_rate: self.playback_rate.clone(),
            custom,
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

/// The parameters a query does not name: only `custom.<key>` filters, so a
/// misspelled parameter fails when the query is read, whatever the report.
fn custom_filters<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> Result<BTreeMap<String, String>, D::Error> {
    struct Filters;
    impl<'de> serde::de::Visitor<'de> for Filters {
        type Value = BTreeMap<String, String>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("custom.<key> filters")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut filters = BTreeMap::new();
            while let Some((name, value)) = map.next_entry::<String, String>()? {
                if !name.strip_prefix("custom.").is_some_and(event::custom_key) {
                    return Err(serde::de::Error::custom(format!(
                        "Unsupported query parameter: {name}"
                    )));
                }
                if filters.insert(name.clone(), value).is_some() {
                    return Err(serde::de::Error::custom(format!(
                        "Repeated query parameter: {name}"
                    )));
                }
            }
            Ok(filters)
        }
    }
    d.deserialize_map(Filters)
}

/// The `playback_rate` query parameter as its dimension text, normalized when
/// the query is read so filters and caches share one value per rate: digits
/// with at most two decimals, from 0.01 to 16, written like ClickHouse writes
/// it (`2`, `1.5`). `2.0` and `2` select the same views.
fn optional_rate<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Input {
        Number(f64),
        Text(String),
    }
    let invalid = || {
        serde::de::Error::custom(format!(
            "playback_rate must be a number from {} to {} with at most two decimals",
            event::MIN_PLAYBACK_RATE,
            event::MAX_PLAYBACK_RATE
        ))
    };
    let Some(input) = Option::<Input>::deserialize(d)? else {
        return Ok(None);
    };
    let rate = match input {
        Input::Number(rate) => rate,
        Input::Text(text) => {
            let (whole, decimals) = text.split_once('.').unwrap_or((&text, "0"));
            let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
            if !digits(whole) || !digits(decimals) || decimals.len() > 2 {
                return Err(invalid());
            }
            text.parse().map_err(|_| invalid())?
        }
    };
    if !(event::MIN_PLAYBACK_RATE..=event::MAX_PLAYBACK_RATE).contains(&rate)
        || (rate * 100.0).round() / 100.0 != rate
    {
        return Err(invalid());
    }
    Ok(Some(rate.to_string()))
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
            (&self.video_title, "e_video_title"),
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
            (&self.subtitle_language, "e_subtitle_language"),
            (&self.playback_rate, "d_playback_rate"),
        ] {
            if let Some(value) = value {
                conditions.push(format!("{column} = {}", params.text(value)));
            }
        }
        // Values are never empty, so a view without the key never matches.
        for (key, value) in &self.custom {
            conditions.push(format!(
                "c_custom[{}] = {}",
                params.text(key),
                params.text(value)
            ));
        }
        conditions.join(" AND ")
    }

    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    /// The filters hourly rollups can answer: a date range, a video, and at
    /// most one first-start context value. Viewer, view and event filters
    /// read events; user, title and custom filters read per-view rows (see
    /// [`Filters::views_range`]).
    pub fn rollup_scope(&self) -> Option<RollupScope> {
        if self.viewer_id.is_some()
            || self.needs_views()
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
            (&self.subtitle_language, "e_subtitle_language"),
            (&self.playback_rate, "d_playback_rate"),
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

impl Filters {
    /// Whether these filters use first-start values with many distinct
    /// values, kept per view rather than in hourly rollups: `user_id`,
    /// `video_title` and custom dimensions.
    pub fn needs_views(&self) -> bool {
        self.user_id.is_some() || self.video_title.is_some() || !self.custom.is_empty()
    }

    /// The date range of per-view rollup rows these filters can read, or
    /// `None` when they need events: viewer, view and event filters do.
    pub fn views_range(&self) -> Option<(Option<i64>, Option<i64>)> {
        if self.viewer_id.is_some() || self.view_id.is_some() || self.event_name.is_some() {
            return None;
        }
        Some((self.from_ms, self.to_ms))
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

#[cfg(test)]
mod tests {
    use super::{BreakdownBy, Dimension, Query};
    use serde_json::json;

    fn query(fields: serde_json::Value) -> Query {
        serde_json::from_value(fields).unwrap()
    }

    #[test]
    fn custom_filters_have_named_keys_and_other_parameters_are_rejected() {
        let filters = query(json!({"custom.plan": "premium", "custom.ab_test": "B"}))
            .filters()
            .unwrap();
        assert!(filters.needs_views());
        assert!(filters.rollup_scope().is_none());
        let mut params = crate::clickhouse::Params::default();
        assert_eq!(
            filters.clickhouse(&mut params).matches("c_custom[").count(),
            2
        );
        for invalid in [
            json!({"countyr": "FR"}),
            json!({"custom.": "x"}),
            json!({"custom.Plan": "x"}),
            json!({"custom.plan.tier": "x"}),
        ] {
            assert!(
                serde_json::from_value::<Query>(invalid.clone()).is_err(),
                "{invalid}"
            );
        }
        for invalid in [
            json!({"custom.plan": ""}),
            json!({"custom.plan": "x".repeat(257)}),
        ] {
            assert!(query(invalid.clone()).filters().is_err(), "{invalid}");
        }
        let eleven: serde_json::Map<_, _> = (0..11)
            .map(|n| (format!("custom.k{n}"), json!("x")))
            .collect();
        assert!(query(serde_json::Value::Object(eleven)).filters().is_err());
        // Viewer filters still need events; user IDs read per-view rows.
        let user = query(json!({"user_id": "u"})).filters().unwrap();
        assert!(user.needs_views() && user.views_range().is_some());
        let viewer = query(json!({"custom.plan": "x", "viewer_id": "v"}))
            .filters()
            .unwrap();
        assert!(viewer.views_range().is_none());
    }

    #[test]
    fn dimensions_round_trip_by_name() {
        for name in [
            "country",
            "custom.plan",
            "custom_keys",
            "user_id",
            "video_title",
            "playback_rate",
        ] {
            let dimension: Dimension = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(serde_json::to_value(&dimension).unwrap(), json!(name));
        }
        for name in [
            "custom.plan",
            "page_path",
            "subtitle_language",
            "video_title",
        ] {
            let group: BreakdownBy = serde_json::from_value(json!(name)).unwrap();
            assert_eq!(serde_json::to_value(&group).unwrap(), json!(name));
        }
        for name in [
            "custom_keys",
            "user_id",
            "video_id",
            "custom.Plan",
            "custom.",
        ] {
            assert!(
                serde_json::from_value::<BreakdownBy>(json!(name)).is_err(),
                "{name}"
            );
        }
        let custom: Dimension = serde_json::from_value(json!("custom.plan")).unwrap();
        assert!(!custom.in_rollups());
        assert_eq!(custom.start_column(), "c_custom");
        let title: BreakdownBy = serde_json::from_value(json!("video_title")).unwrap();
        assert!(!title.0.in_rollups());
        assert_eq!(title.0.start_column(), "e_video_title");
        let country: BreakdownBy = serde_json::from_value(json!("country")).unwrap();
        assert!(country.0.in_rollups());
    }
}
