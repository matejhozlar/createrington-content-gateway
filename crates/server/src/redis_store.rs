use std::{
    future::Future,
    sync::{Mutex, MutexGuard, PoisonError},
    time::{Duration, Instant},
};

use redis::{
    Client, RedisResult,
    aio::{ConnectionManager, ConnectionManagerConfig},
};

pub const DEFAULT_PREFIX: &str = "content-gateway";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(1);
const PAUSE_AFTER_FAILURE: Duration = Duration::from_secs(5);
const RECONNECT_RETRIES: usize = 2;
const RECONNECT_MAX_DELAY: Duration = Duration::from_secs(1);

/// The connection to Redis, which keeps the gateway's state across restarts.
///
/// It connects on first use and reconnects by itself. A command that fails or
/// takes longer than a second makes the store skip Redis for five seconds, so
/// callers fall back to memory without waiting on it. Losing Redis and getting
/// it back are each logged once. Every key starts with the store's prefix.
pub struct RedisStore {
    connection: ConnectionManager,
    prefix: String,
    link: Mutex<Link>,
}

#[derive(Default)]
struct Link {
    state: LinkState,
    paused_until: Option<Instant>,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum LinkState {
    #[default]
    Untried,
    Up,
    Down,
}

impl RedisStore {
    /// Sets up the connection without making it. Needs a Tokio runtime.
    pub fn open(url: &str, prefix: impl Into<String>) -> RedisResult<Self> {
        let config = ConnectionManagerConfig::new()
            .set_connection_timeout(Some(COMMAND_TIMEOUT))
            .set_response_timeout(Some(COMMAND_TIMEOUT))
            .set_number_of_retries(RECONNECT_RETRIES)
            .set_max_delay(RECONNECT_MAX_DELAY);
        Ok(Self {
            connection: ConnectionManager::new_lazy_with_config(Client::open(url)?, config)?,
            prefix: prefix.into(),
            link: Mutex::default(),
        })
    }

    pub fn key(&self, name: &str) -> String {
        format!("{}:{name}", self.prefix)
    }

    /// Runs one operation, or answers `None` without trying while Redis is skipped. A failure or a slow answer is
    /// `None` too and makes the store skip Redis for a while.
    pub async fn run<T, F, Fut>(&self, operation: F) -> Option<T>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: Future<Output = RedisResult<T>>,
    {
        if self
            .link()
            .paused_until
            .is_some_and(|until| Instant::now() < until)
        {
            return None;
        }
        self.run_even_if_paused(operation).await
    }

    /// Like `run`, but tries Redis even while it is being skipped after a failure.
    pub async fn run_even_if_paused<T, F, Fut>(&self, operation: F) -> Option<T>
    where
        F: FnOnce(ConnectionManager) -> Fut,
        Fut: Future<Output = RedisResult<T>>,
    {
        match tokio::time::timeout(COMMAND_TIMEOUT, operation(self.connection.clone())).await {
            Ok(Ok(value)) => {
                self.mark_up();
                Some(value)
            }
            Ok(Err(error)) => {
                self.mark_down(&error.to_string());
                None
            }
            Err(_) => {
                self.mark_down(&format!("no answer within {} s", COMMAND_TIMEOUT.as_secs()));
                None
            }
        }
    }

    /// Whether Redis answers a `PING`. Connects when no connection was made yet.
    pub async fn ping(&self) -> bool {
        self.run(|mut connection| async move {
            redis::cmd("PING").query_async::<()>(&mut connection).await
        })
        .await
        .is_some()
    }

    #[cfg(test)]
    pub(crate) fn skip_for_a_while(&self) {
        self.link().paused_until = Some(Instant::now() + PAUSE_AFTER_FAILURE);
    }

    fn link(&self) -> MutexGuard<'_, Link> {
        self.link.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mark_up(&self) {
        let mut link = self.link();
        link.paused_until = None;
        let before = std::mem::replace(&mut link.state, LinkState::Up);
        drop(link);
        match before {
            LinkState::Untried => tracing::info!("Redis connected"),
            LinkState::Down => {
                tracing::info!("Redis answers again, the budget and the cache use it again")
            }
            LinkState::Up => {}
        }
    }

    fn mark_down(&self, reason: &str) {
        let mut link = self.link();
        link.paused_until = Some(Instant::now() + PAUSE_AFTER_FAILURE);
        let before = std::mem::replace(&mut link.state, LinkState::Down);
        drop(link);
        if before != LinkState::Down {
            tracing::warn!(
                "Redis failed, the budget and the cache use memory until it answers again: {reason}"
            );
        }
    }
}
