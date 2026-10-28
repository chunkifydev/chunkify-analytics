use super::{Backend, Companion, contract};
use crate::store::Store;
use axum::Router;
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use std::sync::Arc;
use tokio::sync::Semaphore;

pub fn router<S: Clone + Send + Sync + 'static>(
    store: Store,
    exports: crate::exports::Exports,
    allowed_hosts: Vec<String>,
) -> Router<S> {
    let queries = Arc::new(Semaphore::new(4));
    let contract = Arc::new(contract::Contract::new());
    let mut config = StreamableHttpServerConfig::default()
        .with_allowed_hosts(allowed_hosts)
        .with_max_request_body_bytes(64 * 1024);
    // Authenticate every HTTP request instead of retaining a project's identity
    // in a protocol session. This also makes revocation effective on the next call.
    config.legacy_session_mode = false;
    config.json_response = true;
    let service: StreamableHttpService<Companion, LocalSessionManager> = StreamableHttpService::new(
        move || {
            Ok(Companion {
                backend: Backend::Local {
                    store: store.clone(),
                    exports: exports.clone(),
                },
                contract: contract.clone(),
                queries: queries.clone(),
            })
        },
        Default::default(),
        config,
    );
    Router::new().route_service("/mcp", service)
}
