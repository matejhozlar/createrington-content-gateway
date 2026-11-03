use std::{io::IsTerminal, process::ExitCode, sync::Arc, time::Duration};

use content_gateway::{
    app::{self, AppState},
    config::{self, Config, DEFAULT_LOG, RedisUrl},
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

    let budget_keeps = if config.cache_redis_url.is_some() {
        "the budget"
    } else {
        "the budget and the cache"
    };
    let budget_redis = match open_redis(config.redis_url.as_ref(), "REDIS_URL", budget_keeps).await
    {
        Ok(store) => store,
        Err(code) => return code,
    };
    let cache_redis = match &config.cache_redis_url {
        None => budget_redis.clone(),
        Some(url) => {
            match open_redis(Some(url), "CONTENT_GATEWAY_CACHE_REDIS_URL", "the cache").await {
                Ok(store) => store,
                Err(code) => return code,
            }
        }
    };
    let kept_in = |store: &Option<Arc<RedisStore>>| {
        if store.is_some() {
            "Redis, in memory while it fails"
        } else {
            "memory"
        }
    };
    let (budget_in, cache_in) = (kept_in(&budget_redis), kept_in(&cache_redis));

    let state = match AppState::from_config(&config, budget_redis, cache_redis) {
        Ok(state) => Arc::new(state),
        Err(error) => {
            tracing::error!("cannot set up the CurseForge client: {error}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        "listening on {} for {} consumer(s), {} calls a minute, budget kept in {budget_in}, cache in {cache_in}",
        config.listen,
        config.consumers.len(),
        config.budget_per_minute,
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

async fn open_redis(
    url: Option<&RedisUrl>,
    name: &str,
    keeps: &'static str,
) -> Result<Option<Arc<RedisStore>>, ExitCode> {
    let Some(url) = url else {
        return Ok(None);
    };
    match RedisStore::open(url.as_str(), redis_store::DEFAULT_PREFIX) {
        Ok(store) => {
            let store = store.keeping(keeps);
            store.ping().await;
            Ok(Some(Arc::new(store)))
        }
        Err(error) => {
            tracing::error!("cannot use {name}: {error}");
            Err(ExitCode::FAILURE)
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
