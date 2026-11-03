mod app;
mod config;
mod health;

use std::{io::IsTerminal, process::ExitCode};

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, DEFAULT_LOG};

#[tokio::main]
async fn main() -> ExitCode {
    let (log_filter, log_filter_error) = match config::log_filter_from_env() {
        Ok(filter) => (filter, None),
        Err(error) => (EnvFilter::new(DEFAULT_LOG), Some(error)),
    };
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(log_filter)
        .init();

    if let Some(error) = log_filter_error {
        tracing::error!("invalid configuration: {error}");
        return ExitCode::FAILURE;
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!("invalid configuration: {error}");
            return ExitCode::FAILURE;
        }
    };

    let listener = match TcpListener::bind(config.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!("cannot listen on {}: {error}", config.listen);
            return ExitCode::FAILURE;
        }
    };

    tracing::info!("listening on {}", config.listen);
    if let Err(error) = axum::serve(listener, app::router())
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        tracing::error!("server stopped: {error}");
        return ExitCode::FAILURE;
    }

    tracing::info!("stopped");
    ExitCode::SUCCESS
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!("cannot listen for ctrl-c: {error}");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut signal) => {
                signal.recv().await;
            }
            Err(error) => {
                tracing::warn!("cannot listen for SIGTERM: {error}");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = interrupt => {}
        () = terminate => {}
    }
    tracing::info!("shutting down");
}
