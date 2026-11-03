use std::{io::IsTerminal, process::ExitCode, sync::Arc, time::Duration};

use content_gateway::{
    app::{self, AppState},
    config::{self, Config, DEFAULT_LOG},
    redis_store::{self, RedisStore},
    serve::{self, Stopped},
};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

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

    let redis = match &config.redis_url {
        None => None,
        Some(url) => match RedisStore::open(url.as_str(), redis_store::DEFAULT_PREFIX) {
            Ok(store) => {
                store.ping().await;
                Some(store)
            }
            Err(error) => {
                tracing::error!("cannot use REDIS_URL: {error}");
                return ExitCode::FAILURE;
            }
        },
    };

    let state = Arc::new(AppState::from_config(&config, redis));
    tracing::info!(
        "listening on {} for {} consumer(s), {} calls a minute, budget kept in {}",
        config.listen,
        config.consumers.len(),
        config.budget_per_minute,
        if config.redis_url.is_some() {
            "Redis, in memory while it fails"
        } else {
            "memory"
        }
    );
    let stopped = serve::serve(
        listener,
        app::router(state.clone()),
        shutdown_signal(),
        SHUTDOWN_GRACE,
    )
    .await;
    state.budget.write_days_at_exit().await;
    match stopped {
        Ok(Stopped::Drained) => {
            tracing::info!("stopped");
            ExitCode::SUCCESS
        }
        Ok(Stopped::GraceExpired) => {
            tracing::warn!(
                "requests still open {} s after the shutdown signal, stopped anyway",
                SHUTDOWN_GRACE.as_secs()
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!("{error}");
            ExitCode::FAILURE
        }
    }
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
