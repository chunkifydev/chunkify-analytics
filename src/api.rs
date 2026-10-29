use crate::{
    config::Config,
    context::RequestContext,
    event,
    projects::{Permission, RateLimit, Registry, Resolved},
    store::Store,
};
use axum::{
    Json, Router,
    body::to_bytes,
    extract::{ConnectInfo, Extension, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, patch, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct App {
    store: Store,
    exports: crate::exports::Exports,
    projects: Arc<Registry>,
    admin: Arc<crate::admin::Controller>,
    max_body_bytes: usize,
    store_ip: bool,
    trusted_proxies: Vec<ipnet::IpNet>,
    mcp_enabled: bool,
    public_url: Option<reqwest::Url>,
    rate: Arc<Mutex<RateLimit>>,
}

#[derive(Debug)]
pub struct ApiError {
    pub status: StatusCode,
    pub message: String,
    pub retry_after: Option<u64>,
}
impl ApiError {
    pub fn new(status: StatusCode, message: String) -> Self {
        Self {
            status,
            message,
            retry_after: None,
        }
    }
    pub fn bad(message: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, message.into())
    }
    pub fn unavailable() -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Collector is busy or unavailable".into(),
        )
    }
    pub fn internal() -> Self {
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Storage operation failed".into(),
        )
    }
    pub fn limited(message: &str, delay: std::time::Duration) -> Self {
        Self {
            status: StatusCode::TOO_MANY_REQUESTS,
            message: message.into(),
            retry_after: Some((delay.as_secs() + u64::from(delay.subsec_nanos() > 0)).max(1)),
        }
    }
}
impl From<duckdb::Error> for ApiError {
    fn from(_: duckdb::Error) -> Self {
        Self::internal()
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.status, Json(json!({"error": self.message}))).into_response();
        response
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
        if self.status == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert("www-authenticate", HeaderValue::from_static("Bearer"));
        }
        if self.status == StatusCode::TOO_MANY_REQUESTS {
            response.headers_mut().insert(
                "retry-after",
                HeaderValue::from_str(&self.retry_after.unwrap_or(1).to_string())
                    .expect("numeric retry delay"),
            );
        }
        response
    }
}

pub fn router(
    store: Store,
    projects: Arc<Registry>,
    config: &Config,
    admin: Arc<crate::admin::Controller>,
) -> Router {
    let app = App {
        exports: crate::exports::Exports::new(store.clone(), projects.clone(), &config.rate_limits),
        store,
        projects,
        admin,
        max_body_bytes: config.max_body_bytes,
        store_ip: config.store_ip,
        trusted_proxies: config.trusted_proxies.clone(),
        mcp_enabled: config.mcp_enabled,
        public_url: config.public_url.clone(),
        rate: Arc::new(Mutex::new(RateLimit::new(config.ingest_per_second))),
    };
    let mcp = if config.mcp_enabled {
        crate::mcp::router(
            app.store.clone(),
            app.exports.clone(),
            config.mcp_allowed_hosts.clone(),
        )
    } else {
        Router::new()
    };
    Router::new()
        .merge(mcp)
        .route("/healthz", get(health))
        .route("/v1/projects", post(create_project))
        .route("/v1/projects/{project_id}/keys", get(admin_project_keys))
        .route(
            "/v1/projects/{project_id}/keys/{key_id}/rotate",
            post(admin_rotate_project_key),
        )
        .route("/v1/project", patch(update_project).delete(delete_project))
        .route(
            "/v1/project/keys",
            get(project_keys).post(create_project_key),
        )
        .route("/v1/project/keys/{key_id}/rotate", post(rotate_project_key))
        .route("/v1/summary", get(summary))
        .route("/v1/quality", get(quality))
        .route("/v1/renditions", get(renditions))
        .route("/v1/engagement", get(engagement))
        .route("/v1/timeseries", get(timeseries))
        .route("/v1/videos", get(videos))
        .route("/v1/views", get(views))
        .route("/v1/views/{view_id}", get(view))
        .route("/v1/errors", get(errors))
        .route("/v1/dimensions/{dimension}", get(dimension))
        .route("/v1/exports", post(create_export))
        .route(
            "/v1/exports/{token}",
            get(download_export).head(export_head),
        )
        .route(
            "/v1/events",
            get(events)
                .post(ingest)
                .options(|| async { StatusCode::NO_CONTENT }),
        )
        .route_layer(middleware::from_fn_with_state(app.clone(), access))
        .with_state(app)
}

async fn access(
    State(app): State<App>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let mcp = app.mcp_enabled && request.uri().path() == "/mcp";
    let ingest = request.uri().path() == "/v1/events"
        && matches!(*request.method(), Method::POST | Method::OPTIONS);
    let download = matches!(*request.method(), Method::GET | Method::HEAD)
        && request.uri().path().starts_with("/v1/exports/");
    let resolved = if download {
        None
    } else if request.uri().path() == "/v1/projects"
        || request.uri().path().starts_with("/v1/projects/")
    {
        app.admin.authorize_admin(&bearer(request.headers()))?;
        None
    } else if ingest && request.method() != Method::OPTIONS {
        Some(
            app.projects.authorize(
                request
                    .headers()
                    .get("x-chunkify-ingest-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                Permission::Public,
            )?,
        )
    } else if !ingest && (request.uri().path().starts_with("/v1/") || mcp) {
        let secret = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        let resolved = app.projects.authorize(secret, Permission::Secret)?;
        let analytics_read = matches!(*request.method(), Method::GET | Method::HEAD)
            && matches!(
                request.uri().path(),
                "/v1/events"
                    | "/v1/summary"
                    | "/v1/videos"
                    | "/v1/timeseries"
                    | "/v1/quality"
                    | "/v1/renditions"
                    | "/v1/engagement"
                    | "/v1/views"
                    | "/v1/errors"
            )
            || matches!(*request.method(), Method::GET | Method::HEAD)
                && ["/v1/views/", "/v1/dimensions/"]
                    .iter()
                    .any(|prefix| request.uri().path().starts_with(prefix))
            || *request.method() == Method::POST && request.uri().path() == "/v1/exports";
        if !mcp && !analytics_read {
            resolved.require_write()?;
        }
        Some(resolved)
    } else {
        None
    };
    let origin = if ingest || mcp {
        match request.headers().get("origin") {
            Some(origin)
                if origin.to_str().is_ok_and(|s| {
                    resolved.as_ref().map_or_else(
                        || app.projects.allows_origin(s),
                        |r| r.project.allows_origin(s),
                    )
                }) =>
            {
                Some(origin.clone())
            }
            Some(_) => {
                return Err(ApiError::new(
                    StatusCode::FORBIDDEN,
                    "Origin is not allowed".into(),
                ));
            }
            None => None,
        }
    } else {
        None
    };
    if ingest && request.method() == Method::OPTIONS {
        let method = request
            .headers()
            .get("access-control-request-method")
            .and_then(|v| v.to_str().ok());
        let headers_ok = request
            .headers()
            .get("access-control-request-headers")
            .is_none_or(|v| {
                v.to_str().is_ok_and(|s| {
                    s.split(',').all(|h| {
                        matches!(
                            h.trim().to_ascii_lowercase().as_str(),
                            "content-type" | "content-encoding" | "x-chunkify-ingest-key"
                        )
                    })
                })
            });
        if origin.is_none() || method != Some("POST") || !headers_ok {
            return Err(ApiError::new(
                StatusCode::FORBIDDEN,
                "Unsupported preflight request".into(),
            ));
        }
    } else if ingest {
        resolved
            .as_ref()
            .expect("authenticated ingest")
            .project
            .rate
            .lock()
            .map_err(|_| ApiError::unavailable())?
            .take()?;
        app.rate
            .lock()
            .map_err(|_| ApiError::unavailable())?
            .take()?;
    }
    if mcp {
        let base = public_base(&app, &request)?;
        request
            .extensions_mut()
            .insert(crate::exports::DownloadBase(base));
    }
    if let Some(resolved) = resolved {
        request.extensions_mut().insert(resolved);
    }
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    if let Some(origin) = origin {
        response
            .headers_mut()
            .insert("access-control-allow-origin", origin);
        response
            .headers_mut()
            .insert("vary", HeaderValue::from_static("Origin"));
        response.headers_mut().insert(
            "access-control-allow-methods",
            HeaderValue::from_static("POST"),
        );
        response.headers_mut().insert(
            "access-control-allow-headers",
            HeaderValue::from_static("Content-Type, Content-Encoding, X-Chunkify-Ingest-Key"),
        );
    }
    Ok(response)
}

async fn ingest(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Result<Response, ApiError> {
    let resolved = request
        .extensions()
        .get::<Resolved>()
        .cloned()
        .ok_or_else(ApiError::internal)?;
    let context = app
        .store_ip
        .then(|| RequestContext::capture(peer.ip(), request.headers(), &app.trusted_proxies));
    let event = read_json(request, app.max_body_bytes).await?;
    event::validate(&event)?;
    let inserted = app
        .store
        .insert(&resolved.project, &resolved.key_id, event, context)
        .await?
        > 0;
    Ok((
        if inserted {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(json!({"accepted": inserted, "duplicate": !inserted})),
    )
        .into_response())
}

async fn read_json(request: Request, limit: usize) -> Result<Value, ApiError> {
    if !request
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| {
            v.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .eq_ignore_ascii_case("application/json")
        })
    {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected application/json".into(),
        ));
    }
    if request
        .headers()
        .get("content-encoding")
        .is_some_and(|value| value != "identity")
    {
        return Err(ApiError::new(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Unsupported Content-Encoding".into(),
        ));
    }
    let bytes = to_bytes(request.into_body(), limit).await.map_err(|_| {
        ApiError::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body exceeds limit or could not be read".into(),
        )
    })?;
    tokio::task::spawn_blocking(move || {
        serde_json::from_slice(&bytes).map_err(|_| ApiError::bad("Malformed JSON"))
    })
    .await
    .map_err(|_| ApiError::unavailable())?
}

fn bearer(headers: &HeaderMap) -> String {
    headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .unwrap_or("")
        .to_owned()
}

async fn project_action(
    app: App,
    token: String,
    action: crate::admin::ProjectAction,
) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || app.admin.project(&token, action))
        .await
        .map_err(|_| ApiError::unavailable())?
        .map(Json)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProject {
    name: String,
    #[serde(default)]
    allowed_origins: Vec<String>,
    retention_seconds: Option<u64>,
}

async fn create_project(
    State(app): State<App>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let token = bearer(request.headers());
    let body: NewProject = serde_json::from_value(read_json(request, app.max_body_bytes).await?)
        .map_err(|_| {
            ApiError::bad("Expected a name and optional allowed_origins and retention_seconds")
        })?;
    crate::projects::validate_name(&body.name).map_err(ApiError::bad)?;
    let mut origins = body.allowed_origins;
    crate::projects::normalize_origins(&mut origins).map_err(ApiError::bad)?;
    let result = tokio::task::spawn_blocking(move || {
        app.admin
            .create_project(&token, body.name, origins, body.retention_seconds)
    })
    .await
    .map_err(|_| ApiError::unavailable())??;
    Ok((StatusCode::CREATED, Json(result)))
}

async fn update_project(State(app): State<App>, request: Request) -> Result<Json<Value>, ApiError> {
    let token = bearer(request.headers());
    let mut update: crate::projects::Update =
        serde_json::from_value(read_json(request, app.max_body_bytes).await?).map_err(|_| {
            ApiError::bad("Expected optional name, allowed_origins and retention_seconds")
        })?;
    update.validate().map_err(ApiError::bad)?;
    project_action(app, token, crate::admin::ProjectAction::Update(update)).await
}

async fn delete_project(State(app): State<App>, request: Request) -> Result<Json<Value>, ApiError> {
    let token = bearer(request.headers());
    if request.uri().query().is_some_and(|query| !query.is_empty()) {
        return Err(ApiError::bad(
            "Project deletion does not accept query parameters",
        ));
    }
    to_bytes(request.into_body(), 0)
        .await
        .map_err(|_| ApiError::bad("Project deletion does not accept a request body"))?;
    project_action(app, token, crate::admin::ProjectAction::Delete).await
}

async fn project_keys(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    project_action(
        app,
        bearer(&headers),
        crate::admin::ProjectAction::Keys(crate::admin::KeyAction::List),
    )
    .await
}

async fn create_project_key(
    State(app): State<App>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let token = bearer(request.headers());
    let key: crate::projects::NewKey =
        serde_json::from_value(read_json(request, app.max_body_bytes).await?)
            .map_err(|_| ApiError::bad("Expected name, permission and optional read_only"))?;
    key.validate().map_err(ApiError::bad)?;
    let result = project_action(
        app,
        token,
        crate::admin::ProjectAction::Keys(crate::admin::KeyAction::Create(key)),
    )
    .await?;
    Ok((StatusCode::CREATED, result))
}

async fn rotate_project_key(
    State(app): State<App>,
    Path(key_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    project_action(
        app,
        bearer(&headers),
        crate::admin::ProjectAction::Keys(crate::admin::KeyAction::Rotate(key_id)),
    )
    .await
}

async fn admin_key_action(
    app: App,
    token: String,
    project_id: String,
    action: crate::admin::KeyAction,
) -> Result<Json<Value>, ApiError> {
    tokio::task::spawn_blocking(move || app.admin.admin_keys(&token, &project_id, action))
        .await
        .map_err(|_| ApiError::unavailable())?
        .map(Json)
}

async fn admin_project_keys(
    State(app): State<App>,
    Path(project_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin_key_action(
        app,
        bearer(&headers),
        project_id,
        crate::admin::KeyAction::List,
    )
    .await
}

async fn admin_rotate_project_key(
    State(app): State<App>,
    Path((project_id, key_id)): Path<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    admin_key_action(
        app,
        bearer(&headers),
        project_id,
        crate::admin::KeyAction::Rotate(key_id),
    )
    .await
}

async fn events(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store.events(resolved.project.id.clone(), query).await?,
    ))
}
async fn health(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    app.store
        .call("health", |db| {
            db.query_row("SELECT 1", [], |row| row.get::<_, i32>(0))?;
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "status": "ok" })))
}

async fn summary(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .summary(resolved.project.id.clone(), query, false)
            .await?,
    ))
}

async fn engagement(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .engagement(resolved.project.id.clone(), query)
            .await?,
    ))
}

async fn quality(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .quality(resolved.project.id.clone(), query)
            .await?,
    ))
}

async fn renditions(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .renditions(resolved.project.id.clone(), query)
            .await?,
    ))
}

async fn videos(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .summary(resolved.project.id.clone(), query, true)
            .await?,
    ))
}

async fn timeseries(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::TimeseriesQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .timeseries(resolved.project.id.clone(), query)
            .await?,
    ))
}

fn csv_response(bytes: impl Into<axum::body::Body>, filename: &str) -> Result<Response, ApiError> {
    let mut response = Response::new(bytes.into());
    response.headers_mut().insert(
        "content-type",
        HeaderValue::from_static("text/csv; charset=utf-8"),
    );
    response.headers_mut().insert(
        "content-disposition",
        HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
            .map_err(|_| ApiError::internal())?,
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
        .headers_mut()
        .insert("cache-control", HeaderValue::from_static("no-store"));
    Ok(response)
}

async fn views(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::ViewsQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store.views(resolved.project.id.clone(), query).await?,
    ))
}

async fn view(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    Path(id): Path<String>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .view(resolved.project.id.clone(), id, query)
            .await?,
    ))
}

async fn dimension(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    Path(dimension): Path<crate::query::Dimension>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store
            .dimension(resolved.project.id.clone(), dimension, query)
            .await?,
    ))
}

async fn errors(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store.errors(resolved.project.id.clone(), query).await?,
    ))
}

async fn create_export(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let base = public_base(&app, &request)?;
    let body = serde_json::from_value(read_json(request, 32 * 1024).await?)
        .map_err(|_| ApiError::bad("Invalid export request"))?;
    Ok((
        StatusCode::CREATED,
        Json(app.exports.create(&resolved, body, &base)?),
    ))
}
async fn download_export(
    State(app): State<App>,
    Path(token): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return Err(ApiError::bad("Download filters are bound to the token"));
    }
    let report = app.exports.download(&token).await?;
    let stream = futures::stream::unfold(report.chunks, |mut chunks| async move {
        chunks
            .recv()
            .await
            .map(|chunk| (chunk.map_err(|e| std::io::Error::other(e.message)), chunks))
    });
    csv_response(axum::body::Body::from_stream(stream), &report.filename)
}
async fn export_head(
    State(app): State<App>,
    Path(token): Path<String>,
    request: Request,
) -> Result<Response, ApiError> {
    if request.uri().query().is_some_and(|q| !q.is_empty()) {
        return Err(ApiError::bad("Download filters are bound to the token"));
    }
    let filename = app.exports.filename(&token)?;
    csv_response(axum::body::Body::empty(), &filename)
}
fn public_base(app: &App, request: &Request) -> Result<reqwest::Url, ApiError> {
    if let Some(url) = &app.public_url {
        return Ok(url.clone());
    }
    let trusted = request
        .extensions()
        .get::<ConnectInfo<SocketAddr>>()
        .is_some_and(|peer| {
            app.trusted_proxies
                .iter()
                .any(|network| network.contains(&peer.0.ip()))
        });
    let scheme = if trusted {
        request
            .headers()
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .filter(|v| matches!(*v, "http" | "https"))
            .unwrap_or("http")
    } else {
        request.uri().scheme_str().unwrap_or("http")
    };
    let authority = request
        .headers()
        .get("host")
        .and_then(|v| v.to_str().ok())
        .or_else(|| request.uri().authority().map(|a| a.as_str()))
        .ok_or_else(|| ApiError::bad("Missing Host"))?;
    let url = reqwest::Url::parse(&format!("{scheme}://{authority}/"))
        .map_err(|_| ApiError::bad("Invalid Host"))?;
    if !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ApiError::bad("Invalid Host"));
    }
    Ok(url)
}
