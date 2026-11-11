mod admin;
mod api;
mod backend;
mod batch;
mod cache;
mod cli;
mod clickhouse;
mod config;
mod settings;

use clap::Parser;
use tracing_subscriber::prelude::*;
mod context;
mod event;
mod exports;
mod geolocation;
mod limits;
mod mcp;
mod projects;
mod query;
mod retention;
mod storage;
mod store;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let cli = cli::Cli::parse();
    let settings = settings::Settings::load(cli.env_file.as_deref(), cli.config.as_deref())?;
    if matches!(cli.command, cli::Command::Mcp) {
        return mcp::run(&settings).await;
    }
    let bind = match &cli.command {
        cli::Command::Serve { bind } => *bind,
        _ => None,
    };
    let config = config::Config::load_with(&settings, bind)?;
    match cli.command {
        cli::Command::Init => {
            println!("{}", config.initialize()?);
            Ok(())
        }
        cli::Command::Migrate => {
            let applied = clickhouse::migrate(&config.clickhouse, false)?;
            println!(
                "{}",
                serde_json::json!({"version": clickhouse::migrations::latest(), "applied": applied})
            );
            Ok(())
        }
        cli::Command::Config {
            command: cli::ConfigCommand::Check,
        } => {
            println!("Configuration is valid.");
            Ok(())
        }
        cli::Command::Serve { .. } => {
            tracing_subscriber::registry()
                // Dependency diagnostics can contain request or credential data.
                .with(
                    tracing_subscriber::filter::Targets::new()
                        .with_target("chunkify_analytics", config.log_level),
                )
                .with(
                    tracing_subscriber::fmt::layer()
                        .with_writer(std::io::stderr)
                        .with_ansi(false)
                        .with_target(false),
                )
                .try_init()
                .map_err(|_| "Cannot initialize server logging")?;
            let geolocation =
                std::sync::Arc::new(geolocation::Database::open(&config.geoip_database)?);
            // Servers keep no local state: any number can share one database.
            let keys = config.keys()?;
            let config = std::sync::Arc::new(config);
            let projects = std::sync::Arc::new(projects::Registry::new(&keys, &config)?);
            let controller = std::sync::Arc::new(admin::Controller::new(
                config.clone(),
                projects.clone(),
                keys,
            ));
            let store = clickhouse::Store::open(&config, projects.clone()).await?;
            println!(
                "Node ID: {} ({})",
                store.node_id(),
                if config.clickhouse.node_id.is_some() {
                    "configured"
                } else {
                    "leased"
                }
            );
            let refresh = admin::refresh_registry(config.clone(), controller.clone());
            let interval = std::time::Duration::from_secs(config.retention_interval_seconds);
            // One server sweeps per interval; the others skip it.
            let removed = store
                .purge_expired_once(interval)
                .await
                .map_err(|_| "Startup retention sweep failed")?;
            if let Some(removed) = removed.filter(|removed| *removed > 0) {
                println!("Retention removed {removed} events");
            }
            let listener = tokio::net::TcpListener::bind(config.bind).await?;
            if projects
                .retention
                .read()
                .expect("retention policy")
                .enabled()
            {
                println!(
                    "Retention: {} seconds by default; project overrides applied; sweep interval: {} seconds",
                    config.retention_seconds,
                    interval.as_secs()
                );
            } else {
                println!("Retention: disabled; project policies can be enabled live");
            }
            // Keep the timer active even when all policies are disabled, because
            // the administrator can enable expiration without restarting.
            let maintenance_store = store.clone();
            let maintenance = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    match maintenance_store.purge_expired_once(interval).await {
                        Ok(Some(removed)) if removed > 0 => {
                            println!("Retention removed {removed} events")
                        }
                        Err(_) => {
                            eprintln!("Retention sweep failed; will retry at the next interval")
                        }
                        _ => {}
                    }
                }
            });
            // Any server may refresh rollups; a claimed run keeps others idle.
            // Shutdown waits for a run in progress, so the next run need not
            // wait for its claim to time out.
            let rollup_store = store.clone();
            let rollup_interval = store.rollup_interval();
            let (stop_rollups, mut stopping) = tokio::sync::watch::channel(false);
            let rollups = tokio::spawn(async move {
                if rollup_interval.is_zero() {
                    return;
                }
                loop {
                    if rollup_store.refresh_rollups().await.is_err() {
                        tracing::warn!(backend = "clickhouse", "rollup_refresh_failed");
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(rollup_interval) => {}
                        _ = stopping.changed() => return,
                    }
                }
            });
            let shutdown = shutdown();
            println!("Listening on http://{}", listener.local_addr()?);
            let result = axum::serve(
                listener,
                api::router(store.clone(), projects, &config, controller, geolocation)
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
            .with_graceful_shutdown(shutdown)
            .await;
            // Graceful shutdown has completed every in-flight request, and with
            // it every acknowledged write.
            refresh.abort();
            let _ = refresh.await;
            maintenance.abort();
            let _ = maintenance.await;
            let _ = stop_rollups.send(true);
            let rollups_abort = rollups.abort_handle();
            if tokio::time::timeout(std::time::Duration::from_secs(30), rollups)
                .await
                .is_err()
            {
                rollups_abort.abort();
            }
            result?;
            Ok(())
        }
        cli::Command::Project { command } => {
            let (command, args) = command.request();
            admin::command(&config, command, &args)
        }
        cli::Command::Key { command } => {
            let (command, args) = command.request();
            admin::command(&config, command, &args)
        }
        cli::Command::Mcp => unreachable!(),
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
