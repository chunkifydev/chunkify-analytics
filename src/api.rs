use crate::{
    config::{Config, Keys},
    event, otlp,
    store::Store,
};
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use std::{
    io::Read,
    sync::{Arc, Mutex},
    time::Instant,
};
use subtle::ConstantTimeEq;

#[derive(Clone)]
struct App {
    store: Store,
    keys: Keys,
    origins: Vec<String>,
    max_body_bytes: usize,
    max_otlp_records: usize,
    rate: Arc<Mutex<RateLimit>>,
}

struct RateLimit {
    tokens: f64,
    capacity: f64,
    updated: Instant,
}
impl RateLimit {
    fn take(&mut self) -> Result<(), ApiError> {
        let now = Instant::now();
        self.tokens = (self.tokens
            + now.duration_since(self.updated).as_secs_f64() * self.capacity)
            .min(self.capacity);
        self.updated = now;
        if self.tokens < 1.0 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "Ingestion rate limit exceeded".into(),
            ));
        }
        self.tokens -= 1.0;
        Ok(())
    }
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

fn authorized(headers: &HeaderMap, name: &str, expected: &str) -> Result<(), ApiError> {
    let supplied = headers.get(name).map(|v| v.as_bytes()).unwrap_or_default();
    if bool::from(supplied.ct_eq(expected.as_bytes())) {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Missing or invalid credentials".into(),
        ))
    }
}

pub fn router(store: Store, keys: Keys, config: &Config) -> Router {
    let capacity = f64::from(config.ingest_per_second);
    let app = App {
        store,
        keys,
        origins: config.allowed_origins.clone(),
        max_body_bytes: config.max_body_bytes,
        max_otlp_records: config.max_otlp_records,
        rate: Arc::new(Mutex::new(RateLimit {
            tokens: capacity,
            capacity,
            updated: Instant::now(),
        })),
    };
    Router::new()
        .route("/healthz", get(health))
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
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let ingest = matches!(request.uri().path(), "/v1/events" | "/v1/logs")
        && matches!(*request.method(), Method::POST | Method::OPTIONS);
    let origin = if ingest {
        match request.headers().get("origin") {
            Some(origin)
                if origin
                    .to_str()
                    .is_ok_and(|s| app.origins.iter().any(|o| o == s)) =>
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
        authorized(
            request.headers(),
            "x-chunkify-ingest-key",
            &app.keys.ingest_key,
        )?;
        app.rate
            .lock()
            .map_err(|_| ApiError::unavailable())?
            .take()?;
    } else if request.uri().path().starts_with("/v1/") {
        authorized(
            request.headers(),
            "authorization",
            &format!("Bearer {}", app.keys.read_token),
        )?;
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

async fn ingest(State(app): State<App>, request: Request) -> Result<Response, ApiError> {
    let event = read_json(request, app.max_body_bytes, false).await?;
    event::validate(&event)?;
    let inserted = app.store.insert(event).await?;
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

async fn ingest_otlp(State(app): State<App>, request: Request) -> Result<Json<Value>, ApiError> {
    let request = read_json(request, app.max_body_bytes, true).await?;
    app.store
        .insert_batch(otlp::decode(&request, app.max_otlp_records)?)
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
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(app.store.events(query).await?))
}
async fn health(State(app): State<App>) -> Result<Json<Value>, ApiError> {
    app.store
        .call(|db| {
            db.query_row("SELECT 1", [], |row| row.get::<_, i32>(0))?;
            Ok(())
        })
        .await?;
    Ok(Json(json!({ "status": "ok" })))
}

async fn summary(
    State(app): State<App>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(app.store.summary(query, false).await?))
}

async fn videos(
    State(app): State<App>,
    axum::extract::Query(query): axum::extract::Query<crate::query::Query>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(app.store.summary(query, true).await?))
}
