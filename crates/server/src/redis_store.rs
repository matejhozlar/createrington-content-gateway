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

/// The connection to Redis, skipped for five seconds after a failure or a slow answer so callers fall back to memory.
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

    /// Runs one operation, or answers `None` without trying while Redis is skipped.
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

    /// Sends Redis a `PING`, which connects when no connection was made yet.
    pub async fn ping(&self) {
        self.run(|mut connection| async move {
            redis::cmd("PING").query_async::<()>(&mut connection).await
        })
        .await;
    }

    #[cfg(test)]
    pub(crate) fn skip_for_a_while(&self) {
        self.link().paused_until = Some(Instant::now() + PAUSE_AFTER_FAILURE);
    }

    fn link(&self) -> MutexGuard<'_, Link> {
        self.link.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn mark_up(&self) {
        if let Some(news) = self.came_up() {
            tracing::info!("{news}");
        }
    }

    fn came_up(&self) -> Option<&'static str> {
        let mut link = self.link();
        link.paused_until = None;
        match std::mem::replace(&mut link.state, LinkState::Up) {
            LinkState::Untried => Some("Redis connected"),
            LinkState::Down => Some("Redis answers again, the budget and the cache use it again"),
            LinkState::Up => None,
        }
    }

    fn mark_down(&self, reason: &str) {
        if let Some(news) = self.went_down(reason) {
            tracing::warn!("{news}");
        }
    }

    fn went_down(&self, reason: &str) -> Option<String> {
        let mut link = self.link();
        link.paused_until = Some(Instant::now() + PAUSE_AFTER_FAILURE);
        (std::mem::replace(&mut link.state, LinkState::Down) != LinkState::Down).then(|| {
            format!(
                "Redis failed, the budget and the cache use memory until it answers again: {reason}"
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use std::{
        io,
        sync::atomic::{AtomicBool, Ordering},
    };

    use super::*;

    fn store() -> RedisStore {
        RedisStore::open("redis://127.0.0.1:1", "content-gateway-test").unwrap()
    }

    async fn fail(store: &RedisStore) -> Option<()> {
        store
            .run(|_| async { Err(io::Error::other("refused").into()) })
            .await
    }

    async fn answer(store: &RedisStore) -> (Option<u8>, bool) {
        let asked = AtomicBool::new(false);
        let answered = store
            .run(|_| async {
                asked.store(true, Ordering::SeqCst);
                Ok(7)
            })
            .await;
        (answered, asked.load(Ordering::SeqCst))
    }

    #[tokio::test]
    async fn a_failure_skips_redis_until_the_pause_ends() {
        let store = store();
        assert_eq!(answer(&store).await, (Some(7), true));
        assert_eq!(fail(&store).await, None);
        assert_eq!(store.link().state, LinkState::Down);

        assert_eq!(answer(&store).await, (None, false));

        store.link().paused_until = Some(Instant::now() - Duration::from_millis(1));
        assert_eq!(answer(&store).await, (Some(7), true));
        let link = store.link();
        assert_eq!((link.state, link.paused_until), (LinkState::Up, None));
    }

    #[tokio::test]
    async fn says_once_when_redis_fails_and_once_when_it_answers_again() {
        let store = store();
        assert_eq!(store.came_up(), Some("Redis connected"));
        assert_eq!(store.came_up(), None);
        assert_eq!(
            store.went_down("refused").as_deref(),
            Some(
                "Redis failed, the budget and the cache use memory until it answers again: refused"
            )
        );
        assert_eq!(store.went_down("refused again"), None);
        assert_eq!(
            store.came_up(),
            Some("Redis answers again, the budget and the cache use it again")
        );
        assert_eq!(store.came_up(), None);
    }

    #[tokio::test(start_paused = true)]
    async fn an_operation_without_an_answer_within_a_second_counts_as_a_failure() {
        let store = store();
        let answered: Option<()> = store.run_even_if_paused(|_| std::future::pending()).await;
        assert_eq!(answered, None);
        let link = store.link();
        assert_eq!(link.state, LinkState::Down);
        assert!(link.paused_until.is_some());
    }
}
