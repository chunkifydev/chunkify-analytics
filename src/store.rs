//! The analytics store shared by HTTP, MCP and CSV exports.
use crate::{api::ApiError, query::Query};

pub type Store = std::sync::Arc<crate::clickhouse::Store>;

/// Retention reports need a video and accept only audience and date filters.
pub(crate) fn validate_retention_query(query: &Query) -> Result<(), ApiError> {
    if query.video_id.is_none()
        || query.event_name.is_some()
        || query.group_by.is_some()
        || query.sort_by.is_some()
        || query.order.is_some()
        || query.cursor.is_some()
        || query.limit.is_some()
    {
        return Err(ApiError::bad(
            "Retention requires video_id and accepts only audience and date filters",
        ));
    }
    Ok(())
}
