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
    extract::{ConnectInfo, Extension, Request, State},
    http::{HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
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
    max_body_bytes: usize,
    max_otlp_records: usize,
    store_ip: bool,
    trusted_proxies: Vec<ipnet::IpNet>,
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
        if self.0 == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert("retry-after", HeaderValue::from_static("1"));
        }
        response
    }
}

pub fn router(store: Store, projects: Registry, config: &Config) -> Router {
    let app = App {
        store,
        projects: Arc::new(projects),
        max_body_bytes: config.max_body_bytes,
        max_otlp_records: config.max_otlp_records,
        store_ip: config.store_ip,
        trusted_proxies: config.trusted_proxies.clone(),
        rate: Arc::new(Mutex::new(RateLimit::new(config.ingest_per_second))),
    };
    Router::new()
        .route("/healthz", get(health))
        .route("/v1/ingestion", get(ingestion_status))
        .route("/v1/summary", get(summary))
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
    let ingest = matches!(request.uri().path(), "/v1/events" | "/v1/logs")
        && matches!(*request.method(), Method::POST | Method::OPTIONS);
    let resolved = if ingest && request.method() != Method::OPTIONS {
        Some(
            app.projects.authorize(
                request
                    .headers()
                    .get("x-chunkify-ingest-key")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or(""),
                Permission::Ingest,
            )?,
        )
    } else if !ingest && request.uri().path().starts_with("/v1/") {
        let secret = request
            .headers()
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("Bearer "))
            .unwrap_or("");
        Some(app.projects.authorize(secret, Permission::Read)?)
    } else {
        None
    };
    let origin = if ingest {
        match request.headers().get("origin") {
            Some(origin)
                if origin.to_str().is_ok_and(|s| {
                    resolved.as_ref().map_or_else(
                        || {
                            app.projects
                                .projects
                                .values()
                                .any(|p| p.origins.iter().any(|o| o == s))
                        },
                        |r| r.project.origins.iter().any(|o| o == s),
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
    let inserted = match app
        .store
        .insert(&resolved.project, &resolved.key_id, event, context)
        .await?
    {
        crate::store::Receipt::Queued => {
            return Ok((
                StatusCode::ACCEPTED,
                Json(json!({"accepted": true, "queued": true})),
            )
                .into_response());
        }
        crate::store::Receipt::Committed(count) => count > 0,
    };
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
    app.store.ensure_healthy()?;
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
