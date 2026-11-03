use std::{collections::HashSet, sync::Arc};

use axum::{
    body::Bytes,
    http::{HeaderValue, Method, StatusCode},
    response::Response,
};
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::{
    app::AppState,
    auth::Pool,
    cache::{Answer, AnswerCache, Entry, Finisher, LoadFailure, Outcome, Waiting},
    curseforge::{self, UpstreamFailure},
};

const MINUTE_MS: u64 = 60_000;
const HOUR_MS: u64 = 60 * MINUTE_MS;
pub const IDS_PER_CALL: usize = 100;

/// A batch route of CurseForge whose answer is one row per id: `POST /mods` or `POST /mods/files`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Batch {
    Mods,
    Files,
}

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
            _ => None,
        }
    }

    /// The ids of a body that holds only them, in their first order without repeats. `None` for any other body.
    pub fn ids(self, body: &[u8]) -> Option<Vec<u64>> {
        let ids = match self {
            Self::Mods => serde_json::from_slice::<ModIds>(body).ok()?.ids,
            Self::Files => serde_json::from_slice::<FileIds>(body).ok()?.ids,
        };
        let mut seen = HashSet::new();
        let ids: Vec<u64> = ids.into_iter().filter(|id| seen.insert(*id)).collect();
        (!ids.is_empty()).then_some(ids)
    }

    /// The cache key of one id, shared with `GET /mods/{id}` and `GET /mods/{modId}/files/{id}`.
    pub fn key(self, id: u64) -> String {
        match self {
            Self::Mods => format!("mod:{id}"),
            Self::Files => format!("file:{id}"),
        }
    }

    pub fn found_ms(self) -> u64 {
        match self {
            Self::Mods => HOUR_MS,
            Self::Files => 10 * MINUTE_MS,
        }
    }

    pub fn not_found_ms(self) -> u64 {
        match self {
            Self::Mods => 5 * MINUTE_MS,
            Self::Files => 10 * MINUTE_MS,
        }
    }

    fn path(self) -> &'static str {
        match self {
            Self::Mods => "mods",
            Self::Files => "mods/files",
        }
    }

    fn body(self, ids: &[u64]) -> Bytes {
        let field = match self {
            Self::Mods => "modIds",
            Self::Files => "fileIds",
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
    Waiting(Waiting),
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
    ids: Vec<u64>,
    forced: bool,
) -> Response {
    let now = state.clock.now_ms();
    let mut slots = Vec::with_capacity(ids.len());
    let mut mine = Vec::new();
    for &id in &ids {
        let key = batch.key(id);
        let kept = state.cache.get(&key).filter(|entry| usable(entry));
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
        if let Some(finisher) = finisher {
            mine.push(Mine {
                id,
                load_key,
                finisher,
            });
        }
        slots.push((id, kept, Slot::Waiting(waiting)));
    }

    let called = !mine.is_empty();
    if called {
        tokio::spawn(fetch(state.clone(), pool.clone(), batch, mine));
    }

    let mut rows = Vec::with_capacity(slots.len());
    let mut stale = false;
    for (_, kept, slot) in slots {
        let loaded = match slot {
            Slot::Kept(entry) => Ok(entry),
            Slot::Waiting(waiting) => AnswerCache::wait(waiting).await.map_or(
                Err(LoadFailure::Upstream(UpstreamFailure::Unreachable)),
                |outcome| outcome.loaded,
            ),
        };
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
            rows.push(row.clone());
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
    if rows.is_empty() {
        let answer = Answer {
            status: StatusCode::NOT_FOUND,
            content_type: None,
            body: Bytes::new(),
        };
        return curseforge::respond(&answer, cache);
    }
    let mut body = Vec::with_capacity(rows.iter().map(Bytes::len).sum::<usize>() + rows.len() + 11);
    body.extend_from_slice(b"{\"data\":[");
    for (index, row) in rows.iter().enumerate() {
        if index > 0 {
            body.push(b',');
        }
        body.extend_from_slice(row);
    }
    body.extend_from_slice(b"]}");
    let answer = Answer {
        status: StatusCode::OK,
        content_type: Some(HeaderValue::from_static("application/json; charset=utf-8")),
        body: body.into(),
    };
    curseforge::respond(&answer, cache)
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

async fn fetch(state: Arc<AppState>, pool: Pool, batch: Batch, mine: Vec<Mine>) {
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
        Ok(answer) if answer.status == StatusCode::OK => match split(&answer) {
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

fn split(answer: &Answer) -> Option<Vec<(u64, Bytes, Option<u64>)>> {
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

fn kept_row(
    batch: Batch,
    row: &Bytes,
    owner: Option<u64>,
    content_type: &Option<HeaderValue>,
    now: u64,
) -> Entry {
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
    use crate::{curseforge::TIMEOUT, test_support::*};

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

    #[test]
    fn reads_only_a_body_of_nothing_but_ids() {
        assert_eq!(Batch::Mods.ids(br#"{"modIds":[3,1,3]}"#), Some(vec![3, 1]));
        assert_eq!(Batch::Files.ids(br#"{"fileIds":[7]}"#), Some(vec![7]));
        assert_eq!(Batch::Mods.ids(br#"{"modIds":[]}"#), None);
        assert_eq!(
            Batch::Mods.ids(br#"{"modIds":[1],"filterPcOnly":true}"#),
            None
        );
        assert_eq!(Batch::Mods.ids(br#"{"fileIds":[1]}"#), None);
        assert_eq!(Batch::Mods.ids(b"not json"), None);
    }

    #[test]
    fn a_row_is_a_slice_of_the_answer_as_it_came() {
        let body = Bytes::from_static(br#"{ "data" : {"id":50, "modId":5} }"#);
        let (row, owner) = row_of(&body).unwrap();
        assert_eq!(row, r#"{"id":50, "modId":5}"#);
        assert_eq!(owner, Some(5));
    }
}
