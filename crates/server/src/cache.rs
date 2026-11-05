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

use crate::redis_store::RedisStore;

pub const STALE_FOR: Duration = Duration::from_secs(24 * 60 * 60);
const MAX_BYTES: u64 = 256 * 1024 * 1024;
const FORMAT: u8 = 1;

/// One answer CurseForge gave, kept as it came: status, content type and body bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub status: StatusCode,
    pub content_type: Option<HeaderValue>,
    pub body: Bytes,
}

/// A cached answer, when it was stored, and until when it is fresh and after that stale, in milliseconds since the
/// Unix epoch. For a project or a file, `row` is the object under `data`, a slice of the body, and `owner` the
/// project a file belongs to. For a fingerprint, the body and `row` are the `exactMatches` items it matched, joined
/// with commas: not JSON on its own, so such an entry is only ever joined into a rebuilt answer, never sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub answer: Answer,
    pub row: Option<Bytes>,
    pub owner: Option<u64>,
    pub stored_at_ms: u64,
    pub fresh_until_ms: u64,
    pub stale_until_ms: u64,
}

impl Entry {
    pub fn new(answer: Answer, now_ms: u64, fresh_ms: u64) -> Self {
        let fresh_until_ms = now_ms + fresh_ms;
        Self {
            answer,
            row: None,
            owner: None,
            stored_at_ms: now_ms,
            fresh_until_ms,
            stale_until_ms: fresh_until_ms + u64::try_from(STALE_FOR.as_millis()).unwrap_or(0),
        }
    }

    pub fn is_fresh(&self, now_ms: u64) -> bool {
        self.fresh_until_ms > now_ms
    }

    pub fn is_stale(&self, now_ms: u64) -> bool {
        self.stale_until_ms > now_ms
    }
}

/// The answers of CurseForge, in memory, at most 256 MiB of them, and in
/// Redis when one is configured, so they survive a restart.
///
/// An entry is kept 24 h past its fresh time, for answering when a call is
/// refused or CurseForge fails. Memory answers first; Redis is asked only
/// when memory has no entry, and an entry found there is kept in memory
/// again. New entries go to Redis in the background, until their stale
/// time. Requests for a key whose load is running wait for that load
/// instead of starting one.
pub struct AnswerCache {
    entries: Cache<String, Kept>,
    loading: Mutex<HashMap<String, Waiting>>,
    redis: Option<Arc<RedisStore>>,
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
    Upstream(crate::upstream::UpstreamFailure),
}

#[derive(Clone)]
struct Kept {
    entry: Arc<Entry>,
    keep_for: Duration,
}

impl Kept {
    fn until_stale(entry: Arc<Entry>, now_ms: u64) -> Self {
        let keep_for = Duration::from_millis(entry.stale_until_ms.saturating_sub(now_ms) + 1);
        Self { entry, keep_for }
    }
}

struct KeepUntilStale;

impl Expiry<String, Kept> for KeepUntilStale {
    fn expire_after_create(
        &self,
        _key: &String,
        kept: &Kept,
        _created_at: Instant,
    ) -> Option<Duration> {
        Some(kept.keep_for)
    }

    fn expire_after_update(
        &self,
        _key: &String,
        kept: &Kept,
        _updated_at: Instant,
        _duration_until_expiry: Option<Duration>,
    ) -> Option<Duration> {
        Some(kept.keep_for)
    }
}

impl Default for AnswerCache {
    fn default() -> Self {
        Self::new(None)
    }
}

impl AnswerCache {
    pub fn new(redis: Option<Arc<RedisStore>>) -> Self {
        Self {
            entries: Cache::builder()
                .max_capacity(MAX_BYTES)
                .weigher(|key: &String, kept: &Kept| {
                    u32::try_from(key.len() + kept.entry.answer.body.len()).unwrap_or(u32::MAX)
                })
                .expire_after(KeepUntilStale)
                .build(),
            loading: Mutex::default(),
            redis,
        }
    }

    /// The entry kept in memory, without asking Redis.
    pub fn get(&self, key: &str) -> Option<Arc<Entry>> {
        self.entries.get(key).map(|kept| kept.entry)
    }

    /// The entry kept in memory, or else the one kept in Redis.
    pub async fn lookup(&self, key: &str, now_ms: u64) -> Option<Arc<Entry>> {
        self.lookup_many(&[key.to_owned()], now_ms)
            .await
            .pop()
            .flatten()
    }

    /// The entries of `keys`, in their order, asking Redis once for every key memory has no entry for. An entry
    /// from Redis whose stale time has passed is none, and one that is kept in memory again stays there only for
    /// the time it has left.
    pub async fn lookup_many(&self, keys: &[String], now_ms: u64) -> Vec<Option<Arc<Entry>>> {
        let mut entries: Vec<Option<Arc<Entry>>> = keys.iter().map(|key| self.get(key)).collect();
        let Some(redis) = &self.redis else {
            return entries;
        };
        let missing: Vec<usize> = (0..keys.len())
            .filter(|index| entries[*index].is_none())
            .collect();
        if missing.is_empty() {
            return entries;
        }
        let stored_keys: Vec<String> = missing
            .iter()
            .map(|index| redis.key(&stored_key(&keys[*index])))
            .collect();
        let Some(stored) = redis
            .run(|mut connection| async move {
                redis::cmd("MGET")
                    .arg(&stored_keys)
                    .query_async::<Vec<Option<Vec<u8>>>>(&mut connection)
                    .await
            })
            .await
        else {
            return entries;
        };
        for (index, bytes) in missing.into_iter().zip(stored) {
            if let Some(entry) = bytes
                .and_then(decode)
                .filter(|entry| entry.is_stale(now_ms))
            {
                let entry = Arc::new(entry);
                self.entries.insert(
                    keys[index].clone(),
                    Kept::until_stale(entry.clone(), now_ms),
                );
                entries[index] = Some(entry);
            }
        }
        entries
    }

    /// Keeps `entry` in memory, and writes it to Redis in the background until its stale time.
    pub fn insert(&self, key: &str, entry: Arc<Entry>) {
        self.entries.insert(
            key.to_owned(),
            Kept::until_stale(entry.clone(), entry.stored_at_ms),
        );
        let Some(redis) = self.redis.clone() else {
            return;
        };
        let lifetime_ms = entry.stale_until_ms.saturating_sub(entry.stored_at_ms);
        if lifetime_ms == 0 {
            return;
        }
        let stored_key = redis.key(&stored_key(key));
        tokio::spawn(async move {
            let bytes = encode(&entry);
            redis
                .run(|mut connection| async move {
                    redis::cmd("SET")
                        .arg(stored_key)
                        .arg(bytes)
                        .arg("PX")
                        .arg(lifetime_ms)
                        .query_async::<()>(&mut connection)
                        .await
                })
                .await;
        });
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

fn stored_key(key: &str) -> String {
    format!("cache:{key}")
}

fn encode(entry: &Entry) -> Vec<u8> {
    let body = &entry.answer.body;
    let content_type = entry
        .answer
        .content_type
        .as_ref()
        .map_or(&[][..], HeaderValue::as_bytes);
    let row = entry.row.as_ref().and_then(|row| {
        let start = (row.as_ptr() as usize).checked_sub(body.as_ptr() as usize)?;
        let end = start.checked_add(row.len())?;
        (end <= body.len()).then_some((u32::try_from(start).ok()?, u32::try_from(end).ok()?))
    });

    let mut bytes = Vec::with_capacity(48 + content_type.len() + body.len());
    bytes.push(FORMAT);
    bytes.extend_from_slice(&entry.answer.status.as_u16().to_be_bytes());
    for ms in [
        entry.stored_at_ms,
        entry.fresh_until_ms,
        entry.stale_until_ms,
    ] {
        bytes.extend_from_slice(&ms.to_be_bytes());
    }
    match entry.owner {
        Some(owner) => {
            bytes.push(1);
            bytes.extend_from_slice(&owner.to_be_bytes());
        }
        None => bytes.push(0),
    }
    match row {
        Some((start, end)) => {
            bytes.push(1);
            bytes.extend_from_slice(&start.to_be_bytes());
            bytes.extend_from_slice(&end.to_be_bytes());
        }
        None => bytes.push(0),
    }
    let content_type_length = u16::try_from(content_type.len()).unwrap_or(0);
    bytes.extend_from_slice(&content_type_length.to_be_bytes());
    bytes.extend_from_slice(&content_type[..usize::from(content_type_length)]);
    bytes.extend_from_slice(body);
    bytes
}

fn decode(bytes: Vec<u8>) -> Option<Entry> {
    let bytes = Bytes::from(bytes);
    let mut at = 0;
    let mut take = |length: usize| {
        let part = bytes.get(at..at + length)?;
        at += length;
        Some(part)
    };
    if take(1)? != [FORMAT] {
        return None;
    }
    let status = StatusCode::from_u16(u16::from_be_bytes(take(2)?.try_into().ok()?)).ok()?;
    let mut ms = || Some(u64::from_be_bytes(take(8)?.try_into().ok()?));
    let (stored_at_ms, fresh_until_ms, stale_until_ms) = (ms()?, ms()?, ms()?);
    let owner = match take(1)? {
        [0] => None,
        [1] => Some(u64::from_be_bytes(take(8)?.try_into().ok()?)),
        _ => return None,
    };
    let row = match take(1)? {
        [0] => None,
        [1] => {
            let start = u32::from_be_bytes(take(4)?.try_into().ok()?);
            let end = u32::from_be_bytes(take(4)?.try_into().ok()?);
            Some((usize::try_from(start).ok()?, usize::try_from(end).ok()?))
        }
        _ => return None,
    };
    let content_type_length = usize::from(u16::from_be_bytes(take(2)?.try_into().ok()?));
    let content_type = match take(content_type_length)? {
        [] => None,
        value => Some(HeaderValue::from_bytes(value).ok()?),
    };
    let body = bytes.slice(at..);
    let row = match row {
        Some((start, end)) if start <= end && end <= body.len() => Some(body.slice(start..end)),
        Some(_) => return None,
        None => None,
    };
    Some(Entry {
        answer: Answer {
            status,
            content_type,
            body,
        },
        row,
        owner,
        stored_at_ms,
        fresh_until_ms,
        stale_until_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(body: &'static str) -> Arc<Entry> {
        Arc::new(Entry::new(
            Answer {
                status: StatusCode::OK,
                content_type: None,
                body: Bytes::from_static(body.as_bytes()),
            },
            0,
            1_000,
        ))
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

    #[test]
    fn an_entry_comes_back_from_its_stored_form_as_it_was() {
        let body = Bytes::from_static(br#"{"data":{"id":50,"modId":5}}"#);
        let row = body.slice(8..body.len() - 1);
        let kept = Entry {
            row: Some(row),
            owner: Some(5),
            ..Entry::new(
                Answer {
                    status: StatusCode::OK,
                    content_type: Some(HeaderValue::from_static("application/json; charset=utf-8")),
                    body,
                },
                1_000,
                60_000,
            )
        };
        let back = decode(encode(&kept)).unwrap();
        assert_eq!(back, kept);
        assert_eq!(back.row.unwrap(), r#"{"id":50,"modId":5}"#);

        let not_found = Entry::new(
            Answer {
                status: StatusCode::NOT_FOUND,
                content_type: None,
                body: Bytes::new(),
            },
            1_000,
            60_000,
        );
        assert_eq!(decode(encode(&not_found)).unwrap(), not_found);
    }

    #[test]
    fn a_stored_form_that_does_not_read_is_no_entry() {
        assert_eq!(decode(Vec::new()), None);
        assert_eq!(decode(vec![FORMAT + 1, 0, 200]), None);
        let mut cut = encode(&entry("one"));
        cut.truncate(10);
        assert_eq!(decode(cut), None);
    }

    #[tokio::test]
    async fn entries_survive_in_redis_for_a_new_cache() {
        let Ok(url) = std::env::var("CONTENT_GATEWAY_TEST_REDIS_URL") else {
            eprintln!("CONTENT_GATEWAY_TEST_REDIS_URL is not set, skipped");
            return;
        };
        let prefix = format!("content-gateway-test:cache:{}", std::process::id());
        let open = || Arc::new(RedisStore::open(&url, prefix.clone()).unwrap());
        let kept = AnswerCache::new(Some(open()));
        kept.insert("mod:1", entry("one"));
        kept.insert("mod:2", entry("two"));

        let store = open();
        let stored = vec![store.key("cache:mod:1"), store.key("cache:mod:2")];
        for _ in 0..100 {
            let keys = stored.clone();
            let written = store
                .run(|mut connection| async move {
                    redis::cmd("EXISTS")
                        .arg(keys)
                        .query_async::<u32>(&mut connection)
                        .await
                })
                .await;
            if written == Some(2) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        let after_restart = AnswerCache::new(Some(store.clone()));
        let found = after_restart
            .lookup_many(
                &["mod:2".to_owned(), "mod:3".to_owned(), "mod:1".to_owned()],
                0,
            )
            .await;
        let past_stale = 1_000 + u64::try_from(STALE_FOR.as_millis()).unwrap();
        assert_eq!(
            AnswerCache::new(Some(store.clone()))
                .lookup("mod:1", past_stale)
                .await,
            None
        );
        let bodies: Vec<Option<Bytes>> = found
            .iter()
            .map(|entry| entry.as_ref().map(|entry| entry.answer.body.clone()))
            .collect();
        assert_eq!(
            bodies,
            [Some(Bytes::from("two")), None, Some(Bytes::from("one"))]
        );
        assert!(after_restart.get("mod:1").is_some());

        store
            .run(|mut connection| async move {
                redis::cmd("DEL")
                    .arg(stored)
                    .query_async::<()>(&mut connection)
                    .await
            })
            .await;
    }

    #[tokio::test]
    async fn keeps_what_is_inserted() {
        let cache = AnswerCache::default();
        assert!(cache.get("mods/1").is_none());
        cache.insert("mods/1", entry("one"));
        assert_eq!(cache.get("mods/1").unwrap().answer.body, "one");
    }
}
