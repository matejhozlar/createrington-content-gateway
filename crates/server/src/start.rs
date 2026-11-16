use std::{io::IsTerminal, process::ExitCode, sync::Arc, time::Duration};

use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::{
    app::{self, Addresses, AppState, Timeouts},
    clock::{Clock, SystemClock},
    config::{self, Config, DEFAULT_LOG},
    openapi,
    redis_store::{self, RedisStore},
    serve::{self, Stopped},
};

const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
const OPENAPI_FLAG: &str = "--openapi";

/// Runs the gateway with configuration from the environment, or prints the OpenAPI document with `--openapi`.
pub async fn run() -> ExitCode {
    if std::env::args().nth(1).as_deref() == Some(OPENAPI_FLAG) {
        return match openapi::pretty_json() {
            Ok(json) => {
                print!("{json}");
                ExitCode::SUCCESS
            }
            Err(error) => {
                eprintln!("cannot write the OpenAPI document: {error}");
                ExitCode::FAILURE
            }
        };
    }

    let (log_filter, log_filter_error) = match config::log_filter_from_env() {
        Ok(filter) => (filter, None),
        Err(error) => (EnvFilter::new(DEFAULT_LOG), Some(error)),
    };
    tracing_subscriber::fmt()
        .with_ansi(std::io::stdout().is_terminal())
        .with_env_filter(log_filter)
        .init();

    if let Some(error) = log_filter_error {
        tracing::error!(%error, "the configuration is invalid");
        return ExitCode::FAILURE;
    }

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(error) => {
            tracing::error!(%error, "the configuration is invalid");
            return ExitCode::FAILURE;
        }
    };

    let listener = match TcpListener::bind(config.listen).await {
        Ok(listener) => listener,
        Err(error) => {
            tracing::error!(address = %config.listen, %error, "cannot listen");
            return ExitCode::FAILURE;
        }
    };

    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let redis = match &config.redis_url {
        None => None,
        Some(url) => {
            match RedisStore::open(url.connection(), redis_store::DEFAULT_PREFIX, clock.clone()) {
                Ok(store) => {
                    store.ping().await;
                    Some(Arc::new(store))
                }
                Err(error) => {
                    tracing::error!(%error, "cannot set up the connection to Redis");
                    return ExitCode::FAILURE;
                }
            }
        }
    };

    let state = match AppState::new(
        &config,
        &Addresses::default(),
        Timeouts::default(),
        clock,
        redis,
    ) {
        Ok(state) => Arc::new(state),
        Err(error) => {
            tracing::error!(%error, "cannot set up the upstream clients");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        address = %config.listen,
        consumers = config.consumers.len(),
        curseforge_limit = config.budget_per_minute,
        modrinth_limit = config.modrinth_per_minute,
        kept_in = if config.redis_url.is_some() {
            "redis"
        } else {
            "memory"
        },
        "listening"
    );
    let stopped = serve::serve(
        listener,
        app::router(state.clone()),
        shutdown_signal(),
        SHUTDOWN_GRACE,
    )
    .await;
    state.curseforge_budget.write_days_at_exit().await;
    state.modrinth_budget.write_days_at_exit().await;
    match stopped {
        Ok(Stopped::Drained) => {
            tracing::info!("stopped");
            ExitCode::SUCCESS
        }
        Ok(Stopped::GraceExpired) => {
            tracing::warn!(
                grace_ms = SHUTDOWN_GRACE.as_millis(),
                "requests were still open when the shutdown grace ended, stopped anyway"
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            tracing::error!(%error, "serving stopped on an error");
            ExitCode::FAILURE
        }
    }
}

async fn shutdown_signal() {
    let interrupt = async {
        if let Err(error) = tokio::signal::ctrl_c().await {
            tracing::warn!(%error, "cannot listen for ctrl-c");
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
                tracing::warn!(%error, "cannot listen for SIGTERM");
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
