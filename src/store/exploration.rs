use super::metrics::{
    CohortMetrics, score_json, utc_timestamp, view_cohort, view_cohort_with_labels,
};
use super::*;
use crate::query::{Dimension, GroupPosition, ViewsQuery};

// Every column is named; rows are read by name, never by position.
const VIEW_COLUMNS: &str = "view_id, video_id, viewer_id, e_user_id AS user_id,
    e_video_title AS video_title, occurred_at_ms,
    context_browser_name, context_os_name, context_device_type, playing_time_ms,
    measurement.startup_time_ms AS startup_time_ms,
    measurement.rebuffer_duration_ms AS rebuffer_duration_ms,
    COALESCE(measurement.reached_end, FALSE) AS reached_end,
    COALESCE(measurement.has_ended_event, FALSE) AS has_ended_event,
    COALESCE(has_fatal_error, FALSE) AS has_fatal_error, playback_score";

fn view_row(row: &duckdb::Row<'_>) -> duckdb::Result<Value> {
    let score: Option<f64> = row.get("playback_score")?;
    Ok(json!({
        "view_id": row.get::<_, String>("view_id")?,
        "video_id": row.get::<_, String>("video_id")?,
        "video_title": row.get::<_, Option<String>>("video_title")?,
        "viewer_id": row.get::<_, Option<String>>("viewer_id")?,
        "user_id": row.get::<_, Option<String>>("user_id")?,
        "started_at": utc_timestamp(row.get("occurred_at_ms")?),
        "browser_name": row.get::<_, Option<String>>("context_browser_name")?,
        "os_name": row.get::<_, Option<String>>("context_os_name")?,
        "device_type": row.get::<_, Option<String>>("context_device_type")?,
        "playing_time_ms": row.get::<_, f64>("playing_time_ms")?,
        "startup_time_ms": row.get::<_, Option<f64>>("startup_time_ms")?,
        "rebuffer_duration_ms": row.get::<_, Option<f64>>("rebuffer_duration_ms")?,
        "reached_end": row.get::<_, bool>("reached_end")?,
        "has_ended_event": row.get::<_, bool>("has_ended_event")?,
        "has_fatal_error": row.get::<_, bool>("has_fatal_error")?,
        "playback_score": score_json(score, i64::from(score.is_some())),
    }))
}

fn validate(query: &Query) -> Result<(), ApiError> {
    if query.event_name.is_some() || query.group_by.is_some() || query.sort_by.is_some() {
        return Err(ApiError::bad("Unsupported exploration query parameter"));
    }
    Ok(())
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ViewPosition {
    id: String,
    value: Option<ViewSortValue>,
}
#[derive(serde::Serialize, serde::Deserialize)]
enum ViewSortValue {
    Text(String),
    Number(f64),
}

impl Store {
    pub async fn views(&self, project: String, input: ViewsQuery) -> Result<Value, ApiError> {
        let query = input.query;
        validate(&query)?;
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let order = query.order.unwrap_or(SortOrder::Desc);
        let sort = input.sort_by;
        let column = sort.column();
        let kind = format!(
            "views_{}_{}_{:?}",
            column,
            order.as_str(),
            input.has_fatal_error
        );
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| Cursor::<ViewPosition>::decode(c, &kind, &filters, &project))
            .transpose()?;
        if cursor.as_ref().is_some_and(|c| match &c.after.value {
            Some(ViewSortValue::Text(value)) => !sort.is_text() || value.len() > 16384,
            Some(ViewSortValue::Number(value)) => sort.is_text() || !value.is_finite(),
            None => false,
        }) {
            return Err(ApiError::bad("Invalid view cursor position"));
        }
        self.read("views", project.clone(), move |db| {
            let through = match &cursor { Some(c) => c.through, None => watermark(db, &project)? };
            let (sql, mut values) = view_cohort(project.clone(), through, &filters, CohortMetrics::Complete);
            let (direction, comparison) = match order { SortOrder::Asc => ("ASC", ">"), SortOrder::Desc => ("DESC", "<") };
            let mut conditions = Vec::new();
            if let Some(c) = cursor {
                if let Some(value) = c.after.value {
                    conditions.push(format!("({column} {comparison} ? OR ({column} = ? AND view_id {comparison} ?) OR {column} IS NULL)"));
                    let value = match value { ViewSortValue::Text(s) => SqlValue::Text(s), ViewSortValue::Number(n) => SqlValue::Double(n) };
                    values.extend([value.clone(), value, SqlValue::Text(c.after.id)]);
                } else {
                    conditions.push(format!("({column} IS NULL AND view_id {comparison} ?)"));
                    values.push(SqlValue::Text(c.after.id));
                }
            }
            if let Some(fatal) = input.has_fatal_error { conditions.push("COALESCE(has_fatal_error, FALSE) = ?".into()); values.push(SqlValue::Boolean(fatal)); }
            values.push(SqlValue::BigInt((limit + 1) as i64));
            let conditions = if conditions.is_empty() { "TRUE".into() } else { conditions.join(" AND ") };
            let projection = if sort.is_text() { column.to_owned() } else { format!("{column}::DOUBLE") };
            let mut statement = db.prepare(&format!("{sql} SELECT {VIEW_COLUMNS}, {projection} AS sort_key FROM view_metrics WHERE {conditions} ORDER BY {column} {direction} NULLS LAST, view_id {direction} LIMIT ?"))?;
            let rows = query_rows(&mut statement,&values,export.is_some())?.mapped(|row| {
                let value = if sort.is_text() { row.get::<_, Option<String>>("sort_key")?.map(ViewSortValue::Text) }
                    else { row.get::<_, Option<f64>>("sort_key")?.map(ViewSortValue::Number) };
                Ok((view_row(row)?, value))
            });
            let mut views = Vec::new(); let mut next_cursor = None; let mut last = None;
            for row in rows {
                if views.len() == limit {
                    next_cursor = Some(Cursor { version:1, project_id:project.clone(), kind:kind.clone(),
                        after:last.expect("nonempty page"), through, filters:filters.clone() }.encode()?);
                    break;
                }
                let (view, value) = row?;
                last = Some(ViewPosition { id: view["view_id"].as_str().unwrap().to_owned(), value });
                crate::exports::emit(&export,&mut views,view)?;
            }
            Ok(json!({"views":views,"next_cursor":next_cursor}))
        }).await
    }

    pub async fn view(
        &self,
        project: String,
        view_id: String,
        mut query: Query,
    ) -> Result<Value, ApiError> {
        // A detail page accepts only timeline pagination. Its identity comes from the path.
        if query.from.is_some()
            || query.to.is_some()
            || query.video_id.is_some()
            || query.viewer_id.is_some()
            || query.user_id.is_some()
            || query.view_id.is_some()
            || query.browser_name.is_some()
            || query.os_name.is_some()
            || query.device_type.is_some()
            || query.continent.is_some()
            || query.country.is_some()
            || query.city.is_some()
            || query.page_origin.is_some()
            || query.page_path.is_some()
            || query.order.is_some()
        {
            return Err(ApiError::bad("View detail accepts only limit and cursor"));
        }
        validate(&query)?;
        query.view_id = Some(view_id);
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| Cursor::<i64>::decode(c, "view_events", &filters, &project))
            .transpose()?;
        if cursor
            .as_ref()
            .is_some_and(|c| c.after < 0 || c.after > c.through)
        {
            return Err(ApiError::bad("Invalid cursor position"));
        }
        let remote = self.remote.is_some();
        self.read("view", project.clone(), move |db| {
            let through = match &cursor {
                Some(c) => c.through,
                None => watermark(db, &project)?,
            };
            let (sql, values) =
                view_cohort(project.clone(), through, &filters, CohortMetrics::Complete);
            let mut statement =
                db.prepare(&format!("{sql} SELECT {VIEW_COLUMNS} FROM view_metrics"))?;
            let view = statement
                .query_map(params_from_iter(values.iter()), view_row)?
                .next()
                .transpose()?
                .ok_or_else(|| {
                    ApiError::new(
                        axum::http::StatusCode::NOT_FOUND,
                        "View has no retained start".into(),
                    )
                })?;
            let after = cursor.as_ref().map_or(0, |c| c.after);
            let (conditions, mut values) = filters.sql();
            let conditions = format!("({conditions}) AND project_id = ?");
            values.extend([
                SqlValue::Text(project.clone()),
                SqlValue::BigInt(after),
                SqlValue::BigInt(through),
                SqlValue::BigInt((limit + 1) as i64),
            ]);
            let mut statement = db.prepare(&(if remote {
                crate::storage::parquet_page_sql
            } else {
                crate::storage::page_sql
            })(&conditions, false))?;
            let mut rows = statement.query(params_from_iter(values.iter()))?;
            let mut events = Vec::new();
            let mut last = after;
            let mut next_cursor = None;
            while let Some(row) = rows.next()? {
                if events.len() == limit {
                    next_cursor = Some(
                        Cursor {
                            version: 1,
                            project_id: project.clone(),
                            kind: "view_events".into(),
                            after: last,
                            through,
                            filters: filters.clone(),
                        }
                        .encode()?,
                    );
                    break;
                }
                let (id, record) = crate::storage::decode(row)?;
                last = id;
                events.push(record);
            }
            Ok(json!({"view":view,"events":events,"next_cursor":next_cursor}))
        })
        .await
    }

    pub async fn dimension(
        &self,
        project: String,
        dimension: Dimension,
        query: Query,
    ) -> Result<Value, ApiError> {
        validate(&query)?;
        if query.order.is_some() {
            return Err(ApiError::bad("Dimension values sort by views descending"));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let kind = format!("dimension_{}_views_desc", dimension.column());
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| Cursor::<GroupPosition>::decode(c, &kind, &filters, &project))
            .transpose()?;
        self.read_report("dimension", project.clone(), (&query, dimension.column()), export.is_some(), move |db| {
            let through = match &cursor { Some(c) => c.through, None => watermark(db, &project)? };
            let (sql, mut values) = view_cohort_with_labels(project.clone(), through, &filters, CohortMetrics::Basic, matches!(dimension, Dimension::UserId));
            let column = dimension.column();
            let continuation = match cursor { None => "TRUE".into(), Some(c) => c.after.continuation(column, &mut values) };
            values.push(SqlValue::BigInt((limit + 1) as i64));
            let mut statement = db.prepare(&format!("{sql} SELECT {column}, COUNT(*) FROM view_metrics GROUP BY {column} HAVING {continuation} ORDER BY COUNT(*) DESC, {column} ASC NULLS FIRST LIMIT ?"))?;
            let rows = query_rows(&mut statement,&values,export.is_some())?.mapped(|r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)));
            let mut rows_out = Vec::new(); let mut last = None; let mut next_cursor = None;
            for row in rows {
                if rows_out.len() == limit {
                    next_cursor = Some(Cursor {version:1, project_id:project.clone(), kind:kind.clone(), after:last.expect("nonempty page"), through, filters:filters.clone()}.encode()?); break;
                }
                let (value, views) = row?; last = Some(GroupPosition {views, value:value.clone()}); crate::exports::emit(&export,&mut rows_out,json!({"value":value,"views":views}))?;
            }
            Ok(json!({"dimension":dimension,"values":rows_out,"next_cursor":next_cursor}))
        }).await
    }

    pub async fn errors(&self, project: String, query: Query) -> Result<Value, ApiError> {
        validate(&query)?;
        if query.order.is_some() {
            return Err(ApiError::bad(
                "Error groups sort by affected views descending",
            ));
        }
        let export = query.export.clone();
        let filters = query.filters()?;
        let limit = query.limit()?;
        let cursor = query
            .cursor
            .as_deref()
            .map(|c| {
                Cursor::<(i64, String, String)>::decode(c, "errors_views_desc", &filters, &project)
            })
            .transpose()?;
        self.read_report("errors", project.clone(), &query, export.is_some(), move |db| {
            let through = match &cursor { Some(c) => c.through, None => watermark(db, &project)? };
            let (sql, mut values) = view_cohort(project.clone(), through, &filters, CohortMetrics::Basic);
            values.extend([SqlValue::Text(project.clone()), SqlValue::BigInt(through)]);
            let continuation: String = match cursor { None => "TRUE".into(), Some(c) => {
                values.extend([SqlValue::BigInt(c.after.0), SqlValue::BigInt(c.after.0), SqlValue::Text(c.after.1.clone()), SqlValue::Text(c.after.1), SqlValue::Text(c.after.2)]);
                "(affected_views < ? OR (affected_views = ? AND (code > ? OR (code = ? AND fatal > ?))))".into()
            }};
            values.push(SqlValue::BigInt((limit + 1) as i64));
            let mut statement = db.prepare(&format!("{sql}, error_groups AS (
                SELECT COALESCE(CASE WHEN json_type(e.event_remainder, '$.error_code') IN ('VARCHAR','BIGINT','UBIGINT','DOUBLE')
                    THEN CAST(json_extract(e.event_remainder, '$.error_code') AS VARCHAR) END, 'null') AS code,
                    COALESCE(CAST(e.e_error_fatal AS VARCHAR), 'null') AS fatal,
                    COUNT(DISTINCT e.event_id) AS error_events, COUNT(DISTINCT e.view_id) AS affected_views
                FROM events e JOIN view_metrics v ON e.view_id = v.view_id AND e.video_id = v.video_id
                    AND e.viewer_id IS NOT DISTINCT FROM v.viewer_id
                WHERE e.project_id = ? AND e.ingest_id <= ? AND e.event_name = 'chunkify.video.playback_error'
                GROUP BY code, fatal)
                SELECT code, fatal, error_events, affected_views FROM error_groups WHERE {continuation} ORDER BY affected_views DESC, code, fatal LIMIT ?"))?;
            let rows = query_rows(&mut statement,&values,export.is_some())?.mapped(|r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, i64>(2)?, r.get::<_, i64>(3)?)));
            let mut errors = Vec::new(); let mut last = None; let mut next_cursor = None;
            for row in rows {
                if errors.len() == limit { next_cursor = Some(Cursor {version:1, project_id:project.clone(), kind:"errors_views_desc".into(), after:last.expect("nonempty page"), through, filters:filters.clone()}.encode()?); break; }
                let (code, fatal, count, views) = row?;
                let code_value: Value = serde_json::from_str(&code).map_err(|_| ApiError::internal())?;
                let fatal_value: Value = serde_json::from_str(&fatal).map_err(|_| ApiError::internal())?;
                last = Some((views, code, fatal)); crate::exports::emit(&export,&mut errors,json!({"error_code":code_value,"fatal":fatal_value,"error_events":count,"affected_views":views}))?;
            }
            Ok(json!({"errors":errors,"next_cursor":next_cursor}))
        }).await
    }
}
