use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use axum::{
    body::Bytes,
    http::{HeaderValue, Method, StatusCode},
    response::Response,
};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::value::RawValue;

use crate::{
    app::AppState,
    auth::Pool,
    cache::{Answer, AnswerCache, Entry, Finisher, LoadFailure, Outcome, Waiting},
    curseforge::{self, UpstreamFailure},
};

use crate::curseforge::{HOUR_MS, MINUTE_MS};

pub const IDS_PER_CALL: usize = 100;

/// A batch route of CurseForge whose answer is made of rows per id: `POST /mods`, `POST /mods/files`, and
/// `POST /fingerprints` with or without Minecraft's game id, where an id is a fingerprint and its rows are the files
/// that have it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    Mods,
    Files,
    Fingerprints,
    MinecraftFingerprints,
}

const MINECRAFT: &str = "432";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModIds {
    #[serde(rename = "modIds")]
    ids: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FileIds {
    #[serde(rename = "fileIds")]
    ids: Vec<u64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FingerprintIds {
    fingerprints: Vec<u64>,
}

#[derive(Deserialize)]
struct Matches<'a> {
    #[serde(borrow)]
    data: MatchLists<'a>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchLists<'a> {
    is_cache_built: bool,
    #[serde(borrow)]
    exact_matches: Vec<&'a RawValue>,
    #[serde(borrow)]
    partial_matches: Vec<&'a RawValue>,
    partial_match_fingerprints: HashMap<String, IgnoredAny>,
    #[serde(borrow, default)]
    unmatched_fingerprints: Option<&'a RawValue>,
}

#[derive(Deserialize)]
struct Match {
    file: MatchedFile,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct MatchedFile {
    file_fingerprint: u64,
}

#[derive(Deserialize)]
struct Rows<'a> {
    #[serde(borrow)]
    data: Vec<&'a RawValue>,
}

#[derive(Deserialize)]
struct Single<'a> {
    #[serde(borrow)]
    data: &'a RawValue,
}

#[derive(Deserialize)]
struct RowIds {
    id: u64,
    #[serde(rename = "modId")]
    mod_id: Option<u64>,
}

impl Batch {
    pub fn of(segments: &[&str]) -> Option<Self> {
        match segments {
            ["mods"] => Some(Self::Mods),
            ["mods", "files"] => Some(Self::Files),
            ["fingerprints"] => Some(Self::Fingerprints),
            ["fingerprints", MINECRAFT] => Some(Self::MinecraftFingerprints),
            _ => None,
        }
    }

    /// The ids of a body that holds only them, as asked. `None` for any other body.
    pub fn ids(self, body: &[u8]) -> Option<Vec<u64>> {
        let ids = match self {
            Self::Mods => serde_json::from_slice::<ModIds>(body).ok()?.ids,
            Self::Files => serde_json::from_slice::<FileIds>(body).ok()?.ids,
            Self::Fingerprints | Self::MinecraftFingerprints => {
                serde_json::from_slice::<FingerprintIds>(body)
                    .ok()?
                    .fingerprints
            }
        };
        (!ids.is_empty()).then_some(ids)
    }

    /// The cache key of one id, shared with `GET /mods/{id}` and `GET /mods/{modId}/files/{id}`.
    pub fn key(self, id: u64) -> String {
        match self {
            Self::Mods => format!("mod:{id}"),
            Self::Files => format!("file:{id}"),
            Self::Fingerprints => format!("fingerprint:{id}"),
            Self::MinecraftFingerprints => format!("fingerprint:{MINECRAFT}:{id}"),
        }
    }

    pub fn found_ms(self) -> u64 {
        match self {
            Self::Mods | Self::Fingerprints | Self::MinecraftFingerprints => HOUR_MS,
            Self::Files => 10 * MINUTE_MS,
        }
    }

    pub fn not_found_ms(self) -> u64 {
        match self {
            Self::Mods => 5 * MINUTE_MS,
            Self::Files | Self::Fingerprints | Self::MinecraftFingerprints => 10 * MINUTE_MS,
        }
    }

    fn is_fingerprints(self) -> bool {
        matches!(self, Self::Fingerprints | Self::MinecraftFingerprints)
    }

    fn path(self) -> &'static str {
        match self {
            Self::Mods => "mods",
            Self::Files => "mods/files",
            Self::Fingerprints => "fingerprints",
            Self::MinecraftFingerprints => "fingerprints/432",
        }
    }

    fn body(self, ids: &[u64]) -> Bytes {
        let field = match self {
            Self::Mods => "modIds",
            Self::Files => "fileIds",
            Self::Fingerprints | Self::MinecraftFingerprints => "fingerprints",
        };
        let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
        format!("{{\"{field}\":[{}]}}", ids.join(",")).into()
    }
}

/// The object under `data` of a single answer, a slice of its body, with its project when it is a file.
pub fn row_of(body: &Bytes) -> Option<(Bytes, Option<u64>)> {
    let single: Single<'_> = serde_json::from_slice(body).ok()?;
    let owner = serde_json::from_str::<RowIds>(single.data.get())
        .ok()
        .and_then(|ids| ids.mod_id);
    Some((body.slice_ref(single.data.get().as_bytes()), owner))
}

enum Slot {
    Kept(Arc<Entry>),
    Waiting { waiting: Waiting, started: bool },
}

struct Mine {
    id: u64,
    load_key: String,
    finisher: Finisher,
}

pub async fn answer(
    state: Arc<AppState>,
    pool: Pool,
    batch: Batch,
    asked: Vec<u64>,
    forced: bool,
) -> Response {
    let mut seen = HashSet::new();
    let ids: Vec<u64> = asked
        .iter()
        .copied()
        .filter(|id| seen.insert(*id))
        .collect();
    let keys: Vec<String> = ids.iter().map(|id| batch.key(*id)).collect();
    let found = state.cache.lookup_many(&keys, state.clock.now_ms()).await;
    let now = state.clock.now_ms();
    let mut slots = Vec::with_capacity(ids.len());
    let mut mine = Vec::new();
    for ((&id, key), kept) in ids.iter().zip(keys).zip(found) {
        let kept = kept.filter(|entry| usable(entry));
        if !forced && let Some(entry) = kept.clone().filter(|entry| entry.is_fresh(now)) {
            slots.push((id, kept, Slot::Kept(entry)));
            continue;
        }
        let load_key = if forced {
            format!("{key} no-cache")
        } else {
            key
        };
        let (waiting, finisher) = state.cache.join(&load_key);
        let started = finisher.is_some();
        if let Some(finisher) = finisher {
            mine.push(Mine {
                id,
                load_key,
                finisher,
            });
        }
        slots.push((id, kept, Slot::Waiting { waiting, started }));
    }

    if !mine.is_empty() {
        tokio::spawn(fetch(state.clone(), pool.clone(), batch, mine, forced));
    }

    let mut rows = Vec::with_capacity(slots.len());
    let mut stale = false;
    let mut called = false;
    for (id, kept, slot) in slots {
        let loaded = match slot {
            Slot::Kept(entry) => Ok(entry),
            Slot::Waiting { waiting, started } => match AnswerCache::wait(waiting).await {
                Some(outcome) => {
                    called |= started && outcome.called;
                    outcome.loaded
                }
                None => Err(LoadFailure::Upstream(UpstreamFailure::Unreachable)),
            },
        };
        let loaded = loaded.and_then(|entry| {
            if usable(&entry) {
                Ok(entry)
            } else {
                Err(LoadFailure::Upstream(UpstreamFailure::Unreadable))
            }
        });
        let entry = match loaded {
            Ok(entry) => entry,
            Err(failure) => {
                let now = state.clock.now_ms();
                match kept.filter(|entry| entry.is_stale(now)) {
                    Some(entry) if failed_upstream(&failure) => {
                        stale = true;
                        entry
                    }
                    _ => return curseforge::failed(failure, &pool),
                }
            }
        };
        if let Some(row) = &entry.row {
            rows.push((id, row.clone()));
        }
    }

    if !called {
        state.budget.record_hit(&pool.consumer, &pool.name);
    }
    let cache = if stale {
        "stale"
    } else if called {
        "miss"
    } else {
        "hit"
    };
    if batch.is_fingerprints() {
        return curseforge::respond(&json(fingerprint_answer(&rows, &asked)), cache);
    }
    if rows.is_empty() {
        let answer = Answer {
            status: StatusCode::NOT_FOUND,
            content_type: None,
            body: Bytes::new(),
        };
        return curseforge::respond(&answer, cache);
    }
    let mut body =
        Vec::with_capacity(rows.iter().map(|(_, row)| row.len() + 1).sum::<usize>() + 11);
    body.extend_from_slice(b"{\"data\":[");
    join(&mut body, rows.iter().map(|(_, row)| row.as_ref()));
    body.extend_from_slice(b"]}");
    curseforge::respond(&json(body), cache)
}

fn json(body: Vec<u8>) -> Answer {
    Answer {
        status: StatusCode::OK,
        content_type: Some(HeaderValue::from_static("application/json; charset=utf-8")),
        body: body.into(),
    }
}

fn join<'a>(body: &mut Vec<u8>, parts: impl Iterator<Item = &'a [u8]>) {
    for (index, part) in parts.enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.extend_from_slice(part);
    }
}

fn fingerprint_answer(rows: &[(u64, Bytes)], asked: &[u64]) -> Vec<u8> {
    let numbers = |ids: &mut dyn Iterator<Item = u64>| {
        ids.map(|id| id.to_string()).collect::<Vec<_>>().join(",")
    };
    let mut body =
        Vec::with_capacity(rows.iter().map(|(_, row)| row.len() + 1).sum::<usize>() + 256);
    body.extend_from_slice(b"{\"data\":{\"isCacheBuilt\":true,\"exactMatches\":[");
    join(&mut body, rows.iter().map(|(_, row)| row.as_ref()));
    body.extend_from_slice(b"],\"exactFingerprints\":[");
    body.extend_from_slice(numbers(&mut rows.iter().map(|(id, _)| *id)).as_bytes());
    body.extend_from_slice(
        b"],\"partialMatches\":[],\"partialMatchFingerprints\":{},\"installedFingerprints\":[",
    );
    body.extend_from_slice(numbers(&mut asked.iter().copied()).as_bytes());
    body.extend_from_slice(b"],\"unmatchedFingerprints\":null}}");
    body
}

fn usable(entry: &Entry) -> bool {
    entry.answer.status == StatusCode::NOT_FOUND || entry.row.is_some()
}

fn failed_upstream(failure: &LoadFailure) -> bool {
    match failure {
        LoadFailure::Answered(answer) => answer.status.is_server_error(),
        LoadFailure::Refused(_) | LoadFailure::Upstream(_) => true,
    }
}

async fn fetch(state: Arc<AppState>, pool: Pool, batch: Batch, mine: Vec<Mine>, forced: bool) {
    let mut missing = Vec::with_capacity(mine.len());
    for load in mine {
        let kept = state
            .cache
            .get(&batch.key(load.id))
            .filter(|entry| !forced && usable(entry) && entry.is_fresh(state.clock.now_ms()));
        match kept {
            Some(entry) => {
                let outcome = Outcome {
                    loaded: Ok(entry),
                    called: false,
                };
                state.cache.finish(&load.load_key, load.finisher, outcome);
            }
            None => missing.push(load),
        }
    }
    if missing.is_empty() {
        return;
    }
    let mine = missing;
    let calls = u32::try_from(mine.len().div_ceil(IDS_PER_CALL)).unwrap_or(u32::MAX);
    if let Err(refusal) = state.budget.admit(&pool.consumer, &pool.name, calls).await {
        for load in mine {
            let outcome = Outcome {
                loaded: Err(LoadFailure::Refused(refusal)),
                called: false,
            };
            state.cache.finish(&load.load_key, load.finisher, outcome);
        }
        return;
    }
    let mut mine = mine.into_iter().peekable();
    while mine.peek().is_some() {
        let chunk: Vec<Mine> = mine.by_ref().take(IDS_PER_CALL).collect();
        tokio::spawn(fetch_chunk(state.clone(), pool.clone(), batch, chunk));
    }
}

async fn fetch_chunk(state: Arc<AppState>, pool: Pool, batch: Batch, chunk: Vec<Mine>) {
    let ids: Vec<u64> = chunk.iter().map(|load| load.id).collect();
    let sent = curseforge::send_recorded(
        &state,
        &pool,
        Method::POST,
        batch.path(),
        None,
        batch.body(&ids),
    )
    .await;
    let found = match sent {
        Ok(answer) if answer.status == StatusCode::OK => match split(batch, &answer) {
            Some(found) => Ok((found, answer.content_type)),
            None => Err(LoadFailure::Upstream(UpstreamFailure::Unreadable)),
        },
        Ok(answer) if answer.status == StatusCode::NOT_FOUND => {
            Ok((Vec::new(), answer.content_type))
        }
        Ok(answer) => Err(LoadFailure::Answered(answer)),
        Err(failure) => Err(failure),
    };

    let now = state.clock.now_ms();
    for load in chunk {
        let loaded = match &found {
            Ok((rows, content_type)) => {
                let entry = match rows.iter().find(|(id, _, _)| *id == load.id) {
                    Some((_, row, owner)) => kept_row(batch, row, *owner, content_type, now),
                    None => Entry::new(
                        Answer {
                            status: StatusCode::NOT_FOUND,
                            content_type: None,
                            body: Bytes::new(),
                        },
                        now,
                        batch.not_found_ms(),
                    ),
                };
                let entry = Arc::new(entry);
                state.cache.insert(&batch.key(load.id), entry.clone());
                Ok(entry)
            }
            Err(failure) => Err(failure.clone()),
        };
        let outcome = Outcome {
            loaded,
            called: true,
        };
        state.cache.finish(&load.load_key, load.finisher, outcome);
    }
}

fn split(batch: Batch, answer: &Answer) -> Option<Vec<(u64, Bytes, Option<u64>)>> {
    if batch.is_fingerprints() {
        return split_matches(answer);
    }
    let rows: Rows<'_> = serde_json::from_slice(&answer.body).ok()?;
    rows.data
        .iter()
        .map(|row| {
            let ids: RowIds = serde_json::from_str(row.get()).ok()?;
            Some((
                ids.id,
                answer.body.slice_ref(row.get().as_bytes()),
                ids.mod_id,
            ))
        })
        .collect()
}

fn split_matches(answer: &Answer) -> Option<Vec<(u64, Bytes, Option<u64>)>> {
    let matches: Matches<'_> = serde_json::from_slice(&answer.body).ok()?;
    let lists = matches.data;
    let plain = lists.is_cache_built
        && lists.partial_matches.is_empty()
        && lists.partial_match_fingerprints.is_empty()
        && lists.unmatched_fingerprints.is_none();
    if !plain {
        return None;
    }
    let mut grouped: Vec<(u64, Vec<&[u8]>)> = Vec::new();
    for raw in lists.exact_matches {
        let fingerprint = serde_json::from_str::<Match>(raw.get())
            .ok()?
            .file
            .file_fingerprint;
        match grouped.iter_mut().find(|(known, _)| *known == fingerprint) {
            Some((_, rows)) => rows.push(raw.get().as_bytes()),
            None => grouped.push((fingerprint, vec![raw.get().as_bytes()])),
        }
    }
    Some(
        grouped
            .into_iter()
            .map(|(fingerprint, rows)| {
                let mut row = Vec::new();
                join(&mut row, rows.into_iter());
                (fingerprint, Bytes::from(row), None)
            })
            .collect(),
    )
}

fn kept_row(
    batch: Batch,
    row: &Bytes,
    owner: Option<u64>,
    content_type: &Option<HeaderValue>,
    now: u64,
) -> Entry {
    if batch.is_fingerprints() {
        return Entry {
            row: Some(row.clone()),
            ..Entry::new(
                Answer {
                    status: StatusCode::OK,
                    content_type: content_type.clone(),
                    body: row.clone(),
                },
                now,
                batch.found_ms(),
            )
        };
    }
    let mut body = Vec::with_capacity(row.len() + 9);
    body.extend_from_slice(b"{\"data\":");
    body.extend_from_slice(row);
    body.push(b'}');
    let body = Bytes::from(body);
    let row = body.slice(8..body.len() - 1);
    Entry {
        row: Some(row),
        owner,
        ..Entry::new(
            Answer {
                status: StatusCode::OK,
                content_type: content_type.clone(),
                body,
            },
            now,
            batch.found_ms(),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use axum::http::Request;

    use super::*;
    use crate::{curseforge::TIMEOUT, redis_store::RedisStore, test_support::*};

    fn ids_in(body: &str) -> Vec<u64> {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        let field = if body.get("modIds").is_some() {
            "modIds"
        } else {
            "fileIds"
        };
        body[field]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_u64().unwrap())
            .collect()
    }

    fn mod_row(id: u64) -> String {
        format!(r#"{{"id":{id},"name":"mod {id}"}}"#)
    }

    fn file_row(id: u64) -> String {
        format!(
            r#"{{"id":{id},"modId":{},"fileName":"f{id}.jar"}}"#,
            id / 10
        )
    }

    fn rows(ids: impl Iterator<Item = u64>, row: fn(u64) -> String) -> Reply {
        let rows: Vec<String> = ids.map(row).collect();
        if rows.is_empty() {
            return reply(404, "");
        }
        reply(200, format!(r#"{{"data":[{}]}}"#, rows.join(",")))
    }

    fn curseforge(_: usize, seen: &Seen) -> Reply {
        let known = |id: &u64| *id < 1_000;
        match seen.uri.as_str() {
            "/v1/mods" => rows(ids_in(&seen.body).into_iter().rev().filter(known), mod_row),
            "/v1/mods/files" => rows(ids_in(&seen.body).into_iter().filter(known), file_row),
            uri => {
                let segments: Vec<&str> = uri.trim_start_matches("/v1/").split('/').collect();
                match segments.as_slice() {
                    ["mods", id] => reply(
                        200,
                        format!(r#"{{"data":{}}}"#, mod_row(id.parse().unwrap())),
                    ),
                    ["mods", project, "files", file]
                        if file.parse::<u64>().unwrap() / 10 == project.parse::<u64>().unwrap() =>
                    {
                        reply(
                            200,
                            format!(r#"{{"data":{}}}"#, file_row(file.parse().unwrap())),
                        )
                    }
                    _ => reply(404, ""),
                }
            }
        }
    }

    async fn post(gateway: &Gateway, uri: &str, body: &str) -> Got {
        send(&gateway.router, Request::post(uri), body).await
    }

    fn many(count: u64) -> String {
        let ids: Vec<String> = (1..=count).map(|id| id.to_string()).collect();
        format!(r#"{{"modIds":[{}]}}"#, ids.join(","))
    }

    #[tokio::test]
    async fn asks_for_each_id_once_and_answers_in_the_order_asked() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let got = post(
            &gateway,
            "/v1/curseforge/mods",
            r#"{"modIds":[2,1,1000,2]}"#,
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.header("x-cache"), Some("miss"));
        assert_eq!(
            got.header("content-type"),
            Some("application/json; charset=utf-8")
        );
        assert_eq!(
            got.body,
            format!(r#"{{"data":[{},{}]}}"#, mod_row(2), mod_row(1))
        );
        assert_eq!(mock.seen()[0].body, r#"{"modIds":[2,1,1000]}"#);
    }

    #[tokio::test]
    async fn kept_rows_answer_later_batches_and_the_single_route() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1,2,1000]}"#).await;

        let again = post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[2,1000,1]}"#).await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(
            again.body,
            format!(r#"{{"data":[{},{}]}}"#, mod_row(2), mod_row(1))
        );

        let single = get(&gateway.router, "/v1/curseforge/mods/1").await;
        assert_eq!(single.header("x-cache"), Some("hit"));
        assert_eq!(single.body, format!(r#"{{"data":{}}}"#, mod_row(1)));
        let unknown = get(&gateway.router, "/v1/curseforge/mods/1000").await;
        assert_eq!(
            (unknown.status, unknown.header("x-cache")),
            (StatusCode::NOT_FOUND, Some("hit"))
        );
        assert_eq!(mock.calls(), 1);
        assert_eq!(
            gateway.state.budget.snapshot().await.days[0].pools[0]
                .counts
                .hits,
            3
        );
    }

    #[tokio::test]
    async fn only_the_ids_not_kept_are_asked_for() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        let got = post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1,2]}"#).await;
        assert_eq!(
            got.body,
            format!(r#"{{"data":[{},{}]}}"#, mod_row(1), mod_row(2))
        );
        assert_eq!(mock.seen()[1].body, r#"{"modIds":[2]}"#);
    }

    #[tokio::test]
    async fn counts_one_call_per_hundred_ids_asked_for() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let got = post(&gateway, "/v1/curseforge/mods", &many(250)).await;
        assert_eq!(got.status, StatusCode::OK);
        let mut sizes: Vec<usize> = mock
            .seen()
            .iter()
            .map(|seen| ids_in(&seen.body).len())
            .collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [50, 100, 100]);
        let snapshot = gateway.state.budget.snapshot().await;
        assert_eq!(
            (snapshot.used, snapshot.days[0].pools[0].counts.calls),
            (3, 3)
        );
    }

    #[tokio::test]
    async fn a_batch_larger_than_a_minute_allows_is_refused_whole() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 2, TIMEOUT);
        let got = post(&gateway, "/v1/curseforge/mods", &many(250)).await;
        assert_eq!(got.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(got.json()["code"], "REQUEST_TOO_LARGE");
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn a_batch_of_unknown_ids_is_a_kept_404_without_a_body() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for cache in ["miss", "hit"] {
            let got = post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1000,1001]}"#).await;
            assert_eq!(
                (got.status, got.header("x-cache")),
                (StatusCode::NOT_FOUND, Some(cache))
            );
            assert!(got.body.is_empty());
        }
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_kept_file_answers_the_single_route_only_under_its_own_project() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        post(
            &gateway,
            "/v1/curseforge/mods/files",
            r#"{"fileIds":[50,51]}"#,
        )
        .await;

        let own = get(&gateway.router, "/v1/curseforge/mods/5/files/50").await;
        assert_eq!(own.header("x-cache"), Some("hit"));
        assert_eq!(own.body, format!(r#"{{"data":{}}}"#, file_row(50)));
        let other = get(&gateway.router, "/v1/curseforge/mods/6/files/50").await;
        assert_eq!(
            (other.status, other.header("x-cache")),
            (StatusCode::NOT_FOUND, Some("miss"))
        );
        assert_eq!(mock.seen()[1].uri, "/v1/mods/6/files/50");
    }

    #[tokio::test]
    async fn a_file_from_the_single_route_answers_a_later_batch() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/mods/5/files/50").await;
        let got = post(&gateway, "/v1/curseforge/mods/files", r#"{"fileIds":[50]}"#).await;
        assert_eq!(got.header("x-cache"), Some("hit"));
        assert_eq!(got.body, format!(r#"{{"data":[{}]}}"#, file_row(50)));
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_batch_joins_a_single_lookup_already_running() {
        let mock = Mock::start(Duration::from_millis(200), curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let single = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1]}"#).await;
        single.await.unwrap();
        assert_eq!(got.header("x-cache"), Some("hit"));
        assert_eq!(got.body, format!(r#"{{"data":[{}]}}"#, mod_row(1)));
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn kept_rows_answer_stale_when_curseforge_fails() {
        let mock = Mock::start(Duration::ZERO, |call, seen| match call {
            0 => curseforge(call, seen),
            _ => reply(503, "unavailable"),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1,2]}"#).await;
        gateway.clock.advance(2 * HOUR_MS);

        let stale = post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1,2]}"#).await;
        assert_eq!(
            (stale.status, stale.header("x-cache")),
            (StatusCode::OK, Some("stale"))
        );
        assert_eq!(
            stale.body,
            format!(r#"{{"data":[{},{}]}}"#, mod_row(1), mod_row(2))
        );

        let failed = post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1,3]}"#).await;
        assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(failed.body, "unavailable");
    }

    #[tokio::test]
    async fn no_cache_asks_for_every_id_again() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1,2]}"#).await;
        let forced = send(
            &gateway.router,
            Request::post("/v1/curseforge/mods").header("cache-control", "no-cache"),
            r#"{"modIds":[1,2]}"#,
        )
        .await;
        assert_eq!(forced.header("x-cache"), Some("miss"));
        assert_eq!(mock.seen()[1].body, r#"{"modIds":[1,2]}"#);
    }

    #[tokio::test]
    async fn an_id_kept_meanwhile_is_neither_asked_for_nor_counted() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        get(&gateway.router, "/v1/curseforge/mods/1").await;

        let (waiting, finisher) = gateway.state.cache.join("mod:1");
        let pool = Pool {
            consumer: "app".into(),
            name: "launcher".into(),
        };
        let mine = vec![Mine {
            id: 1,
            load_key: "mod:1".to_owned(),
            finisher: finisher.unwrap(),
        }];
        fetch(gateway.state.clone(), pool, Batch::Mods, mine, false).await;

        let outcome = AnswerCache::wait(waiting).await.unwrap();
        assert!(!outcome.called);
        assert_eq!(
            outcome.loaded.unwrap().row.clone().unwrap(),
            Bytes::from(mod_row(1))
        );
        assert_eq!(mock.calls(), 1);
        assert_eq!(gateway.state.budget.snapshot().await.used, 1);
    }

    #[tokio::test]
    async fn a_joined_answer_without_a_row_fails_the_batch_instead_of_dropping_the_id() {
        let mock = Mock::start(Duration::from_millis(200), |_, _| {
            reply(200, r#"{"error":"not a project"}"#)
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let single = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let got = post(&gateway, "/v1/curseforge/mods", r#"{"modIds":[1]}"#).await;
        assert_eq!(single.await.unwrap().status, StatusCode::OK);
        assert_eq!(got.status, StatusCode::BAD_GATEWAY);
        assert_eq!(got.json()["code"], "UPSTREAM_UNREADABLE");
        assert_eq!(mock.calls(), 1);
    }

    fn fingerprints_in(body: &str) -> Vec<u64> {
        let body: serde_json::Value = serde_json::from_str(body).unwrap();
        body["fingerprints"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| id.as_u64().unwrap())
            .collect()
    }

    fn file_match(fingerprint: u64, file: u64) -> String {
        format!(
            r#"{{"id":{},"file":{{"id":{file},"fileFingerprint":{fingerprint}}},"latestFiles":[]}}"#,
            fingerprint * 10
        )
    }

    fn matches_for(fingerprint: u64) -> Vec<String> {
        match fingerprint {
            0..=99 => vec![file_match(fingerprint, fingerprint * 100)],
            100..=199 => vec![
                file_match(fingerprint, fingerprint * 100),
                file_match(fingerprint, fingerprint * 100 + 1),
            ],
            _ => Vec::new(),
        }
    }

    fn lists(exact: &[String], exact_fingerprints: &[u64], installed: &[u64]) -> String {
        let numbers = |ids: &[u64]| ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        format!(
            r#"{{"data":{{"isCacheBuilt":true,"exactMatches":[{}],"exactFingerprints":[{}],"partialMatches":[],"partialMatchFingerprints":{{}},"installedFingerprints":[{}],"unmatchedFingerprints":null}}}}"#,
            exact.join(","),
            numbers(exact_fingerprints),
            numbers(installed)
        )
    }

    fn expected(asked: &[u64]) -> String {
        let mut seen = HashSet::new();
        let known: Vec<u64> = asked
            .iter()
            .copied()
            .filter(|id| seen.insert(*id) && !matches_for(*id).is_empty())
            .collect();
        let exact: Vec<String> = known.iter().flat_map(|id| matches_for(*id)).collect();
        lists(&exact, &known, asked)
    }

    fn fingerprint_api(_: usize, seen: &Seen) -> Reply {
        let asked = fingerprints_in(&seen.body);
        reply(200, expected(&asked))
    }

    #[tokio::test]
    async fn rebuilds_a_fingerprint_answer_from_what_each_fingerprint_matched() {
        let mock = Mock::start(Duration::ZERO, fingerprint_api).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let got = post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2,150,2,999]}"#,
        )
        .await;
        assert_eq!(
            (got.status, got.header("x-cache")),
            (StatusCode::OK, Some("miss"))
        );
        assert_eq!(got.body, expected(&[2, 150, 2, 999]));
        assert_eq!(mock.seen()[0].uri, "/v1/fingerprints/432");
        assert_eq!(mock.seen()[0].body, r#"{"fingerprints":[2,150,999]}"#);
    }

    #[tokio::test]
    async fn kept_fingerprints_answer_later_lookups_in_any_order() {
        let mock = Mock::start(Duration::ZERO, fingerprint_api).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2,150,999]}"#,
        )
        .await;

        let again = post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[999,150,2]}"#,
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(again.body, expected(&[999, 150, 2]));
        let more = post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2,3]}"#,
        )
        .await;
        assert_eq!(more.body, expected(&[2, 3]));
        assert_eq!(mock.seen()[1].body, r#"{"fingerprints":[3]}"#);
    }

    #[tokio::test]
    async fn fingerprints_curseforge_knows_none_of_are_an_answer_not_a_404() {
        let mock = Mock::start(Duration::ZERO, fingerprint_api).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for cache in ["miss", "hit"] {
            let got = post(
                &gateway,
                "/v1/curseforge/fingerprints/432",
                r#"{"fingerprints":[998,999]}"#,
            )
            .await;
            assert_eq!(
                (got.status, got.header("x-cache")),
                (StatusCode::OK, Some(cache))
            );
            assert_eq!(got.body, expected(&[998, 999]));
        }
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn the_route_without_a_game_keeps_its_own_fingerprints() {
        let mock = Mock::start(Duration::ZERO, fingerprint_api).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2]}"#,
        )
        .await;
        let other = post(
            &gateway,
            "/v1/curseforge/fingerprints",
            r#"{"fingerprints":[2]}"#,
        )
        .await;
        assert_eq!(other.header("x-cache"), Some("miss"));
        assert_eq!(other.body, expected(&[2]));
        assert_eq!(mock.seen()[1].uri, "/v1/fingerprints");
        let again = post(
            &gateway,
            "/v1/curseforge/fingerprints",
            r#"{"fingerprints":[2]}"#,
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
    }

    #[tokio::test]
    async fn another_game_passes_through() {
        let mock = Mock::start(Duration::ZERO, fingerprint_api).await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        for _ in 0..2 {
            post(
                &gateway,
                "/v1/curseforge/fingerprints/1",
                r#"{"fingerprints":[2]}"#,
            )
            .await;
        }
        assert_eq!(mock.calls(), 2);
    }

    #[tokio::test]
    async fn an_answer_with_partial_matches_is_not_split() {
        let mock = Mock::start(Duration::ZERO, |_, _| {
            reply(
                200,
                r#"{"data":{"isCacheBuilt":true,"exactMatches":[],"exactFingerprints":[],"partialMatches":[{"id":1}],"partialMatchFingerprints":{"1":[2]},"installedFingerprints":[2],"unmatchedFingerprints":null}}"#,
            )
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let got = post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2]}"#,
        )
        .await;
        assert_eq!(got.status, StatusCode::BAD_GATEWAY);
        assert_eq!(got.json()["code"], "UPSTREAM_UNREADABLE");
    }

    #[tokio::test]
    async fn reads_curseforge_lists_in_any_spacing() {
        let mock = Mock::start(Duration::ZERO, |_, _| {
            reply(
                200,
                "{ \"data\" : { \"isCacheBuilt\" : true, \"exactMatches\" : [ ], \"partialMatches\" : [ ],\n  \"partialMatchFingerprints\" : { }, \"unmatchedFingerprints\" : null } }",
            )
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        let got = post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[998]}"#,
        )
        .await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(got.body, expected(&[998]));
    }

    #[tokio::test]
    async fn an_answer_whose_cache_is_not_built_or_that_lists_unmatched_ones_is_not_split() {
        for lists in [
            r#"{"data":{"isCacheBuilt":false,"exactMatches":[],"partialMatches":[],"partialMatchFingerprints":{},"unmatchedFingerprints":null}}"#,
            r#"{"data":{"isCacheBuilt":true,"exactMatches":[],"partialMatches":[],"partialMatchFingerprints":{},"unmatchedFingerprints":[2]}}"#,
        ] {
            let mock = Mock::start(Duration::ZERO, move |_, _| reply(200, lists)).await;
            let gateway = gateway(&mock, 300, TIMEOUT);
            let got = post(
                &gateway,
                "/v1/curseforge/fingerprints/432",
                r#"{"fingerprints":[2]}"#,
            )
            .await;
            assert_eq!(got.status, StatusCode::BAD_GATEWAY, "{lists}");
            assert_eq!(got.json()["code"], "UPSTREAM_UNREADABLE", "{lists}");
        }
    }

    #[tokio::test]
    async fn kept_fingerprints_answer_stale_when_curseforge_fails() {
        let mock = Mock::start(Duration::ZERO, |call, seen| match call {
            0 => fingerprint_api(call, seen),
            _ => reply(500, "boom"),
        })
        .await;
        let gateway = gateway(&mock, 300, TIMEOUT);
        post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2,150,999]}"#,
        )
        .await;
        gateway.clock.advance(2 * HOUR_MS);

        let stale = post(
            &gateway,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[150,2,2]}"#,
        )
        .await;
        assert_eq!(
            (stale.status, stale.header("x-cache")),
            (StatusCode::OK, Some("stale"))
        );
        assert_eq!(stale.body, expected(&[150, 2, 2]));
    }

    #[tokio::test]
    async fn answers_kept_in_redis_are_hits_after_a_restart() {
        let Ok(url) = std::env::var("CONTENT_GATEWAY_TEST_REDIS_URL") else {
            eprintln!("CONTENT_GATEWAY_TEST_REDIS_URL is not set, skipped");
            return;
        };
        let prefix = format!("content-gateway-test:restart:{}", std::process::id());
        let open = || Some(Arc::new(RedisStore::open(&url, prefix.clone()).unwrap()));
        let mock = Mock::start(Duration::ZERO, curseforge).await;

        let before = gateway_on(&mock, 300, TIMEOUT, open());
        get(&before.router, "/v1/curseforge/mods/1").await;
        post(&before, "/v1/curseforge/mods", r#"{"modIds":[2,1000]}"#).await;
        get(&before.router, "/v1/curseforge/mods/3/files/30").await;

        let store = RedisStore::open(&url, prefix.clone()).unwrap();
        let written: Vec<String> = ["mod:1", "mod:2", "mod:1000", "mods/3/files/30?", "file:30"]
            .iter()
            .map(|key| store.key(&format!("cache:{key}")))
            .collect();
        for _ in 0..100 {
            let keys = written.clone();
            let found = store
                .run(|mut connection| async move {
                    redis::cmd("EXISTS")
                        .arg(keys)
                        .query_async::<usize>(&mut connection)
                        .await
                })
                .await;
            if found == Some(written.len()) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let calls = mock.calls();

        let after = gateway_on(&mock, 300, TIMEOUT, open());
        let single = get(&after.router, "/v1/curseforge/mods/2").await;
        assert_eq!(single.header("x-cache"), Some("hit"));
        assert_eq!(single.body, format!(r#"{{"data":{}}}"#, mod_row(2)));
        let joined = post(&after, "/v1/curseforge/mods", r#"{"modIds":[1,2,1000]}"#).await;
        assert_eq!(joined.header("x-cache"), Some("hit"));
        assert_eq!(
            joined.body,
            format!(r#"{{"data":[{},{}]}}"#, mod_row(1), mod_row(2))
        );
        assert_eq!(
            get(&after.router, "/v1/curseforge/mods/3/files/30")
                .await
                .header("x-cache"),
            Some("hit")
        );
        assert_eq!(mock.calls(), calls);

        let keys = vec![store.key("*")];
        let mut stored: Vec<String> = Vec::new();
        for pattern in keys {
            stored.extend(
                store
                    .run(|mut connection| async move {
                        redis::cmd("KEYS")
                            .arg(pattern)
                            .query_async::<Vec<String>>(&mut connection)
                            .await
                    })
                    .await
                    .unwrap_or_default(),
            );
        }
        store
            .run(|mut connection| async move {
                redis::cmd("DEL")
                    .arg(stored)
                    .query_async::<()>(&mut connection)
                    .await
            })
            .await;
    }

    #[test]
    fn reads_only_a_body_of_nothing_but_ids() {
        assert_eq!(
            Batch::Mods.ids(br#"{"modIds":[3,1,3]}"#),
            Some(vec![3, 1, 3])
        );
        assert_eq!(Batch::Files.ids(br#"{"fileIds":[7]}"#), Some(vec![7]));
        assert_eq!(Batch::Mods.ids(br#"{"modIds":[]}"#), None);
        assert_eq!(
            Batch::Mods.ids(br#"{"modIds":[1],"filterPcOnly":true}"#),
            None
        );
        assert_eq!(Batch::Mods.ids(br#"{"fileIds":[1]}"#), None);
        assert_eq!(Batch::Mods.ids(b"not json"), None);
        assert_eq!(
            Batch::MinecraftFingerprints.ids(br#"{"fingerprints":[4,4]}"#),
            Some(vec![4, 4])
        );
    }

    #[test]
    fn a_row_is_a_slice_of_the_answer_as_it_came() {
        let body = Bytes::from_static(br#"{ "data" : {"id":50, "modId":5} }"#);
        let (row, owner) = row_of(&body).unwrap();
        assert_eq!(row, r#"{"id":50, "modId":5}"#);
        assert_eq!(owner, Some(5));
    }
}
