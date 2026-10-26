mod admin;
mod api;
mod config;
mod context;
mod event;
mod mcp;
mod otlp;
mod profile;
mod projects;
mod query;
mod reader;
mod retention;
mod spool;
mod storage;
mod store;
mod writer;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let command = args.next();
    let parameters: Vec<_> = args.collect();
    if command.as_deref() == Some("mcp") && parameters.is_empty() {
        return mcp::run().await;
    }
    let config = config::Config::load()?;
    match (command.as_deref(), parameters.as_slice()) {
        (Some("init"), []) => config.initialize(),
        (Some("serve"), []) => {
            let _lock = config.lock()?;
            let keys = config.keys()?;
            let config = std::sync::Arc::new(config);
            let projects = std::sync::Arc::new(projects::Registry::new(&keys, &config)?);
            let admin = admin::Server::bind(config.clone(), projects.clone(), keys)?;
            let (store, worker) = store::Store::open(&config.data_dir.join("analytics.duckdb"), config.batch.clone(), config.profile, config.checkpoint_threshold_mib, config.spool.clone(), config.separate_reads, config.duckdb_threads)?;
            store.drain().await.map_err(|_| "Queue recovery failed")?;
            let removed = store.purge_expired(projects.clone()).await
                .map_err(|_| "Startup retention sweep failed")?;
            if removed > 0 { println!("Retention removed {removed} events"); }
            let listener = tokio::net::TcpListener::bind(config.bind).await?;
            let interval = std::time::Duration::from_secs(config.retention_interval_seconds);
            if projects.retention.read().expect("retention policy").enabled() {
                println!("Retention: {} seconds by default; project overrides applied; sweep interval: {} seconds", config.retention_seconds, interval.as_secs());
            } else {
                println!("Retention: disabled; project policies can be enabled live");
            }
            // Keep the timer active even when all policies are disabled, because
            // the administrator can enable expiration without restarting.
            let maintenance_store = store.clone();
            let maintenance_projects = projects.clone();
            let maintenance = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    match maintenance_store.purge_expired(maintenance_projects.clone()).await {
                        Ok(removed) if removed > 0 => println!("Retention removed {removed} events"),
                        Err(_) => eprintln!("Retention sweep failed; will retry at the next interval"),
                        _ => {},
                    }
                }
            });
            let shutdown = shutdown();
            let admin = admin.start();
            println!("Listening on http://{}", listener.local_addr()?);
            let result = axum::serve(listener, api::router(store.clone(), projects, &config)
                .into_make_service_with_connect_info::<std::net::SocketAddr>())
                .with_graceful_shutdown(shutdown)
                .await;
            admin.stop().await;
            maintenance.abort();
            let _ = maintenance.await;
            drop(store);
            worker.join().map_err(|_| "Database worker panicked")??;
            result?;
            Ok(())
        }
        (Some("delete-viewer"), [viewer_id]) | (Some("delete-viewer"), [_, viewer_id]) => {
            let project_id = if parameters.len() == 2 { parameters[0].clone() } else { projects::default_project() };
            let _lock = config.lock()?;
            let keys = config.keys()?;
            if !projects::projects(&keys).iter().any(|p| p.id == project_id) { return Err("Unknown project".into()); }
            let (store, worker) = store::Store::open(&config.data_dir.join("analytics.duckdb"), config.batch.clone(), config.profile, config.checkpoint_threshold_mib, config.spool.clone(), config.separate_reads, config.duckdb_threads)?;
            store.drain().await.map_err(|_| "Queue recovery failed")?;
            let result = store.delete_viewer(project_id, viewer_id.clone()).await;
            drop(store);
            worker.join().map_err(|_| "Database worker panicked")??;
            println!("{}", serde_json::json!({"deleted_events": result.map_err(|_| "Viewer deletion failed")?}));
            Ok(())
        }
        (Some(command @ ("project-list" | "project-create" | "project-rename" | "project-configure" | "project-retention" | "key-create" | "key-revoke" | "key-rotate")), args) => admin::command(&config, command, args).await,
        (Some("rotate-key"), [_]) => admin::command(&config, "rotate-key", &parameters).await,
        _ => Err("Usage: chunkify-analytics init | serve | mcp | delete-viewer [project] <viewer_id> | rotate-key <ingest|read> | project-list | project-create <id> [name] | project-rename <id> <name> | project-configure <id> <settings.json> | project-retention <id> <seconds|inherit> | key-create <project> <ingest|read> <name> | key-rotate <project> <key-id> | key-revoke <project> <key-id>".into()),
    }
}

#[cfg(unix)]
fn shutdown() -> impl std::future::Future<Output = ()> {
    let mut terminate = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("install SIGTERM handler");
    let mut interrupt = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
        .expect("install SIGINT handler");
    async move {
        tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
    }
}

#[cfg(not(unix))]
async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
}
