mod admin;
mod api;
mod backend;
mod cli;
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
mod profile;
mod projects;
mod query;
mod reader;
mod retention;
mod s3_store;
mod storage;
mod store;
mod writer;

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
    let config = config::Config::load_with(&settings, cli.data_dir.as_deref(), bind)?;
    match cli.command {
        cli::Command::Init => {
            println!("{}", config.initialize()?);
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
            let _lock = config.lock()?;
            let keys = config.keys()?;
            let config = std::sync::Arc::new(config);
            let projects = std::sync::Arc::new(projects::Registry::new(&keys, &config)?);
            let admin = admin::Server::bind(config.clone(), projects.clone(), keys)?;
            let controller = admin.controller();
            let (store, worker) = store::Store::from_config(&config, projects.clone())?;
            let removed = store
                .purge_expired(projects.clone())
                .await
                .map_err(|_| "Startup retention sweep failed")?;
            if removed > 0 {
                println!("Retention removed {removed} events");
            }
            let listener = tokio::net::TcpListener::bind(config.bind).await?;
            let interval = std::time::Duration::from_secs(config.retention_interval_seconds);
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
            let maintenance_projects = projects.clone();
            let maintenance = tokio::spawn(async move {
                loop {
                    tokio::time::sleep(interval).await;
                    match maintenance_store
                        .purge_expired(maintenance_projects.clone())
                        .await
                    {
                        Ok(removed) if removed > 0 => {
                            println!("Retention removed {removed} events")
                        }
                        Err(_) => {
                            eprintln!("Retention sweep failed; will retry at the next interval")
                        }
                        _ => {}
                    }
                }
            });
            let shutdown = shutdown();
            let admin = admin.start();
            println!("Listening on http://{}", listener.local_addr()?);
            let result = axum::serve(
                listener,
                api::router(store.clone(), projects, &config, controller, geolocation)
                    .into_make_service_with_connect_info::<std::net::SocketAddr>(),
            )
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
        cli::Command::Project { command } => {
            let (command, args) = command.request();
            admin::command(&config, command, &args).await
        }
        cli::Command::Key { command } => {
            let (command, args) = command.request();
            admin::command(&config, command, &args).await
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
