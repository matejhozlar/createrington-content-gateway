use std::{
    collections::HashMap,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    http::{HeaderValue, StatusCode},
};
use moka::{Expiry, sync::Cache};
use tokio::sync::watch;

pub const STALE_FOR: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_BYTES: u64 = 256 * 1024 * 1024;

/// One answer CurseForge gave, kept as it came: status, content type and body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: StatusCode,
    pub content_type: Option<HeaderValue>,
    pub body: Bytes,
}

/// A cached answer, when it was stored, and until when it is fresh and after that stale, in milliseconds since the
/// Unix epoch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub answer: Answer,
    pub stored_at_ms: u64,
    pub fresh_until_ms: u64,
    pub stale_until_ms: u64,
}

/// The answers of CurseForge, in memory, at most 256 MiB of them.
///
/// An entry is kept 24 h past its fresh time, for answering when a call is
/// refused or CurseForge fails. Requests for a key whose load is running
/// wait for that load instead of starting one.
pub struct AnswerCache {
    entries: Cache<String, Arc<Entry>>,
    loading: Mutex<HashMap<String, Waiting>>,
}

/// What a load ended with, handed to every request that waited on it.
pub type Loaded = Result<Arc<Entry>, LoadFailure>;

/// A load's outcome, and whether it called CurseForge or found the answer kept meanwhile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub loaded: Loaded,
    pub called: bool,
}

pub type Waiting = watch::Receiver<Option<Outcome>>;
pub type Finisher = watch::Sender<Option<Outcome>>;

/// Why a load gave no answer to keep. `Answered` is an answer that is not kept, such as a 5xx, passed on as it came.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadFailure {
    Answered(Answer),
    Refused(crate::budget::Refusal),
    Upstream(crate::curseforge::UpstreamFailure),
}

struct KeepUntilStale;

impl Expiry<String, Arc<Entry>> for KeepUntilStale {
    fn expire_after_create(
        &self,
        _key: &String,
        entry: &Arc<Entry>,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(
            Duration::from_millis(entry.stale_until_ms.saturating_sub(entry.stored_at_ms))
                + Duration::from_millis(1),
        )
    }

    fn expire_after_update(
        &self,
        key: &String,
        entry: &Arc<Entry>,
        updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        self.expire_after_create(key, entry, updated_at)
    }
}

impl Default for AnswerCache {
    fn default() -> Self {
        Self {
            entries: Cache::builder()
                .max_capacity(MAX_BYTES)
                .weigher(|key: &String, entry: &Arc<Entry>| {
                    u32::try_from(key.len() + entry.answer.body.len()).unwrap_or(u32::MAX)
                })
                .expire_after(KeepUntilStale)
                .build(),
            loading: Mutex::default(),
        }
    }
}

impl AnswerCache {
    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        self.entries.get(key)
    }

    pub fn insert(&self, key: &str, entry: Arc<Entry>) {
        self.entries.insert(key.to_owned(), entry);
    }

    /// Joins the load of `key` that is running, or registers a new one. The sender comes back only for a new load:
    /// whoever holds it runs the load to the end, also when every request waiting on it is gone, and calls `finish`.
    pub fn join(&self, key: &str) -> (Waiting, Option<Finisher>) {
        let mut loading = self.loading();
        if let Some(waiting) = loading
            .get(key)
            .filter(|waiting| waiting.has_changed().is_ok())
        {
            return (waiting.clone(), None);
        }
        let (finisher, waiting) = watch::channel(None);
        loading.insert(key.to_owned(), waiting.clone());
        (waiting, Some(finisher))
    }

    /// Ends the load of `key` with `outcome`: later requests start a new load, and every waiting one gets `outcome`.
    pub fn finish(&self, key: &str, finisher: Finisher, outcome: Outcome) {
        let mut loading = self.loading();
        if loading
            .get(key)
            .is_some_and(|waiting| waiting.same_channel(&finisher.subscribe()))
        {
            loading.remove(key);
        }
        drop(loading);
        finisher.send_replace(Some(outcome));
    }

    /// The outcome of a load, or `None` when it ended without one.
    pub async fn wait(mut waiting: Waiting) -> Option<Outcome> {
        waiting
            .wait_for(Option::is_some)
            .await
            .ok()
            .and_then(|outcome| outcome.clone())
    }

    #[cfg(test)]
    pub(crate) fn loads_running(&self) -> usize {
        self.loading().len()
    }

    fn loading(&self) -> std::sync::MutexGuard<'_, HashMap<String, Waiting>> {
        self.loading.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &'static str) -> Arc<Entry> {
        Arc::new(Entry {
            answer: Answer {
                status: StatusCode::OK,
                content_type: None,
                body: Bytes::from_static(body.as_bytes()),
            },
            stored_at_ms: 0,
            fresh_until_ms: 1_000,
            stale_until_ms: 2_000,
        })
    }

    fn outcome(body: &'static str) -> Outcome {
        Outcome {
            loaded: Ok(entry(body)),
            called: true,
        }
    }

    #[tokio::test]
    async fn requests_for_a_running_load_wait_for_it() {
        let cache = AnswerCache::default();
        let (first, finisher) = cache.join("mods/1");
        let (second, none) = cache.join("mods/1");
        assert!(none.is_none());
        assert_eq!(cache.loads_running(), 1);

        cache.finish("mods/1", finisher.unwrap(), outcome("one"));
        assert_eq!(cache.loads_running(), 0);
        for waiting in [first, second] {
            let got = AnswerCache::wait(waiting).await.unwrap();
            assert_eq!(got.loaded.unwrap().answer.body, "one");
        }
    }

    #[tokio::test]
    async fn a_finished_load_lets_the_next_request_start_one() {
        let cache = AnswerCache::default();
        let (_, finisher) = cache.join("mods/1");
        cache.finish("mods/1", finisher.unwrap(), outcome("one"));
        let (_, finisher) = cache.join("mods/1");
        assert!(finisher.is_some());
    }

    #[tokio::test]
    async fn a_load_that_ended_without_an_outcome_is_replaced() {
        let cache = AnswerCache::default();
        let (waiting, finisher) = cache.join("mods/1");
        drop(finisher);
        assert_eq!(AnswerCache::wait(waiting).await, None);
        let (_, finisher) = cache.join("mods/1");
        assert!(finisher.is_some());
    }

    #[tokio::test]
    async fn keeps_what_is_inserted() {
        let cache = AnswerCache::default();
        assert!(cache.get("mods/1").is_none());
        cache.insert("mods/1", entry("one"));
        assert_eq!(cache.get("mods/1").unwrap().answer.body, "one");
    }
}
