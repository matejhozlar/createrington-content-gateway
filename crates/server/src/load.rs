use std::{
    collections::{HashMap, HashSet},
    future::Future,
    hash::Hash,
    sync::Arc,
};

use axum::{
    body::Bytes,
    http::{Method, StatusCode},
};

use crate::{
    app::AppState,
    auth::Pool,
    budget::Budget,
    cache::{
        Answer, AnswerCache, CacheStatus, Entry, Finisher, LoadFailure, Outcome, Waiting, load_key,
    },
    passthrough,
    upstream::{Source, UpstreamFailure},
};

/// What a request was given, and whether it came from the cache (`hit`), from a call (`miss`) or from an old copy
/// that stood in for a failure (`stale`).
pub struct Given<T> {
    pub value: T,
    pub cache: CacheStatus,
    called: bool,
}

impl<T> Given<T> {
    /// Counts the request as a hit of `pool` when it was answered without a call of its own.
    pub fn counted(self, budget: &Budget, pool: &Pool) -> Self {
        if !self.called {
            budget.record_hit(pool);
        }
        self
    }

    fn map<U>(self, value: impl FnOnce(T) -> U) -> Given<U> {
        Given {
            value: value(self.value),
            cache: self.cache,
            called: self.called,
        }
    }
}

struct Won<I> {
    id: I,
    key: String,
    load_key: String,
    finisher: Finisher,
}

impl<I> Won<I> {
    fn finish(self, cache: &AnswerCache, outcome: Outcome) {
        cache.finish(&self.load_key, self.finisher, outcome);
    }
}

enum Slot<V> {
    Kept(V),
    Waiting {
        key: String,
        waiting: Waiting,
        started: bool,
        kept: Option<(V, Arc<Entry>)>,
    },
}

async fn many<I, V, F, Fut>(
    state: &Arc<AppState>,
    ids: Vec<I>,
    key: impl Fn(&I) -> String,
    forced: bool,
    read: fn(&Arc<Entry>) -> Option<V>,
    fetch: F,
) -> Result<Given<Vec<(I, V)>>, LoadFailure>
where
    I: Clone + Send + 'static,
    V: 'static,
    F: FnOnce(Vec<Won<I>>) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let keys: Vec<String> = ids.iter().map(key).collect();
    let found = state.cache.lookup_many(&keys, state.clock.now_ms()).await;
    let now = state.clock.now_ms();
    let mut slots = Vec::with_capacity(ids.len());
    let mut won = Vec::new();
    for ((id, key), found) in ids.into_iter().zip(keys).zip(found) {
        let kept = match found.and_then(|entry| Some((read(&entry)?, entry))) {
            Some((value, entry)) if !forced && entry.is_fresh(now) => {
                slots.push((id, Slot::Kept(value)));
                continue;
            }
            kept => kept,
        };
        let load_key = load_key(key.clone(), forced);
        let (waiting, finisher) = state.cache.join(&load_key);
        let started = finisher.is_some();
        if let Some(finisher) = finisher {
            won.push(Won {
                id: id.clone(),
                key: key.clone(),
                load_key,
                finisher,
            });
        }
        slots.push((
            id,
            Slot::Waiting {
                key,
                waiting,
                started,
                kept,
            },
        ));
    }
    if let Some(first) = won.first() {
        let (key, keys) = (first.key.clone(), won.len());
        spawn_load(run(state.clone(), won, forced, read, fetch), key, keys);
    }

    let mut values = Vec::with_capacity(slots.len());
    let (mut stale, mut called) = (false, false);
    for (id, slot) in slots {
        let value = match slot {
            Slot::Kept(value) => value,
            Slot::Waiting {
                key,
                waiting,
                started,
                kept,
            } => {
                let loaded = match AnswerCache::wait(waiting).await {
                    Some(outcome) => {
                        called |= started && outcome.called;
                        outcome.loaded
                    }
                    None => {
                        tracing::debug!(%key, "a load this request waited on ended without an answer");
                        Err(LoadFailure::Lost)
                    }
                };
                let value = loaded.and_then(|entry| {
                    read(&entry).ok_or(LoadFailure::Upstream(UpstreamFailure::Unreadable))
                });
                match value {
                    Ok(value) => value,
                    Err(failure) => {
                        let now = state.clock.now_ms();
                        match kept.filter(|(_, entry)| entry.can_stand_in(now)) {
                            Some((value, _)) if failure.allows_stale() => {
                                stale = true;
                                value
                            }
                            _ => return Err(failure),
                        }
                    }
                }
            }
        };
        values.push((id, value));
    }
    Ok(Given {
        value: values,
        cache: CacheStatus::of(stale, called),
        called,
    })
}

async fn run<I, V, F, Fut>(
    state: Arc<AppState>,
    won: Vec<Won<I>>,
    forced: bool,
    read: fn(&Arc<Entry>) -> Option<V>,
    fetch: F,
) where
    F: FnOnce(Vec<Won<I>>) -> Fut,
    Fut: Future<Output = ()>,
{
    let now = state.clock.now_ms();
    let mut missing = Vec::with_capacity(won.len());
    for load in won {
        let kept = state
            .cache
            .get(&load.key)
            .filter(|entry| !forced && entry.is_fresh(now) && read(entry).is_some());
        match kept {
            Some(entry) => load.finish(&state.cache, Outcome::not_called(Ok(entry))),
            None => missing.push(load),
        }
    }
    if !missing.is_empty() {
        fetch(missing).await;
    }
}

/// What `read` makes of the entry of `key`: from the cache while the entry is fresh, else from the load of the key,
/// which `fetch` runs unless another request is loading it or the answer was kept meanwhile. An entry `read` makes
/// nothing of counts as not kept, and an older entry stands in for a load that failed in a way that allows it.
pub async fn one<V, Fut>(
    state: &Arc<AppState>,
    key: String,
    forced: bool,
    read: fn(&Arc<Entry>) -> Option<V>,
    fetch: Fut,
) -> Result<Given<V>, LoadFailure>
where
    V: 'static,
    Fut: Future<Output = Outcome> + Send + 'static,
{
    let finishing = state.clone();
    let fetch = move |won: Vec<Won<String>>| async move {
        if let Some(load) = won.into_iter().next() {
            let outcome = fetch.await;
            load.finish(&finishing.cache, outcome);
        }
    };
    let given = many(state, vec![key.clone()], String::clone, forced, read, fetch).await?;
    let Given {
        value,
        cache,
        called,
    } = given;
    match value.into_iter().next() {
        Some((_, value)) => Ok(Given {
            value,
            cache,
            called,
        }),
        None => {
            tracing::warn!(%key, "a load ended without an answer for its own key");
            Err(LoadFailure::Lost)
        }
    }
}

/// A route kept per id: the ids not kept are asked for `PER_CALL` at a time, and each gets an entry of its own.
pub trait PerId: Copy + Send + Sync + 'static {
    type Id: Clone + Eq + Hash + Send + Sync + 'static;
    const SOURCE: Source;
    const PER_CALL: usize;
    /// Whether a 404 says the upstream knows none of the ids, as CurseForge answers, or is a failure like any other.
    const KNOWS_NONE_AS_404: bool;

    fn key(self, id: &Self::Id) -> String;
    fn path(self) -> &'static str;
    fn body(self, ids: &[Self::Id]) -> Bytes;
    /// The entry of every id a 200 answer knows, or `None` when the answer does not read.
    fn split(self, answer: &Answer, now_ms: u64) -> Option<Vec<(Self::Id, Entry)>>;
    fn not_found_ms(self) -> u64;
}

fn row(entry: &Arc<Entry>) -> Option<Option<Bytes>> {
    entry.fits_a_batch().then(|| entry.row.clone())
}

/// The row of each id the upstream knows, in the order of `ids`, each kept and loaded by the rules of `one`, counted
/// against the budget of the route's source.
pub async fn rows<R: PerId>(
    state: &Arc<AppState>,
    pool: &Pool,
    route: R,
    ids: Vec<R::Id>,
    forced: bool,
) -> Result<Given<Vec<(R::Id, Bytes)>>, LoadFailure> {
    let fetch = {
        let (state, pool) = (state.clone(), pool.clone());
        move |won| fetch(state, pool, route, won)
    };
    let given = many(state, ids, |id| route.key(id), forced, row, fetch).await?;
    Ok(given.counted(state.budget_of(R::SOURCE), pool).map(|rows| {
        rows.into_iter()
            .filter_map(|(id, row)| Some((id, row?)))
            .collect()
    }))
}

async fn fetch<R: PerId>(state: Arc<AppState>, pool: Pool, route: R, won: Vec<Won<R::Id>>) {
    let calls = u32::try_from(won.len().div_ceil(R::PER_CALL)).unwrap_or(u32::MAX);
    if let Err(refusal) = state.budget_of(R::SOURCE).admit(&pool, calls).await {
        for load in won {
            let outcome = Outcome::not_called(Err(LoadFailure::Refused(refusal)));
            load.finish(&state.cache, outcome);
        }
        return;
    }
    let mut won = won.into_iter().peekable();
    while won.peek().is_some() {
        let chunk: Vec<Won<R::Id>> = won.by_ref().take(R::PER_CALL).collect();
        let Some(key) = chunk.first().map(|load| load.key.clone()) else {
            break;
        };
        let keys = chunk.len();
        spawn_load(
            fetch_chunk(state.clone(), pool.clone(), route, chunk),
            key,
            keys,
        );
    }
}

/// The items in their first order, each once, or `None` when more than `most` different ones are listed.
pub fn distinct<T: Eq + Hash + Clone>(
    items: impl IntoIterator<Item = T>,
    most: usize,
) -> Option<Vec<T>> {
    let mut seen = HashSet::new();
    let mut kept = Vec::new();
    for item in items {
        if seen.insert(item.clone()) {
            if kept.len() == most {
                return None;
            }
            kept.push(item);
        }
    }
    Some(kept)
}

fn spawn_load(load: impl Future<Output = ()> + Send + 'static, key: String, keys: usize) {
    let running = tokio::spawn(load);
    tokio::spawn(async move {
        if let Err(error) = running.await {
            tracing::warn!(%key, keys, %error, "a load ended without an answer");
        }
    });
}

async fn fetch_chunk<R: PerId>(state: Arc<AppState>, pool: Pool, route: R, chunk: Vec<Won<R::Id>>) {
    let ids: Vec<R::Id> = chunk.iter().map(|load| load.id.clone()).collect();
    let sent = passthrough::send_recorded(
        &state,
        R::SOURCE,
        &pool,
        Method::POST,
        route.path(),
        None,
        route.body(&ids),
    )
    .await;
    let now = state.clock.now_ms();
    let mut found: Result<HashMap<R::Id, Entry>, LoadFailure> = match sent {
        Ok(answer) if answer.status == StatusCode::OK => match route.split(&answer, now) {
            Some(entries) => Ok(entries.into_iter().collect()),
            None => {
                tracing::warn!(
                    upstream = R::SOURCE.name(),
                    %pool,
                    path = route.path(),
                    "the upstream's batch answer cannot be split per id"
                );
                Err(LoadFailure::Upstream(UpstreamFailure::Unreadable))
            }
        },
        Ok(answer) if answer.status == StatusCode::NOT_FOUND && R::KNOWS_NONE_AS_404 => {
            Ok(HashMap::new())
        }
        Ok(answer) => Err(LoadFailure::Answered(answer)),
        Err(failure) => Err(failure),
    };

    for load in chunk {
        let loaded = match &mut found {
            Ok(entries) => {
                let entry = entries
                    .remove(&load.id)
                    .unwrap_or_else(|| Entry::new(Answer::not_found(), now, route.not_found_ms()));
                let entry = Arc::new(entry);
                state.cache.insert(&load.key, entry.clone());
                Ok(entry)
            }
            Err(failure) => Err(failure.clone()),
        };
        load.finish(&state.cache, Outcome::called(loaded));
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::{batch::Batch, test_support::*};

    #[test]
    fn distinct_keeps_the_first_order_and_refuses_more_than_the_most() {
        assert_eq!(distinct([3, 1, 3, 2, 1], 3), Some(vec![3, 1, 2]));
        assert_eq!(distinct([3, 1, 3, 2, 1, 2, 3], 3), Some(vec![3, 1, 2]));
        assert_eq!(distinct([3, 1, 3, 2, 4], 3), None);
        assert_eq!(distinct(Vec::<u64>::new(), 3), Some(vec![]));
    }

    #[tokio::test]
    async fn an_id_kept_meanwhile_is_neither_asked_for_nor_counted() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, r#"{"data":{"id":1}}"#)).await;
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/curseforge/mods/1").await;

        let (waiting, finisher) = gateway.state.cache.join("mod:1");
        let pool = Pool {
            consumer: "app".into(),
            name: "launcher".into(),
        };
        let won = vec![Won {
            id: 1,
            key: "mod:1".to_owned(),
            load_key: "mod:1".to_owned(),
            finisher: finisher.unwrap(),
        }];
        let fetch = {
            let state = gateway.state.clone();
            move |missing| fetch(state, pool, Batch::Mods, missing)
        };
        run(gateway.state.clone(), won, false, row, fetch).await;

        let outcome = AnswerCache::wait(waiting).await.unwrap();
        assert!(!outcome.called);
        assert_eq!(
            outcome.loaded.unwrap().row.clone().unwrap(),
            Bytes::from(r#"{"id":1}"#)
        );
        assert_eq!(mock.calls(), 1);
        assert_eq!(gateway.state.curseforge_budget.snapshot().await.used, 1);
    }

    #[tokio::test]
    async fn a_load_that_dies_is_lost_rather_than_blamed_on_the_upstream() {
        let mock = Mock::start(Duration::ZERO, |_, _| reply(200, "{}")).await;
        let gateway = gateway(&mock);
        let whole = |entry: &Arc<Entry>| Some(entry.clone());
        let died = one(&gateway.state, "mod:1".to_owned(), false, whole, async {
            panic!("the load dies")
        })
        .await;
        assert!(matches!(died, Err(LoadFailure::Lost)));

        let entry = Arc::new(Entry::new(
            Answer::json(Bytes::from_static(b"{}")),
            gateway.state.clock.now_ms(),
            1_000,
        ));
        let next = one(&gateway.state, "mod:1".to_owned(), false, whole, {
            let entry = entry.clone();
            async move { Outcome::called(Ok(entry)) }
        })
        .await;
        assert!(Arc::ptr_eq(&next.ok().unwrap().value, &entry));
    }
}
