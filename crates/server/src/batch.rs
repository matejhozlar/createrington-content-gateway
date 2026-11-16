use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    http::{HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::value::RawValue;

use crate::{
    app::AppState,
    auth::Pool,
    cache::{Answer, Entry},
    clock::{HOUR_MS, MINUTE_MS},
    error::ApiError,
    load::{self, PerId},
    passthrough,
    upstream::Source,
};

const MOST_IDS: usize = 1000;

/// A CurseForge batch route answered per id: `POST /mods`, `POST /mods/files` and `POST /fingerprints`.
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
struct DataRows<'a> {
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

    pub fn found_ms(self) -> u64 {
        match self {
            Self::Mods | Self::Fingerprints | Self::MinecraftFingerprints => HOUR_MS,
            Self::Files => 10 * MINUTE_MS,
        }
    }

    fn is_fingerprints(self) -> bool {
        matches!(self, Self::Fingerprints | Self::MinecraftFingerprints)
    }

    fn field(self) -> &'static str {
        match self {
            Self::Mods => "modIds",
            Self::Files => "fileIds",
            Self::Fingerprints | Self::MinecraftFingerprints => "fingerprints",
        }
    }
}

impl PerId for Batch {
    type Id = u64;
    const SOURCE: Source = Source::CurseForge;
    const PER_CALL: usize = 100;
    const KNOWS_NONE_AS_404: bool = true;

    /// The cache key of one id, shared with `GET /mods/{id}` and `GET /mods/{modId}/files/{id}`.
    fn key(self, id: &u64) -> String {
        match self {
            Self::Mods => format!("mod:{id}"),
            Self::Files => format!("file:{id}"),
            Self::Fingerprints => format!("fingerprint:{id}"),
            Self::MinecraftFingerprints => format!("fingerprint:{MINECRAFT}:{id}"),
        }
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
        let ids: Vec<String> = ids.iter().map(u64::to_string).collect();
        format!("{{\"{}\":[{}]}}", self.field(), ids.join(",")).into()
    }

    fn split(self, answer: &Answer, now_ms: u64) -> Option<Vec<(u64, Entry)>> {
        let rows = if self.is_fingerprints() {
            split_matches(answer)?
        } else {
            split_rows(answer)?
        };
        Some(
            rows.into_iter()
                .map(|(id, row, owner)| {
                    let entry = kept_row(self, &row, owner, &answer.content_type, now_ms);
                    (id, entry)
                })
                .collect(),
        )
    }

    fn not_found_ms(self) -> u64 {
        match self {
            Self::Mods => 5 * MINUTE_MS,
            Self::Files | Self::Fingerprints | Self::MinecraftFingerprints => 10 * MINUTE_MS,
        }
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

/// The answer to a batch of ids from the per-id cache: the rows CurseForge knows, joined in its shape.
pub async fn answer(
    state: Arc<AppState>,
    pool: Pool,
    batch: Batch,
    asked: Vec<u64>,
    forced: bool,
) -> Response {
    let Some(ids) = load::distinct(asked.iter().copied(), MOST_IDS) else {
        return ApiError::invalid_body(&format!(
            "a list of at most {MOST_IDS} ids under `{}`",
            batch.field()
        ))
        .into_response();
    };
    let given = match load::rows(&state, &pool, batch, ids, forced).await {
        Ok(given) => given,
        Err(failure) => return passthrough::failed(failure, &pool, Source::CurseForge),
    };
    let (rows, cache) = (given.value, given.cache);
    if batch.is_fingerprints() {
        return passthrough::respond(&Answer::json(fingerprint_answer(&rows, &asked)), cache);
    }
    if rows.is_empty() {
        return passthrough::respond(&Answer::not_found(), cache);
    }
    let mut body =
        Vec::with_capacity(rows.iter().map(|(_, row)| row.len() + 1).sum::<usize>() + 11);
    body.extend_from_slice(b"{\"data\":[");
    join(&mut body, rows.iter().map(|(_, row)| row.as_ref()));
    body.extend_from_slice(b"]}");
    passthrough::respond(&Answer::json(body), cache)
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

fn split_rows(answer: &Answer) -> Option<Vec<(u64, Bytes, Option<u64>)>> {
    let rows: DataRows<'_> = serde_json::from_slice(&answer.body).ok()?;
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
    use std::{collections::HashSet, time::Duration};

    use axum::http::Request;

    use super::*;
    use crate::test_support::*;

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

    fn many(count: u64) -> String {
        let ids: Vec<String> = (1..=count).map(|id| id.to_string()).collect();
        format!(r#"{{"modIds":[{}]}}"#, ids.join(","))
    }

    #[tokio::test]
    async fn asks_for_each_id_once_and_answers_in_the_order_asked() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock);
        let got = post_json(
            &gateway.router,
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
        let gateway = gateway(&mock);
        post_json(
            &gateway.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[1,2,1000]}"#,
        )
        .await;

        let again = post_json(
            &gateway.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[2,1000,1]}"#,
        )
        .await;
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
            gateway.state.curseforge_budget.snapshot().await.days[0].pools[0]
                .counts
                .hits,
            3
        );
    }

    #[tokio::test]
    async fn only_the_ids_not_kept_are_asked_for() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/curseforge/mods/1").await;
        let got = post_json(
            &gateway.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[1,2]}"#,
        )
        .await;
        assert_eq!(
            got.body,
            format!(r#"{{"data":[{},{}]}}"#, mod_row(1), mod_row(2))
        );
        assert_eq!(mock.seen()[1].body, r#"{"modIds":[2]}"#);
    }

    #[tokio::test]
    async fn counts_one_call_per_hundred_ids_asked_for() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock);
        let got = post_json(&gateway.router, "/v1/curseforge/mods", &many(250)).await;
        assert_eq!(got.status, StatusCode::OK);
        let mut sizes: Vec<usize> = mock
            .seen()
            .iter()
            .map(|seen| ids_in(&seen.body).len())
            .collect();
        sizes.sort_unstable();
        assert_eq!(sizes, [50, 100, 100]);
        let snapshot = gateway.state.curseforge_budget.snapshot().await;
        assert_eq!(
            (snapshot.used, snapshot.days[0].pools[0].counts.calls),
            (3, 3)
        );
    }

    #[tokio::test]
    async fn more_than_a_thousand_different_ids_are_refused_before_any_call() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock);
        let got = post_json(&gateway.router, "/v1/curseforge/mods", &many(1001)).await;
        assert_eq!(got.status, StatusCode::BAD_REQUEST);
        assert_eq!(got.json()["code"], "INVALID_BODY");
        assert_eq!(
            got.json()["message"],
            "the body must be a list of at most 1000 ids under `modIds`"
        );
        assert_eq!(mock.calls(), 0);

        let repeated = format!(r#"{{"modIds":[{}1]}}"#, "1,".repeat(1000));
        let got = post_json(&gateway.router, "/v1/curseforge/mods", &repeated).await;
        assert_eq!(got.status, StatusCode::OK);
        assert_eq!(mock.calls(), 1);

        let fingerprints = many(1001).replace("modIds", "fingerprints");
        for route in [
            "/v1/curseforge/fingerprints",
            "/v1/curseforge/fingerprints/432",
        ] {
            let got = post_json(&gateway.router, route, &fingerprints).await;
            assert_eq!(got.status, StatusCode::BAD_REQUEST, "{route}");
            assert_eq!(
                got.json()["message"],
                "the body must be a list of at most 1000 ids under `fingerprints`",
                "{route}"
            );
        }
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_batch_larger_than_a_minute_allows_is_refused_whole() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway_with(
            &mock,
            GatewayWith {
                limit: 2,
                ..GatewayWith::default()
            },
        );
        let got = post_json(&gateway.router, "/v1/curseforge/mods", &many(250)).await;
        assert_eq!(got.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(got.json()["code"], "REQUEST_TOO_LARGE");
        assert_eq!(mock.calls(), 0);
    }

    #[tokio::test]
    async fn a_batch_of_unknown_ids_is_a_kept_404_without_a_body() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock);
        for cache in ["miss", "hit"] {
            let got = post_json(
                &gateway.router,
                "/v1/curseforge/mods",
                r#"{"modIds":[1000,1001]}"#,
            )
            .await;
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
        let gateway = gateway(&mock);
        post_json(
            &gateway.router,
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
        let gateway = gateway(&mock);
        get(&gateway.router, "/v1/curseforge/mods/5/files/50").await;
        let got = post_json(
            &gateway.router,
            "/v1/curseforge/mods/files",
            r#"{"fileIds":[50]}"#,
        )
        .await;
        assert_eq!(got.header("x-cache"), Some("hit"));
        assert_eq!(got.body, format!(r#"{{"data":[{}]}}"#, file_row(50)));
        assert_eq!(mock.calls(), 1);
    }

    #[tokio::test]
    async fn a_batch_joins_a_single_lookup_already_running() {
        let (mock, gate) = Mock::held(curseforge).await;
        let gateway = gateway(&mock);
        let single = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        until("the single lookup to reach CurseForge", || {
            mock.calls() == 1
        })
        .await;
        let batch = {
            let router = gateway.router.clone();
            tokio::spawn(async move {
                post_json(&router, "/v1/curseforge/mods", r#"{"modIds":[1]}"#).await
            })
        };
        until("the batch joining it", || gateway.state.cache.joined() == 1).await;
        gate.open();
        let got = batch.await.unwrap();
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
        let gateway = gateway(&mock);
        post_json(
            &gateway.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[1,2]}"#,
        )
        .await;
        gateway.clock.advance(2 * HOUR_MS);

        let stale = post_json(
            &gateway.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[1,2]}"#,
        )
        .await;
        assert_eq!(
            (stale.status, stale.header("x-cache")),
            (StatusCode::OK, Some("stale"))
        );
        assert_eq!(
            stale.body,
            format!(r#"{{"data":[{},{}]}}"#, mod_row(1), mod_row(2))
        );
        let snapshot = gateway.state.curseforge_budget.snapshot().await;
        let counts = &snapshot.days[0].pools[0].counts;
        assert_eq!((counts.calls, counts.hits), (2, 0));

        let failed = post_json(
            &gateway.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[1,3]}"#,
        )
        .await;
        assert_eq!(failed.status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(failed.body, "unavailable");
    }

    #[tokio::test]
    async fn no_cache_asks_for_every_id_again() {
        let mock = Mock::start(Duration::ZERO, curseforge).await;
        let gateway = gateway(&mock);
        post_json(
            &gateway.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[1,2]}"#,
        )
        .await;
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
    async fn a_joined_answer_without_a_row_fails_the_batch_instead_of_dropping_the_id() {
        let (mock, gate) = Mock::held(|_, _| reply(200, r#"{"error":"not a project"}"#)).await;
        let gateway = gateway(&mock);
        let single = {
            let router = gateway.router.clone();
            tokio::spawn(async move { get(&router, "/v1/curseforge/mods/1").await })
        };
        until("the single lookup to reach CurseForge", || {
            mock.calls() == 1
        })
        .await;
        let batch = {
            let router = gateway.router.clone();
            tokio::spawn(async move {
                post_json(&router, "/v1/curseforge/mods", r#"{"modIds":[1]}"#).await
            })
        };
        until("the batch joining it", || gateway.state.cache.joined() == 1).await;
        gate.open();
        let got = batch.await.unwrap();
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
        let gateway = gateway(&mock);
        let got = post_json(
            &gateway.router,
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
        let gateway = gateway(&mock);
        post_json(
            &gateway.router,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2,150,999]}"#,
        )
        .await;

        let again = post_json(
            &gateway.router,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[999,150,2]}"#,
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
        assert_eq!(again.body, expected(&[999, 150, 2]));
        let more = post_json(
            &gateway.router,
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
        let gateway = gateway(&mock);
        for cache in ["miss", "hit"] {
            let got = post_json(
                &gateway.router,
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
        let gateway = gateway(&mock);
        post_json(
            &gateway.router,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2]}"#,
        )
        .await;
        let other = post_json(
            &gateway.router,
            "/v1/curseforge/fingerprints",
            r#"{"fingerprints":[2]}"#,
        )
        .await;
        assert_eq!(other.header("x-cache"), Some("miss"));
        assert_eq!(other.body, expected(&[2]));
        assert_eq!(mock.seen()[1].uri, "/v1/fingerprints");
        let again = post_json(
            &gateway.router,
            "/v1/curseforge/fingerprints",
            r#"{"fingerprints":[2]}"#,
        )
        .await;
        assert_eq!(again.header("x-cache"), Some("hit"));
    }

    #[tokio::test]
    async fn another_game_passes_through() {
        let mock = Mock::start(Duration::ZERO, fingerprint_api).await;
        let gateway = gateway(&mock);
        for _ in 0..2 {
            post_json(
                &gateway.router,
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
        let gateway = gateway(&mock);
        let got = post_json(
            &gateway.router,
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
        let gateway = gateway(&mock);
        let got = post_json(
            &gateway.router,
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
            let gateway = gateway(&mock);
            let got = post_json(
                &gateway.router,
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
        let gateway = gateway(&mock);
        post_json(
            &gateway.router,
            "/v1/curseforge/fingerprints/432",
            r#"{"fingerprints":[2,150,999]}"#,
        )
        .await;
        gateway.clock.advance(2 * HOUR_MS);

        let stale = post_json(
            &gateway.router,
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
    #[ignore = "needs CONTENT_GATEWAY_TEST_REDIS_URL"]
    async fn answers_kept_in_redis_are_hits_after_a_restart() {
        let redis = TestRedis::required();
        let on_redis = || GatewayWith {
            redis: Some(redis.open()),
            ..GatewayWith::default()
        };
        let mock = Mock::start(Duration::ZERO, curseforge).await;

        let before = gateway_with(&mock, on_redis());
        get(&before.router, "/v1/curseforge/mods/1").await;
        post_json(
            &before.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[2,1000]}"#,
        )
        .await;
        get(&before.router, "/v1/curseforge/mods/3/files/30").await;

        let store = redis.open();
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

        let after = gateway_with(&mock, on_redis());
        let single = get(&after.router, "/v1/curseforge/mods/2").await;
        assert_eq!(single.header("x-cache"), Some("hit"));
        assert_eq!(single.body, format!(r#"{{"data":{}}}"#, mod_row(2)));
        let joined = post_json(
            &after.router,
            "/v1/curseforge/mods",
            r#"{"modIds":[1,2,1000]}"#,
        )
        .await;
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
        redis.forget().await;
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
