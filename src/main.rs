mod api;
mod config;
mod event;
mod query;
mod store;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = config::Config::load()?;
    match std::env::args().nth(1).as_deref() {
        Some("init") => config.initialize(),
        Some("serve") => {
            let keys = config.keys()?;
            let (store, worker) = store::Store::open(&config.data_dir.join("analytics.duckdb"))?;
            let listener = tokio::net::TcpListener::bind(config.bind).await?;
            println!("Listening on http://{}", listener.local_addr()?);
            let result = axum::serve(listener, api::router(store.clone(), keys, &config))
                .with_graceful_shutdown(shutdown())
                .await;
            drop(store);
            worker.join().map_err(|_| "Database worker panicked")??;
            result?;
            Ok(())
        }
        _ => Err("Usage: chunkify-analytics-collector <init|serve>".into()),
    }
}

async fn shutdown() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        tokio::select! { _ = tokio::signal::ctrl_c() => {}, _ = terminate.recv() => {} }
    }
    #[cfg(not(unix))]
    let _ = tokio::signal::ctrl_c().await;
}
