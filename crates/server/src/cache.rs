use std::{
    collections::HashMap,
    future::Future,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    http::{HeaderValue, StatusCode},
};
use moka::{Expiry, sync::Cache};
use tokio::sync::OnceCell;

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
/// refused or CurseForge fails. Concurrent loads of one key share one load.
pub struct AnswerCache {
    entries: Cache<String, Arc<Entry>>,
    loading: Mutex<HashMap<String, Arc<OnceCell<Loaded>>>>,
}

/// What a load ended with, handed to every request that waited on it.
pub type Loaded = Result<Arc<Entry>, LoadFailure>;

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

    /// Runs `load` for `key`, or waits for the load of `key` already running and shares its outcome. The flag is
    /// true for the request whose `load` ran.
    pub async fn load<F, Fut>(&self, key: &str, load: F) -> (Loaded, bool)
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Loaded>,
    {
        let cell = self
            .loading()
            .entry(key.to_owned())
            .or_insert_with(|| Arc::new(OnceCell::new()))
            .clone();
        let mut ran = false;
        let loaded = cell
            .get_or_init(|| {
                ran = true;
                load()
            })
            .await
            .clone();
        let mut loading = self.loading();
        if loading
            .get(key)
            .is_some_and(|current| Arc::ptr_eq(current, &cell))
        {
            loading.remove(key);
        }
        (loaded, ran)
    }

    fn loading(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<OnceCell<Loaded>>>> {
        self.loading.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

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

    #[tokio::test]
    async fn concurrent_loads_of_one_key_share_one_load() {
        let cache = Arc::new(AnswerCache::default());
        let loads = Arc::new(AtomicU32::new(0));
        let tasks: Vec<_> = (0..5)
            .map(|_| {
                let (cache, loads) = (cache.clone(), loads.clone());
                tokio::spawn(async move {
                    cache
                        .load("mods/1", || async {
                            loads.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            Ok(entry("one"))
                        })
                        .await
                })
            })
            .collect();

        let mut ran = 0;
        for task in tasks {
            let (loaded, did_run) = task.await.unwrap();
            assert_eq!(loaded.unwrap().answer.body, "one");
            ran += u32::from(did_run);
        }
        assert_eq!(loads.load(Ordering::SeqCst), 1);
        assert_eq!(ran, 1);
    }

    #[tokio::test]
    async fn a_finished_load_runs_again_for_the_next_request() {
        let cache = AnswerCache::default();
        let (_, first) = cache.load("mods/1", || async { Ok(entry("one")) }).await;
        let (loaded, second) = cache.load("mods/1", || async { Ok(entry("two")) }).await;
        assert!(first && second);
        assert_eq!(loaded.unwrap().answer.body, "two");
    }

    #[tokio::test]
    async fn keeps_what_is_inserted() {
        let cache = AnswerCache::default();
        assert!(cache.get("mods/1").is_none());
        cache.insert("mods/1", entry("one"));
        assert_eq!(cache.get("mods/1").unwrap().answer.body, "one");
    }
}
