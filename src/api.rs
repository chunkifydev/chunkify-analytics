use crate::{
    config::Config,
    context::RequestContext,
    event, otlp,
    projects::{Permission, RateLimit, Registry, Resolved},
    store::Store,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{ConnectInfo, Extension, Path, Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    io::Read,
    net::SocketAddr,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct App {
    store: Store,
    projects: Arc<Registry>,
    admin: Arc<crate::admin::Controller>,
    max_body_bytes: usize,
    max_otlp_records: usize,
    store_ip: bool,
    trusted_proxies: Vec<ipnet::IpNet>,
    mcp_enabled: bool,
    rate: Arc<Mutex<RateLimit>>,
}

pub struct ApiError(pub StatusCode, pub String);
impl ApiError {
    pub fn bad(message: impl Into<String>) -> Self {
        Self(StatusCode::BAD_REQUEST, message.into())
    }
    pub fn unavailable() -> Self {
        Self(
            StatusCode::SERVICE_UNAVAILABLE,
            "Collector is busy or unavailable".into(),
        )
    }
    pub fn internal() -> Self {
        Self(
            StatusCode::INTERNAL_SERVER_ERROR,
            "Storage operation failed".into(),
        )
    }
}
impl From<duckdb::Error> for ApiError {
    fn from(_: duckdb::Error) -> Self {
        Self::internal()
    }
}
impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let mut response = (self.0, Json(json!({"error": self.1}))).into_response();
        response
            .headers_mut()
            .insert("cache-control", HeaderValue::from_static("no-store"));
        if self.0 == StatusCode::UNAUTHORIZED {
            response
                .headers_mut()
                .insert("www-authenticate", HeaderValue::from_static("Bearer"));
        }
        if self.0 == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("1"));
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
        store,
        projects,
        admin,
        max_body_bytes: config.max_body_bytes,
        max_otlp_records: config.max_otlp_records,
        store_ip: config.store_ip,
        trusted_proxies: config.trusted_proxies.clone(),
        mcp_enabled: config.mcp_enabled,
        rate: Arc::new(Mutex::new(RateLimit::new(config.ingest_per_second))),
    };
    let mcp = if config.mcp_enabled {
        crate::mcp::router(app.store.clone(), config.mcp_allowed_hosts.clone())
    } else {
        Router::new()
    };
    Router::new()
        .merge(mcp)
        .route("/healthz", get(health))
        .route("/v1/ingestion", get(ingestion_status))
        .route("/v1/projects", post(create_project))
        .route("/v1/project/allowed-origins", put(project_origins))
        .route("/v1/project/keys", get(project_keys))
        .route("/v1/project/keys/{key_id}/rotate", post(rotate_project_key))
        .route("/v1/summary", get(summary))
        .route("/v1/quality", get(quality))
        .route("/v1/engagement", get(engagement))
        .route("/v1/timeseries", get(timeseries))
        .route("/v1/videos", get(videos))
        .route(
            "/v1/logs",
            post(ingest_otlp).options(|| async { StatusCode::NO_CONTENT }),
        )
        .route(
            "/v1/events",
            get(events)
                .post(ingest)
                .options(|| async { StatusCode::NO_CONTENT }),
        )
        .layer(middleware::from_fn_with_state(app.clone(), access))
        .layer(middleware::from_fn(otlp_errors))
        .with_state(app)
}

// Cover authentication and middleware errors as well as ingestion failures.
// OTLP/HTTP JSON uses google.rpc.Status, whose optional code may be omitted.
async fn otlp_errors(request: Request, next: Next) -> Response {
    let is_otlp = request.uri().path() == "/v1/logs";
    let response = next.run(request).await;
    if !is_otlp || response.status().is_success() {
        return response;
    }
    let (mut parts, body) = response.into_parts();
    let message = to_bytes(body, 16384)
        .await
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| value.get("error")?.as_str().map(str::to_owned))
        .unwrap_or_else(|| {
            parts
                .status
                .canonical_reason()
                .unwrap_or("Request failed")
                .into()
        });
    parts.headers.remove("content-length");
    parts
        .headers
        .insert("content-type", HeaderValue::from_static("application/json"));
    parts
        .headers
        .insert("cache-control", HeaderValue::from_static("no-store"));
    Response::from_parts(parts, Body::from(json!({"message": message}).to_string()))
}

async fn access(
    State(app): State<App>,
    mut request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let mcp = app.mcp_enabled && request.uri().path() == "/mcp";
    let ingest = matches!(request.uri().path(), "/v1/events" | "/v1/logs")
        && matches!(*request.method(), Method::POST | Method::OPTIONS);
    let resolved = if request.uri().path() == "/v1/projects" {
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
        Some(app.projects.authorize(secret, Permission::Secret)?)
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
                return Err(ApiError(
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
            return Err(ApiError(
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
    let event = read_json(request, app.max_body_bytes, false).await?;
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

async fn ingest_otlp(
    State(app): State<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let resolved = request
        .extensions()
        .get::<Resolved>()
        .cloned()
        .ok_or_else(ApiError::internal)?;
    let context = app
        .store_ip
        .then(|| RequestContext::capture(peer.ip(), request.headers(), &app.trusted_proxies));
    let request = read_json(request, app.max_body_bytes, true).await?;
    let events = otlp::decode(&request, app.max_otlp_records)?;
    drop(request);
    app.store
        .insert_batch(&resolved.project, &resolved.key_id, events, context)
        .await?;
    Ok(Json(json!({})))
}

async fn read_json(request: Request, limit: usize, allow_gzip: bool) -> Result<Value, ApiError> {
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
        return Err(ApiError(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "Expected application/json".into(),
        ));
    }
    let gzip = match request
        .headers()
        .get("content-encoding")
        .map(|v| v.to_str())
    {
        None | Some(Ok("identity")) => false,
        Some(Ok("gzip")) if allow_gzip => true,
        _ => {
            return Err(ApiError(
                StatusCode::UNSUPPORTED_MEDIA_TYPE,
                "Unsupported Content-Encoding".into(),
            ));
        }
    };
    let bytes = to_bytes(request.into_body(), limit).await.map_err(|_| {
        ApiError(
            StatusCode::PAYLOAD_TOO_LARGE,
            "Request body exceeds limit or could not be read".into(),
        )
    })?;
    tokio::task::spawn_blocking(move || {
        let decoded;
        let json = if gzip {
            let mut output = Vec::new();
            flate2::read::MultiGzDecoder::new(bytes.as_ref())
                .take(limit.saturating_add(1) as u64)
                .read_to_end(&mut output)
                .map_err(|_| ApiError::bad("Malformed gzip body"))?;
            if output.len() > limit {
                return Err(ApiError(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Decompressed body exceeds limit".into(),
                ));
            }
            decoded = output;
            decoded.as_slice()
        } else {
            bytes.as_ref()
        };
        serde_json::from_slice(json).map_err(|_| ApiError::bad("Malformed JSON"))
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

fn normalize_origins(mut origins: Vec<String>) -> Result<Vec<String>, ApiError> {
    for origin in &origins {
        crate::config::validate_origin(origin).map_err(ApiError::bad)?;
    }
    origins.sort();
    origins.dedup();
    Ok(origins)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NewProject {
    name: String,
    #[serde(default)]
    allowed_origins: Vec<String>,
}

async fn create_project(
    State(app): State<App>,
    request: Request,
) -> Result<impl IntoResponse, ApiError> {
    let token = bearer(request.headers());
    let body: NewProject =
        serde_json::from_value(read_json(request, app.max_body_bytes, false).await?)
            .map_err(|_| ApiError::bad("Expected a name and optional allowed_origins"))?;
    crate::projects::validate_name(&body.name).map_err(ApiError::bad)?;
    let origins = normalize_origins(body.allowed_origins)?;
    let result =
        tokio::task::spawn_blocking(move || app.admin.create_project(&token, body.name, origins))
            .await
            .map_err(|_| ApiError::unavailable())??;
    Ok((StatusCode::CREATED, Json(result)))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Origins {
    allowed_origins: Vec<String>,
}

async fn project_origins(
    State(app): State<App>,
    request: Request,
) -> Result<Json<Value>, ApiError> {
    let token = bearer(request.headers());
    let body: Origins =
        serde_json::from_value(read_json(request, app.max_body_bytes, false).await?)
            .map_err(|_| ApiError::bad("Expected allowed_origins as an array of origins"))?;
    let origins = normalize_origins(body.allowed_origins)?;
    project_action(app, token, crate::admin::ProjectAction::Origins(origins)).await
}

async fn project_keys(State(app): State<App>, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    project_action(app, bearer(&headers), crate::admin::ProjectAction::Keys).await
}

async fn rotate_project_key(
    State(app): State<App>,
    Path(key_id): Path<String>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    project_action(
        app,
        bearer(&headers),
        crate::admin::ProjectAction::RotateKey(key_id),
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

async fn ingestion_status(
    State(app): State<App>,
    Extension(resolved): Extension<Resolved>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        app.store.ingestion(resolved.project.id.clone()).await?,
    ))
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
